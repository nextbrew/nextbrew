//! Third-party taps: GitHub repositories of formula and cask files, which
//! Homebrew's JSON API does not carry. nextbrew keeps them under
//! `<prefix>/taps/<user>/homebrew-<repo>` as Homebrew keeps them under
//! `Library/Taps`, and reads their Ruby files without running Ruby (see
//! [`ruby`]).
//!
//! A tap on GitHub is fetched as a tarball of its default branch, so `git`
//! (which needs the Command Line Tools on macOS) is only needed for a tap
//! with another remote.

pub mod ruby;
pub mod trust;
pub mod version;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nb_core::Error;
use serde::{Deserialize, Serialize};

/// Homebrew's `DEPRECATED_OFFICIAL_TAPS`.
const DEPRECATED_OFFICIAL_TAPS: &[&str] = &[
    "aliases",
    "apache",
    "binary",
    "bundle",
    "cask-drivers",
    "cask-eid",
    "cask-fonts",
    "cask-versions",
    "command-not-found",
    "completions",
    "devel-only",
    "dupes",
    "emacs",
    "formula-analytics",
    "fuse",
    "games",
    "gui",
    "head-only",
    "linux-fonts",
    "livecheck",
    "nginx",
    "php",
    "portable-ruby",
    "python",
    "science",
    "services",
    "test-bot",
    "tex",
    "versions",
    "x11",
];

/// A tap's name, `user/repo`, as `Tap.fetch` reads it: lowercase, without
/// the `homebrew-` prefix of the repository.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TapName {
    pub user: String,
    pub repo: String,
}

impl TapName {
    pub fn parse(name: &str) -> Result<Self, Error> {
        let invalid = || Error::InvalidArgument {
            message: format!("Invalid tap name: '{name}'"),
        };
        let (user, repo) = name.split_once('/').ok_or_else(invalid)?;
        if user.is_empty() || repo.is_empty() || repo.contains('/') {
            return Err(invalid());
        }
        let repo = repo.to_lowercase();
        let repo = repo
            .strip_prefix("homebrew-")
            .or_else(|| repo.strip_prefix("linuxbrew-"))
            .unwrap_or(&repo)
            .to_string();
        let mut user = user.to_lowercase();
        if user == "linuxbrew" {
            user = "homebrew".to_string();
        }
        Ok(Self { user, repo })
    }

    /// The tap of a fully-qualified `user/repo/name`, and the name.
    pub fn split_full_name(full_name: &str) -> Option<(Self, String)> {
        let mut parts = full_name.splitn(3, '/');
        let (user, repo, name) = (parts.next()?, parts.next()?, parts.next()?);
        if name.is_empty() || name.contains('/') {
            return None;
        }
        Some((
            Self::parse(&format!("{user}/{repo}")).ok()?,
            name.to_string(),
        ))
    }

    pub fn name(&self) -> String {
        format!("{}/{}", self.user, self.repo)
    }

    pub fn is_official(&self) -> bool {
        self.user == "homebrew"
    }

    /// homebrew/core and homebrew/cask, which nextbrew reads from the API.
    pub fn is_core(&self) -> bool {
        self.is_official() && matches!(self.repo.as_str(), "core" | "homebrew" | "cask")
    }

    pub fn default_remote(&self) -> String {
        format!("https://github.com/{}/homebrew-{}", self.user, self.repo)
    }

    pub fn path(&self, prefix: &Path) -> PathBuf {
        prefix
            .join("taps")
            .join(&self.user)
            .join(format!("homebrew-{}", self.repo))
    }
}

impl std::fmt::Display for TapName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.user, self.repo)
    }
}

/// What nextbrew records about an installed tap.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TapState {
    pub remote: String,
    /// The commit the files are from, when known.
    #[serde(default)]
    pub commit: Option<String>,
}

const STATE_FILE: &str = ".nextbrew-tap.json";

/// The formulae, casks and commands in a tap, and its size, for `Tapped
/// ...` and `Untapped ...`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TapContents {
    pub formulae: usize,
    pub casks: usize,
    pub commands: usize,
    pub files: usize,
    pub bytes: u64,
}

