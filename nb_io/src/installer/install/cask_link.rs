//! `brew link <cask>` / `brew unlink <cask>`: a cask's `Symlinked`
//! artifacts (binaries, man pages, shell completions) linked into the
//! prefix again, or taken away. nextbrew keeps them in the cask's keg under
//! the prefix's layout, so each file there is one link.

use std::fs;
use std::path::{Path, PathBuf};

use nb_core::Error;

use super::Installer;

/// One of a cask's links into the prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaskLink {
    /// `Cask::Artifact`'s English name: `Binary`, `Manpage`, …
    pub kind: &'static str,
    /// The file in the keg.
    pub source: PathBuf,
    /// Where it goes in the prefix.
    pub target: PathBuf,
}

impl CaskLink {
    /// `source.basename` as Homebrew prints it: the downloaded file the
    /// keg's entry leads to.
    pub fn source_name(&self) -> String {
        let source = fs::read_link(&self.source).unwrap_or_else(|_| self.source.clone());
        source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// What linking does to one link, as `Symlinked#link_action` decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaskLinkAction {
    Link,
    Overwrite,
    AlreadyLinked,
    /// Another formula's link of that name is there.
    SkipFormula(String),
    Conflict,
}

/// What `nb link` / `nb unlink` of a cask did, for the command to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaskLinkEvent {
    /// `--dry-run`: a path it would link or remove.
    Would(PathBuf),
    /// Something else was there and is replaced.
    Overwriting(CaskLink),
    Linked(CaskLink),
    AlreadyLinked(CaskLink),
    SkippedFormula(CaskLink, String),
    Unlinked(CaskLink),
}

/// The directories of a keg that are linked into the prefix.
const LINKED_DIRS: &[&str] = &["bin", "sbin", "etc", "share"];

/// The artifact a file in a cask's keg is, by where it goes, in Homebrew's
/// order (`AbstractArtifact#sort_order`).
fn kind_of(rel: &Path) -> (usize, &'static str) {
    let rel = rel.to_string_lossy();
    if rel.starts_with("share/man/") {
        (1, "Manpage")
    } else if rel.starts_with("etc/bash_completion.d/") || rel.starts_with("share/bash-completion/")
    {
        (2, "Bash Completion")
    } else if rel.starts_with("share/zsh/site-functions/") {
        (2, "Zsh Completion")
    } else if rel.starts_with("share/fish/vendor_completions.d/") {
        (2, "Fish Completion")
    } else {
        (0, "Binary")
    }
}

/// Every file (or link) under the keg's linked directories, as links.
fn cask_links(keg: &Path, prefix: &Path) -> Vec<CaskLink> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            let is_dir = path
                .symlink_metadata()
                .is_ok_and(|meta| meta.file_type().is_dir());
            if is_dir {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    for dir in LINKED_DIRS {
        walk(&keg.join(dir), &mut files);
    }
    let mut links: Vec<(usize, CaskLink)> = files
        .into_iter()
        .filter_map(|source| {
            let rel = source.strip_prefix(keg).ok()?.to_path_buf();
            let (rank, kind) = kind_of(&rel);
            Some((
                rank,
                CaskLink {
                    kind,
                    target: prefix.join(&rel),
                    source,
                },
            ))
        })
        .collect();
    links.sort_by_key(|(rank, _)| *rank);
    links.into_iter().map(|(_, link)| link).collect()
}

/// `Symlinked#target_links_to_source?`.
fn links_to_source(link: &CaskLink) -> bool {
    let Ok(points) = fs::read_link(&link.target) else {
        return false;
    };
    let points = if points.is_relative() {
        link.target.parent().unwrap_or(Path::new("")).join(points)
    } else {
        points
    };
    points == link.source
        || fs::canonicalize(&link.target)
            .ok()
            .is_some_and(|real| fs::canonicalize(&link.source).ok() == Some(real))
}

/// `Symlinked#conflicting_formula`: the formula whose keg a link there
/// leads into.
fn conflicting_formula(target: &Path, cellar: &Path) -> Option<String> {
    if !target.is_symlink() {
        return None;
    }
    let real = fs::canonicalize(target).ok()?;
    let cellar = fs::canonicalize(cellar).unwrap_or_else(|_| cellar.to_path_buf());
    let name = real.strip_prefix(&cellar).ok()?.components().next()?;
    let name = name.as_os_str().to_string_lossy().into_owned();
    (!name.starts_with("cask:")).then_some(name)
}

