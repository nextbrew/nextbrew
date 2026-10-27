//! `brew extract`: a formula as homebrew/core had it at an older version.

use nb_core::Error;
use regex::Regex;
use serde_json::Value;

const CORE: &str = "https://api.github.com/repos/Homebrew/homebrew-core";
const RAW: &str = "https://raw.githubusercontent.com/Homebrew/homebrew-core";

/// The commit and the Ruby source of `name` at `version`, renamed to
/// `versioned`, from `path` in homebrew/core (`ruby_source_path`) or where
/// it was before the formulae moved into letter directories in 2023.
pub async fn formula_at(
    http: &reqwest::Client,
    name: &str,
    path: &str,
    version: &str,
    versioned: &str,
) -> Result<(String, String), Error> {
    for path in [path.to_string(), format!("Formula/{name}.rb")] {
        if let Some(sha) = version_commit(http, &path, name, version).await? {
            let source = get(http, &format!("{RAW}/{sha}/{path}"))
                .await?
                .text()
                .await
                .map_err(network)?;
            return Ok((sha, extract(&source, versioned)));
        }
    }
    Err(Error::InvalidArgument {
        message: format!("no version {version} of {name} in homebrew/core's history"),
    })
}

/// The newest commit that made `path` `name version`, by homebrew-core's
/// commit titles (`jq 1.7.1`).
// ponytail: 1,000 commits of a formula's history at most, and titles only;
// a version only ever reached by a revision bump or a rename is not found.
async fn version_commit(
    http: &reqwest::Client,
    path: &str,
    name: &str,
    version: &str,
) -> Result<Option<String>, Error> {
    let title = Regex::new(&format!(
        r"^{}:? (?:update to )?{}(?:\s|$|\))",
        regex::escape(name),
        regex::escape(version)
    ))
    .expect("title pattern");
    for page in 1..=10 {
        let url = format!("{CORE}/commits?path={path}&per_page=100&page={page}");
        let commits: Vec<Value> = get(http, &url).await?.json().await.map_err(network)?;
        if let Some(commit) = commits
            .iter()
            .find(|c| title.is_match(c["commit"]["message"].as_str().unwrap_or_default()))
        {
            return Ok(commit["sha"].as_str().map(String::from));
        }
        if commits.len() < 100 {
            break;
        }
    }
    Ok(None)
}

/// The formula as `versioned`: its class renamed as Homebrew names it
/// (`jq@1.7.1` is `JqAT171`) and the bottles, which are the old name's,
/// left out.
fn extract(source: &str, versioned: &str) -> String {
    let class = Regex::new(r"(?m)^class \S+ < Formula").expect("class pattern");
    let bottle = Regex::new(r"(?ms)^  bottle do\n.*?^  end\n\n?").expect("bottle pattern");
    let renamed = class.replace(source, format!("class {} < Formula", class_name(versioned)));
    bottle.replace(&renamed, "").into_owned()
}

/// `Formulary.class_s`.
fn class_name(name: &str) -> String {
    let mut class = String::new();
    let mut upper = true;
    for c in name.chars() {
        match c {
            '-' | '_' | '.' => upper = true,
            '+' => class.push('x'),
            '@' => class.push_str("AT"),
            c if upper => {
                class.push(c.to_ascii_uppercase());
                upper = false;
            }
            c => class.push(c),
        }
    }
    class
}

async fn get(http: &reqwest::Client, url: &str) -> Result<reqwest::Response, Error> {
    let token =
        std::env::var("HOMEBREW_GITHUB_API_TOKEN").or_else(|_| std::env::var("GITHUB_TOKEN"));
    // GitHub answers a file's long history with a 500 now and then.
    let mut tries = 0;
    loop {
        let mut request = http.get(url);
        if let Ok(token) = &token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(network)?;
        tries += 1;
        if response.status().is_server_error() && tries < 3 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            continue;
        }
        return response.error_for_status().map_err(network);
    }
}

fn network(e: reqwest::Error) -> Error {
    Error::NetworkFailure {
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_class_and_drops_the_bottles() {
        assert_eq!(class_name("jq@1.7.1"), "JqAT171");
        assert_eq!(class_name("gtk+3@3.24"), "Gtkx3AT324");
        let source = "class Jq < Formula\n  url \"x\"\n\n  bottle do\n    sha256 \"y\"\n  end\n\n  depends_on \"z\"\nend\n";
        assert_eq!(
            extract(source, "jq@1.7.1"),
            "class JqAT171 < Formula\n  url \"x\"\n\n  depends_on \"z\"\nend\n"
        );
    }
}