impl TapContents {
    /// `Tap#contents` and `Pathname#abv`: `2 casks and 5 formulae (123
    /// files, 456.7KB)`.
    pub fn summary(&self) -> String {
        let plural =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut parts = Vec::new();
        if self.commands > 0 {
            parts.push(plural(self.commands, "command", "commands"));
        }
        if self.casks > 0 {
            parts.push(plural(self.casks, "cask", "casks"));
        }
        if self.formulae > 0 {
            parts.push(plural(self.formulae, "formula", "formulae"));
        }
        let contents = match parts.as_slice() {
            [] => String::new(),
            [one] => format!("{one} "),
            [init @ .., last] => format!("{} and {last} ", init.join(", ")),
        };
        let files = if self.files > 1 {
            format!("{} files, ", self.files)
        } else {
            String::new()
        };
        format!("{contents}({files}{})", disk_usage(self.bytes))
    }
}

/// `Formatter.disk_usage_readable`: 1000-based, one decimal, no space.
fn disk_usage(bytes: u64) -> String {
    let (size, unit) = if bytes >= 1_000_000_000 {
        (bytes as f64 / 1e9, "GB")
    } else if bytes >= 1_000_000 {
        (bytes as f64 / 1e6, "MB")
    } else if bytes >= 1_000 {
        (bytes as f64 / 1e3, "KB")
    } else {
        return format!("{bytes}B");
    };
    let rounded = (size * 10.0).round() / 10.0;
    if rounded.fract() == 0.0 {
        format!("{rounded:.0}{unit}")
    } else {
        format!("{rounded:.1}{unit}")
    }
}

/// The installed taps of a prefix.
#[derive(Debug, Clone)]
pub struct Taps {
    prefix: PathBuf,
}

impl Taps {
    pub fn new(prefix: &Path) -> Self {
        Self {
            prefix: prefix.to_path_buf(),
        }
    }

    pub fn path(&self, tap: &TapName) -> PathBuf {
        tap.path(&self.prefix)
    }

    pub fn is_installed(&self, tap: &TapName) -> bool {
        self.path(tap).is_dir()
    }

    /// Every installed tap, by name.
    pub fn installed(&self) -> Vec<TapName> {
        let mut taps = Vec::new();
        let Ok(users) = std::fs::read_dir(self.prefix.join("taps")) else {
            return taps;
        };
        for user in users.filter_map(Result::ok) {
            let Ok(repos) = std::fs::read_dir(user.path()) else {
                continue;
            };
            for repo in repos.filter_map(Result::ok) {
                if !repo.path().is_dir() {
                    continue;
                }
                let name = format!(
                    "{}/{}",
                    user.file_name().to_string_lossy(),
                    repo.file_name().to_string_lossy()
                );
                if let Ok(tap) = TapName::parse(&name) {
                    taps.push(tap);
                }
            }
        }
        taps.sort();
        taps
    }

    pub fn state(&self, tap: &TapName) -> Option<TapState> {
        let json = std::fs::read(self.path(tap).join(STATE_FILE)).ok()?;
        serde_json::from_slice(&json).ok()
    }

    /// The checks `Tap#install` makes before fetching anything.
    pub fn check_tappable(tap: &TapName, force: bool) -> Result<(), Error> {
        if tap.is_official() && DEPRECATED_OFFICIAL_TAPS.contains(&tap.repo.as_str()) {
            return Err(Error::InvalidArgument {
                message: format!(
                    "{tap} was deprecated. This tap is now empty and all its contents were either deleted or migrated."
                ),
            });
        }
        if tap.user == "caskroom" {
            let target = if tap.repo == "cask" {
                "homebrew/cask".to_string()
            } else {
                format!("homebrew/cask-{}", tap.repo)
            };
            return Err(Error::InvalidArgument {
                message: format!("{tap} was moved. Tap {target} instead."),
            });
        }
        if tap.is_core() && !force {
            return Err(Error::InvalidArgument {
                message: format!(
                    "Tapping {tap} is no longer typically necessary.\nAdd --force if you are sure you need it for contributing to Homebrew."
                ),
            });
        }
        Ok(())
    }

