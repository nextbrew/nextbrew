//! `brew vulns`: known vulnerabilities from OSV.dev, looked up as Homebrew
//! does, by a formula's source repository and its release tag.

use std::sync::LazyLock;

use futures::{StreamExt, TryStreamExt, stream};
use nb_core::Error;
use regex::Regex;
use serde_json::{Value, json};

const OSV: &str = "https://api.osv.dev/v1";

/// What a formula is looked up by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    pub version: String,
    pub repo: String,
    pub tag: String,
}

/// An advisory for a formula: its ID and one-line summary.
#[derive(Debug, Clone)]
pub struct Advisory {
    pub id: String,
    pub summary: String,
}

static FORGES: LazyLock<Vec<(&str, Regex)>> = LazyLock::new(|| {
    let two = r"/([^/]+/[^/]+)";
    let gitlab = r"/((?:[^/-][^/]*)(?:/[^/]+)+?)(?:\.git)?(?:/-/|/uploads/|/wikis/|/?$)";
    [
        ("github.com", two),
        ("codeberg.org", two),
        ("gitlab.com", gitlab),
        ("gitlab.gnome.org", gitlab),
        ("gitlab.freedesktop.org", gitlab),
        ("invent.kde.org", gitlab),
    ]
    .into_iter()
    .map(|(host, path)| {
        let re = format!(r"^https?://{}{path}", regex::escape(host));
        (host, Regex::new(&re).expect("forge pattern"))
    })
    .collect()
});

static TAGS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"/archive/refs/tags/([^/]+)\.tar\.gz$",
        r"/archive/refs/tags/([^/]+)\.zip$",
        r"/archive/([^/]+)\.tar\.gz$",
        r"/archive/([^/]+)\.zip$",
        r"/releases/download/([^/]+)/",
        r"/tarball/([^/]+)$",
    ]
    .iter()
    .map(|re| Regex::new(re).expect("tag pattern"))
    .collect()
});

/// The first of `urls` on a known forge, as its repository's URL
/// (Homebrew's `Vulns::Identify.repo_url`).
pub fn repo_url(urls: &[Option<&str>]) -> Option<String> {
    let wayback = Regex::new(r"^https?://web\.archive\.org/web/\d+[a-z_*]*/").ok()?;
    for url in urls.iter().flatten() {
        let url = wayback.replace(url, "");
        for (host, re) in FORGES.iter() {
            if let Some(path) = re.captures(&url).and_then(|c| c.get(1)) {
                let mut path = path.as_str().trim_end_matches(".git").to_string();
                if *host == "github.com" {
                    path = path.to_lowercase();
                }
                return Some(format!("https://{host}/{path}"));
            }
        }
    }
    None
}

/// The release tag a source URL downloads.
pub fn tag(url: &str) -> Option<String> {
    TAGS.iter()
        .find_map(|re| re.captures(url).map(|c| c[1].to_string()))
}

/// The target for a formula's API JSON, at `version` as installed. None
/// when its source is on no forge OSV knows.
pub fn target(json: &Value, version: &str) -> Option<Target> {
    let stable = json["urls"]["stable"]["url"].as_str();
    let head = json["urls"]["head"]["url"].as_str();
    let repo = repo_url(&[stable, head, json["homepage"].as_str()])
        .or_else(|| stable.filter(|url| tag(url).is_some()).map(String::from))
        .or_else(|| head.map(String::from))?;
    let tag = stable
        .and_then(tag)
        .or_else(|| json["urls"]["stable"]["tag"].as_str().map(String::from))
        .or_else(|| json["versions"]["stable"].as_str().map(String::from))?;
    // The tag of the version installed, where it is older than the API's.
    let installed = version.split('_').next().unwrap_or(version);
    let tag = match json["versions"]["stable"].as_str() {
        Some(stable) if !stable.is_empty() && stable != installed => tag.replace(stable, installed),
        _ => tag,
    };
    Some(Target {
        name: json["name"].as_str().unwrap_or_default().to_string(),
        version: version.to_string(),
        repo,
        tag,
    })
}

/// The advisories for each target, in their order.
// ponytail: no `--no-ignore-patches`: an advisory a formula's own patch fixes
// is still reported.
pub async fn scan(http: &reqwest::Client, targets: &[Target]) -> Result<Vec<Vec<Advisory>>, Error> {
    scan_at(http, OSV, targets).await
}