/// `Symlinked#link_action`. `cask_kegs` is the cask's directory of
/// versions (`Caskroom/<token>`).
fn link_action(
    link: &CaskLink,
    cask_kegs: &Path,
    cellar: &Path,
    force: bool,
    overwrite: bool,
) -> CaskLinkAction {
    let Ok(meta) = link.target.symlink_metadata() else {
        return CaskLinkAction::Link;
    };
    // `target.exist?` follows the link: a dangling one is not there.
    if meta.file_type().is_symlink() && link.target.metadata().is_err() {
        return CaskLinkAction::Link;
    }
    let ours = links_to_source(link);
    let in_caskroom = fs::canonicalize(&link.target)
        .ok()
        .zip(fs::canonicalize(cask_kegs).ok())
        .is_some_and(|(real, kegs)| real.starts_with(kegs));
    if overwrite || (force && meta.file_type().is_symlink() && (ours || in_caskroom)) {
        CaskLinkAction::Overwrite
    } else if ours {
        CaskLinkAction::AlreadyLinked
    } else if let Some(formula) = conflicting_formula(&link.target, cellar) {
        CaskLinkAction::SkipFormula(formula)
    } else {
        CaskLinkAction::Conflict
    }
}

fn remove(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => fs::remove_dir_all(path).map_err(Error::store("failed to remove a file")),
    }
}

impl Installer {
    /// The keg of an installed cask (`cask:<token>`).
    fn cask_keg(&self, name: &str) -> Result<PathBuf, Error> {
        let token = name.strip_prefix("cask:").unwrap_or(name);
        let keg = self
            .db
            .get_installed(name)
            .ok_or_else(|| Error::InvalidArgument {
                message: format!("Cask '{token}' is not installed."),
            })?;
        Ok(self.cellar.keg_path(name, &keg.version))
    }

    /// `brew link --cask`: link the cask's files into the prefix again.
    /// Nothing changes if one of them is in the way (unless `overwrite`,
    /// or `force` for an old link of this cask).
    pub fn link_cask(
        &mut self,
        name: &str,
        force: bool,
        overwrite: bool,
        dry_run: bool,
        mut report: impl FnMut(CaskLinkEvent),
    ) -> Result<(), Error> {
        let token = name.strip_prefix("cask:").unwrap_or(name);
        let keg = self.cask_keg(name)?;
        let cellar = self.cellar.dir().to_path_buf();
        let cask_kegs = keg.parent().unwrap_or(&keg).to_path_buf();
        let links = cask_links(&keg, &self.prefix);
        let actions: Vec<CaskLinkAction> = links
            .iter()
            .map(|link| link_action(link, &cask_kegs, &cellar, force, overwrite))
            .collect();
        if let Some((link, _)) = links
            .iter()
            .zip(&actions)
            .find(|(_, action)| **action == CaskLinkAction::Conflict)
        {
            return Err(Error::InvalidArgument {
                message: format!(
                    "Could not link {token}: {} already exists.\nTo force the link and overwrite all conflicting files:\n  nb link --cask --overwrite {token}",
                    link.target.display()
                ),
            });
        }
        let mut linked = Vec::new();
        for (link, action) in links.into_iter().zip(actions) {
            match action {
                CaskLinkAction::Overwrite if dry_run => report(CaskLinkEvent::Would(link.target)),
                CaskLinkAction::Link if dry_run => {
                    // `ln --force` also replaces a broken link.
                    if !overwrite || link.target.is_symlink() {
                        report(CaskLinkEvent::Would(link.target));
                    }
                }
                CaskLinkAction::AlreadyLinked if dry_run => {}
                CaskLinkAction::SkipFormula(_) if dry_run => {}
                CaskLinkAction::AlreadyLinked => report(CaskLinkEvent::AlreadyLinked(link)),
                CaskLinkAction::SkipFormula(formula) => {
                    report(CaskLinkEvent::SkippedFormula(link, formula))
                }
                CaskLinkAction::Link | CaskLinkAction::Overwrite => {
                    if action == CaskLinkAction::Overwrite {
                        report(CaskLinkEvent::Overwriting(link.clone()));
                    }
                    remove(&link.target)?;
                    if let Some(dir) = link.target.parent() {
                        fs::create_dir_all(dir)
                            .map_err(Error::store("failed to create link directory"))?;
                    }
                    std::os::unix::fs::symlink(&link.source, &link.target)
                        .map_err(Error::store("failed to create symlink"))?;
                    report(CaskLinkEvent::Linked(link.clone()));
                    linked.push(link);
                }
                CaskLinkAction::Conflict => unreachable!("checked above"),
            }
        }
        if !linked.is_empty() {
            let version = self
                .db
                .get_installed(name)
                .map(|keg| keg.version)
                .unwrap_or_default();
            let tx = self.db.transaction()?;
            for link in &linked {
                tx.record_linked_file(
                    name,
                    &version,
                    &link.target.to_string_lossy(),
                    &link.source.to_string_lossy(),
                )?;
            }
            tx.commit()?;
        }
        Ok(())
    }