    /// Fetch `tap` into its directory: GitHub's tarball of the default
    /// branch for a GitHub remote, otherwise `git clone`.
    pub async fn install(
        &self,
        tap: &TapName,
        remote: Option<&str>,
        client: &reqwest::Client,
    ) -> Result<TapContents, Error> {
        let remote = remote
            .map(str::to_string)
            .unwrap_or_else(|| tap.default_remote());
        let dest = self.path(tap);
        let parent = dest.parent().unwrap_or(&self.prefix).to_path_buf();
        std::fs::create_dir_all(&parent)
            .map_err(Error::file("failed to create the taps directory"))?;
        let staging = parent.join(format!(".{}.tmp", tap.repo));
        let _ = std::fs::remove_dir_all(&staging);
        let fetched = match github_repo(&remote) {
            Some(repo) => fetch_github(client, &repo, &staging).await,
            None => clone_git(&remote, &staging),
        };
        let commit = match fetched {
            Ok(commit) => commit,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(e);
            }
        };
        let state = TapState { remote, commit };
        std::fs::write(
            staging.join(STATE_FILE),
            serde_json::to_vec(&state).unwrap_or_default(),
        )
        .map_err(Error::file("failed to record the tap"))?;
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::rename(&staging, &dest).map_err(Error::file("failed to install the tap"))?;
        Ok(self.contents(tap))
    }

    /// Fetch the tap again if its default branch moved. Returns whether it
    /// changed.
    pub async fn update(&self, tap: &TapName, client: &reqwest::Client) -> Result<bool, Error> {
        let state = self.state(tap).unwrap_or_default();
        let remote = if state.remote.is_empty() {
            tap.default_remote()
        } else {
            state.remote.clone()
        };
        if let Some(repo) = github_repo(&remote)
            && let Some(current) = &state.commit
            && github_head(client, &repo).await?.as_deref() == Some(current.as_str())
        {
            return Ok(false);
        }
        self.install(tap, Some(&remote), client).await?;
        Ok(self.state(tap).and_then(|s| s.commit) != state.commit || state.commit.is_none())
    }

    pub fn uninstall(&self, tap: &TapName) -> Result<TapContents, Error> {
        let contents = self.contents(tap);
        let dir = self.path(tap);
        std::fs::remove_dir_all(&dir).map_err(Error::file("failed to remove the tap"))?;
        if let Some(parent) = dir.parent() {
            let _ = std::fs::remove_dir(parent);
        }
        Ok(contents)
    }

    pub fn contents(&self, tap: &TapName) -> TapContents {
        let dir = self.path(tap);
        let mut contents = TapContents {
            formulae: self.formula_files(tap).len(),
            casks: self.cask_files(tap).len(),
            commands: std::fs::read_dir(dir.join("cmd"))
                .map(|d| d.filter_map(Result::ok).count())
                .unwrap_or(0),
            ..TapContents::default()
        };
        for entry in walkdir(&dir) {
            if entry.file_name().is_some_and(|n| n == STATE_FILE) {
                continue;
            }
            if let Ok(meta) = std::fs::symlink_metadata(&entry)
                && meta.is_file()
            {
                contents.files += 1;
                contents.bytes += meta.len();
            }
        }
        contents
    }

    /// `Tap#formula_dir`: `Formula/`, `HomebrewFormula/`, or the tap
    /// itself, whichever exists first.
    fn formula_dir(&self, tap: &TapName) -> Option<(PathBuf, bool)> {
        let dir = self.path(tap);
        for sub in ["Formula", "HomebrewFormula"] {
            if dir.join(sub).is_dir() {
                return Some((dir.join(sub), true));
            }
        }
        dir.is_dir().then_some((dir, false))
    }

    /// `Tap#formula_files_by_name`: each formula file by its name, the
    /// longer path winning for the same name.
    pub fn formula_files(&self, tap: &TapName) -> BTreeMap<String, PathBuf> {
        let Some((dir, recursive)) = self.formula_dir(tap) else {
            return BTreeMap::new();
        };
        let files = if recursive {
            walkdir(&dir)
        } else {
            std::fs::read_dir(&dir)
                .map(|d| d.filter_map(Result::ok).map(|e| e.path()).collect())
                .unwrap_or_default()
        };
        by_name(files)
    }

    /// `Tap#cask_files_by_name`.
    pub fn cask_files(&self, tap: &TapName) -> BTreeMap<String, PathBuf> {
        by_name(walkdir(&self.path(tap).join("Casks")))
    }

    /// `Tap#alias_table`: alias name to formula name.
    pub fn aliases(&self, tap: &TapName) -> BTreeMap<String, String> {
        let dir = self.path(tap).join("Aliases");
        let mut aliases = BTreeMap::new();
        for entry in std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            let Ok(target) = std::fs::canonicalize(&path) else {
                continue;
            };
            if let (Some(alias), Some(stem)) = (
                path.file_name().map(|n| n.to_string_lossy().into_owned()),
                target.file_stem().map(|n| n.to_string_lossy().into_owned()),
            ) {
                aliases.insert(alias, stem);
            }
        }
        aliases
    }

    fn json_map(&self, tap: &TapName, file: &str) -> BTreeMap<String, String> {
        std::fs::read(self.path(tap).join(file))
            .ok()
            .and_then(|json| serde_json::from_slice(&json).ok())
            .unwrap_or_default()
    }

    /// The formula file for `name` in `tap`, following the tap's aliases,
    /// then its renames, then its migrations (to another name in the same
    /// tap), as `Formulary` does. Returns the name it resolved to.
    pub fn find_formula(&self, tap: &TapName, name: &str) -> Option<(String, PathBuf)> {
        let files = self.formula_files(tap);
        let name = name.to_lowercase();
        let mut candidates = vec![name.clone()];
        if let Some(target) = self.aliases(tap).get(&name) {
            candidates.push(target.clone());
        }
        if let Some(target) = self.json_map(tap, "formula_renames.json").get(&name) {
            candidates.push(target.clone());
        }
        if let Some(target) = self.json_map(tap, "tap_migrations.json").get(&name)
            && !target.contains('/')
        {
            candidates.push(target.clone());
        }
        candidates
            .into_iter()
            .find_map(|candidate| files.get(&candidate).map(|f| (candidate, f.clone())))
    }

    /// The cask file for `token` in `tap`, following its renames.
    pub fn find_cask(&self, tap: &TapName, token: &str) -> Option<(String, PathBuf)> {
        let files = self.cask_files(tap);
        let token = token.to_lowercase();
        let mut candidates = vec![token.clone()];
        if let Some(target) = self.json_map(tap, "cask_renames.json").get(&token) {
            candidates.push(target.clone());
        }
        candidates
            .into_iter()
            .find_map(|candidate| files.get(&candidate).map(|f| (candidate, f.clone())))
    }
}

