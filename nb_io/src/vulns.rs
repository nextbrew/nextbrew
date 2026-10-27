//! `brew vulns`: known vulnerabilities from OSV.dev, looked up as Homebrew
//! does, by a formula's source repository and its release tag.

use std::sync::LazyLock;

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
// ponytail: one page of results per target, and no `--no-ignore-patches`:
// an advisory a formula's own patch fixes is still reported.
pub async fn scan(http: &reqwest::Client, targets: &[Target]) -> Result<Vec<Vec<Advisory>>, Error> {
    let queries: Vec<Value> = targets
        .iter()
        .map(|t| json!({"package": {"ecosystem": "GIT", "name": t.repo}, "version": t.tag}))
        .collect();
    let batch: Value = post(
        http,
        &format!("{OSV}/querybatch"),
        json!({"queries": queries}),
    )
    .await?;
    let mut found = Vec::with_capacity(targets.len());
    for result in batch["results"].as_array().into_iter().flatten() {
        let mut advisories = Vec::new();
        for vuln in result["vulns"].as_array().into_iter().flatten() {
            let id = vuln["id"].as_str().unwrap_or_default().to_string();
            let record = get(http, &format!("{OSV}/vulns/{id}")).await?;
            let summary = record["summary"]
                .as_str()
                .or_else(|| record["details"].as_str())
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            advisories.push(Advisory { id, summary });
        }
        found.push(advisories);
    }
    Ok(found)
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
}