    /// `brew unlink --cask`: take away the cask's links that still lead to
    /// it.
    pub fn unlink_cask(
        &mut self,
        name: &str,
        dry_run: bool,
        mut report: impl FnMut(CaskLinkEvent),
    ) -> Result<(), Error> {
        let keg = self.cask_keg(name)?;
        for link in cask_links(&keg, &self.prefix) {
            if !links_to_source(&link) {
                continue;
            }
            if dry_run {
                report(CaskLinkEvent::Would(link.target));
                continue;
            }
            remove(&link.target)?;
            report(CaskLinkEvent::Unlinked(link));
        }
        if !dry_run {
            let tx = self.db.transaction()?;
            tx.clear_keg_file_records(name)?;
            tx.commit()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }

    #[test]
    fn a_casks_links_come_in_homebrews_order() {
        let tmp = tempfile::tempdir().unwrap();
        let keg = tmp.path().join("Cellar/cask:ant/1.0");
        let prefix = tmp.path();
        file(&keg.join("share/zsh/site-functions/_ant"));
        file(&keg.join("share/man/man1/ant.1"));
        file(&keg.join("bin/ant"));
        file(&keg.join("etc/bash_completion.d/ant"));
        file(&keg.join("cask.json"));
        let links: Vec<(&str, PathBuf)> = cask_links(&keg, prefix)
            .into_iter()
            .map(|link| (link.kind, link.target))
            .collect();
        assert_eq!(
            links,
            vec![
                ("Binary", prefix.join("bin/ant")),
                ("Manpage", prefix.join("share/man/man1/ant.1")),
                ("Bash Completion", prefix.join("etc/bash_completion.d/ant")),
                (
                    "Zsh Completion",
                    prefix.join("share/zsh/site-functions/_ant")
                ),
            ]
        );
    }

    #[test]
    fn decides_like_symlinked_link_action() {
        let tmp = tempfile::tempdir().unwrap();
        let cellar = tmp.path().join("Cellar");
        let kegs = cellar.join("cask:ant");
        let keg = kegs.join("1.0");
        let source = keg.join("bin/ant");
        file(&source);
        let link = CaskLink {
            kind: "Binary",
            source: source.clone(),
            target: tmp.path().join("bin/ant"),
        };
        let action = |force, overwrite| link_action(&link, &kegs, &cellar, force, overwrite);

        assert_eq!(action(false, false), CaskLinkAction::Link);
        fs::create_dir_all(tmp.path().join("bin")).unwrap();
        std::os::unix::fs::symlink(&source, &link.target).unwrap();
        assert_eq!(action(false, false), CaskLinkAction::AlreadyLinked);
        assert_eq!(action(true, false), CaskLinkAction::Overwrite);

        // Another formula's link of that name.
        fs::remove_file(&link.target).unwrap();
        let other = cellar.join("ant/2.0/bin/ant");
        file(&other);
        std::os::unix::fs::symlink(&other, &link.target).unwrap();
        assert_eq!(
            action(false, false),
            CaskLinkAction::SkipFormula("ant".to_string())
        );

        // A plain file is a conflict unless overwriting.
        fs::remove_file(&link.target).unwrap();
        fs::write(&link.target, "mine").unwrap();
        assert_eq!(action(true, false), CaskLinkAction::Conflict);
        assert_eq!(action(false, true), CaskLinkAction::Overwrite);

        // A dangling link is not there.
        fs::remove_file(&link.target).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("gone"), &link.target).unwrap();
        assert_eq!(action(false, false), CaskLinkAction::Link);
    }
}