async fn scan_at(
    http: &reqwest::Client,
    osv: &str,
    targets: &[Target],
) -> Result<Vec<Vec<Advisory>>, Error> {
    // IDs page by page; a target with more comes back with a token for the next.
    let mut ids: Vec<Vec<String>> = vec![Vec::new(); targets.len()];
    let mut tokens: Vec<Option<String>> = vec![None; targets.len()];
    let mut pending: Vec<usize> = (0..targets.len()).collect();
    while !pending.is_empty() {
        let queries: Vec<Value> = pending
            .iter()
            .map(|&i| {
                let t = &targets[i];
                let mut query =
                    json!({"package": {"ecosystem": "GIT", "name": t.repo}, "version": t.tag});
                if let Some(token) = &tokens[i] {
                    query["page_token"] = json!(token);
                }
                query
            })
            .collect();
        let batch = post(
            http,
            &format!("{osv}/querybatch"),
            json!({"queries": queries}),
        )
        .await?;
        let results = batch["results"].as_array().cloned().unwrap_or_default();
        let mut next = Vec::new();
        for (&i, result) in pending.iter().zip(&results) {
            let vulns = result["vulns"].as_array().into_iter().flatten();
            ids[i].extend(vulns.filter_map(|v| v["id"].as_str()).map(String::from));
            tokens[i] = result["next_page_token"].as_str().map(String::from);
            if tokens[i].is_some() {
                next.push(i);
            }
        }
        pending = next;
    }

    // The details, a few at a time, in the IDs' order.
    let details: Vec<Advisory> = stream::iter(ids.iter().flatten())
        .map(|id| advisory(http, osv, id))
        .buffered(8)
        .try_collect()
        .await?;
    let mut details = details.into_iter();
    Ok(ids
        .iter()
        .map(|ids| details.by_ref().take(ids.len()).collect())
        .collect())
}

async fn advisory(http: &reqwest::Client, osv: &str, id: &str) -> Result<Advisory, Error> {
    let record = get(http, &format!("{osv}/vulns/{id}")).await?;
    let summary = record["summary"]
        .as_str()
        .or_else(|| record["details"].as_str())
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    Ok(Advisory {
        id: id.to_string(),
        summary,
    })
}

async fn post(http: &reqwest::Client, url: &str, body: Value) -> Result<Value, Error> {
    read(http.post(url).json(&body).send().await, url).await
}

async fn get(http: &reqwest::Client, url: &str) -> Result<Value, Error> {
    read(http.get(url).send().await, url).await
}

async fn read(
    response: Result<reqwest::Response, reqwest::Error>,
    url: &str,
) -> Result<Value, Error> {
    let failed = |e: reqwest::Error| Error::NetworkFailure {
        message: format!("{url}: {e}"),
    };
    response
        .and_then(|r| r.error_for_status())
        .map_err(failed)?
        .json()
        .await
        .map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_repositories_and_tags_as_homebrew_does() {
        let jq = "https://github.com/jqlang/jq/releases/download/jq-1.8.1/jq-1.8.1.tar.gz";
        assert_eq!(
            repo_url(&[Some(jq)]).as_deref(),
            Some("https://github.com/jqlang/jq")
        );
        assert_eq!(tag(jq).as_deref(), Some("jq-1.8.1"));
        let gitlab = "https://gitlab.gnome.org/GNOME/glib.git";
        assert_eq!(
            repo_url(&[None, Some(gitlab)]).as_deref(),
            Some("https://gitlab.gnome.org/GNOME/glib")
        );
        assert_eq!(
            repo_url(&[Some(
                "https://github.com/BurntSushi/ripgrep/archive/refs/tags/14.1.1.tar.gz"
            )])
            .as_deref(),
            Some("https://github.com/burntsushi/ripgrep")
        );
        assert_eq!(
            repo_url(&[Some("https://ftp.gnu.org/gnu/wget/wget-1.25.tar.gz")]),
            None
        );
    }

    #[test]
    fn a_target_falls_back_to_the_version() {
        let json = json!({
            "name": "wget",
            "homepage": "https://www.gnu.org/software/wget/",
            "versions": {"stable": "1.25.0"},
            "urls": {
                "stable": {"url": "https://ftpmirror.gnu.org/gnu/wget/wget-1.25.0.tar.gz"},
                "head": {"url": "https://git.savannah.gnu.org/git/wget.git"}
            }
        });
        let target = target(&json, "1.25.0").unwrap();
        assert_eq!(target.repo, "https://git.savannah.gnu.org/git/wget.git");
        assert_eq!(target.tag, "1.25.0");
        assert_eq!(super::target(&json, "1.24.5_1").unwrap().tag, "1.24.5");
    }

    #[tokio::test]
    async fn reads_every_page_and_every_advisory() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/querybatch"))
            .and(body_string_contains("page_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [{"vulns": [{"id": "B-2"}]}]
            })))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/querybatch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    {"vulns": [{"id": "A-1"}]},
                    {"vulns": [{"id": "B-1"}], "next_page_token": "t"}
                ]
            })))
            .mount(&server)
            .await;
        for id in ["A-1", "B-1", "B-2"] {
            Mock::given(method("GET"))
                .and(path(format!("/vulns/{id}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"summary": format!("{id} summary")})),
                )
                .mount(&server)
                .await;
        }
        let target = |name: &str| Target {
            name: name.into(),
            version: "1".into(),
            repo: format!("https://github.com/x/{name}"),
            tag: "1".into(),
        };
        let found = scan_at(
            &reqwest::Client::new(),
            &server.uri(),
            &[target("a"), target("b")],
        )
        .await
        .unwrap();
        let ids: Vec<Vec<&str>> = found
            .iter()
            .map(|a| a.iter().map(|a| a.id.as_str()).collect())
            .collect();
        assert_eq!(ids, [vec!["A-1"], vec!["B-1", "B-2"]]);
        assert_eq!(found[1][1].summary, "B-2 summary");
    }
}