fn by_name(files: Vec<PathBuf>) -> BTreeMap<String, PathBuf> {
    let mut map: BTreeMap<String, PathBuf> = BTreeMap::new();
    for file in files {
        if file.extension().is_none_or(|e| e != "rb") || !file.is_file() {
            continue;
        }
        let Some(name) = file.file_stem().map(|s| s.to_string_lossy().to_lowercase()) else {
            continue;
        };
        let longer = map
            .get(&name)
            .is_none_or(|known| file.as_os_str().len() > known.as_os_str().len());
        if longer {
            map.insert(name, file);
        }
    }
    map
}

/// Every file under `dir`, without following symlinks or entering `.git`.
pub(crate) fn walkdir(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => {
                    if entry.file_name() != ".git" {
                        stack.push(path);
                    }
                }
                Ok(_) => out.push(path),
                Err(_) => {}
            }
        }
    }
    out.sort();
    out
}

/// `owner/repo` of a GitHub remote.
fn github_repo(remote: &str) -> Option<String> {
    let rest = remote
        .strip_prefix("https://github.com/")
        .or_else(|| remote.strip_prefix("http://github.com/"))
        .or_else(|| remote.strip_prefix("git@github.com:"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let (owner, repo) = rest.split_once('/')?;
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/'))
        .then(|| format!("{owner}/{repo}"))
}

/// The commit at the head of the default branch.
async fn github_head(client: &reqwest::Client, repo: &str) -> Result<Option<String>, Error> {
    let response = client
        .get(format!("https://api.github.com/repos/{repo}/commits/HEAD"))
        .header("Accept", "application/vnd.github.sha")
        .header("User-Agent", "nextbrew")
        .send()
        .await
        .map_err(Error::network("failed to query the tap"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(Error::NetworkFailure {
            message: format!("{repo} does not exist"),
        });
    }
    if !response.status().is_success() {
        return Ok(None);
    }
    let sha = response
        .text()
        .await
        .map_err(Error::network("failed to read the tap's head"))?;
    Ok(Some(sha.trim().to_string()).filter(|s| s.len() == 40))
}

/// Download and unpack the tarball of `repo`'s default branch into
/// `dest`. Returns the commit it is from.
async fn fetch_github(
    client: &reqwest::Client,
    repo: &str,
    dest: &Path,
) -> Result<Option<String>, Error> {
    let commit = github_head(client, repo).await?;
    let reference = commit.as_deref().unwrap_or("HEAD");
    let response = client
        .get(format!(
            "https://codeload.github.com/{repo}/tar.gz/{reference}"
        ))
        .header("User-Agent", "nextbrew")
        .send()
        .await
        .map_err(Error::network("failed to download the tap"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::NetworkFailure {
            message: if status == reqwest::StatusCode::NOT_FOUND {
                format!("https://github.com/{repo} does not exist or is private")
            } else {
                format!("downloading the tap returned HTTP {status}")
            },
        });
    }
    let bytes = response
        .bytes()
        .await
        .map_err(Error::network("failed to download the tap"))?;
    let unpacked = dest.with_extension("unpack");
    let _ = std::fs::remove_dir_all(&unpacked);
    std::fs::create_dir_all(&unpacked)
        .map_err(Error::file("failed to create the tap directory"))?;
    crate::extraction::extract_tarball_from_reader(std::io::Cursor::new(bytes), &unpacked)?;
    // The tarball holds one `<repo>-<commit>/` directory.
    let top = std::fs::read_dir(&unpacked)
        .map_err(Error::file("failed to read the tap"))?
        .filter_map(Result::ok)
        .find(|e| e.path().is_dir())
        .ok_or_else(|| Error::FileError {
            message: "the tap's tarball is empty".to_string(),
        })?;
    std::fs::rename(top.path(), dest).map_err(Error::file("failed to unpack the tap"))?;
    let _ = std::fs::remove_dir_all(&unpacked);
    Ok(commit)
}

/// `git clone` a tap from a remote that is not GitHub.
fn clone_git(remote: &str, dest: &Path) -> Result<Option<String>, Error> {
    let status = std::process::Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "clone",
            "--origin=origin",
            "-q",
        ])
        .args([
            "--template=",
            "--config",
            "core.fsmonitor=false",
            "--end-of-options",
        ])
        .arg(remote)
        .arg(dest)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .map_err(Error::exec(
            "failed to run git (needed to tap a repository outside GitHub)",
        ))?;
    if !status.success() {
        return Err(Error::ExecutionError {
            message: format!("`git clone {remote}` failed"),
        });
    }
    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(dest)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    Ok(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tap_names_like_tap_fetch() {
        let tap = TapName::parse("Hashicorp/homebrew-Tap").unwrap();
        assert_eq!(tap.name(), "hashicorp/tap");
        assert_eq!(
            tap.default_remote(),
            "https://github.com/hashicorp/homebrew-tap"
        );
        assert!(TapName::parse("nope").is_err());
        assert!(TapName::parse("a/b/c").is_err());
        assert!(TapName::parse("Homebrew/homebrew-core").unwrap().is_core());
        let (tap, name) = TapName::split_full_name("hashicorp/tap/terraform").unwrap();
        assert_eq!(
            (tap.name().as_str(), name.as_str()),
            ("hashicorp/tap", "terraform")
        );
        assert!(TapName::split_full_name("jq").is_none());
        assert_eq!(
            github_repo("https://github.com/a/homebrew-b.git").as_deref(),
            Some("a/homebrew-b")
        );
        assert!(github_repo("https://gitlab.com/a/b").is_none());
    }

    #[test]
    fn summarises_like_tap_contents() {
        let c = TapContents {
            formulae: 5,
            casks: 3,
            commands: 2,
            files: 123,
            bytes: 456_700,
        };
        assert_eq!(
            c.summary(),
            "2 commands, 3 casks and 5 formulae (123 files, 456.7KB)"
        );
        let one = TapContents {
            formulae: 1,
            files: 1,
            bytes: 900,
            ..Default::default()
        };
        assert_eq!(one.summary(), "1 formula (900B)");
    }

    #[test]
    fn finds_formula_files_aliases_and_renames() {
        let tmp = tempfile::tempdir().unwrap();
        let taps = Taps::new(tmp.path());
        let tap = TapName::parse("me/tools").unwrap();
        let dir = taps.path(&tap);
        std::fs::create_dir_all(dir.join("Formula/f")).unwrap();
        std::fs::create_dir_all(dir.join("Aliases")).unwrap();
        std::fs::write(dir.join("Formula/foo.rb"), "").unwrap();
        std::fs::write(dir.join("Formula/f/foo.rb"), "").unwrap();
        std::os::unix::fs::symlink("../Formula/foo.rb", dir.join("Aliases/fu")).unwrap();
        std::fs::write(dir.join("formula_renames.json"), r#"{"old": "foo"}"#).unwrap();
        assert_eq!(taps.installed(), std::slice::from_ref(&tap));
        let (name, file) = taps.find_formula(&tap, "foo").unwrap();
        assert_eq!(name, "foo");
        assert!(file.ends_with("Formula/f/foo.rb"), "the longer path wins");
        assert_eq!(taps.find_formula(&tap, "fu").unwrap().0, "foo");
        assert_eq!(taps.find_formula(&tap, "old").unwrap().0, "foo");
        assert!(taps.find_formula(&tap, "bar").is_none());
    }
}
