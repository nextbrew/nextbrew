use std::path::PathBuf;

use nb_core::{Error, Formula, formula_token};

use super::Installer;

#[derive(Debug, PartialEq, Eq)]
pub enum LinkOutcome {
    /// Linked this many files, after unlinking these related kegs.
    Linked {
        count: usize,
        unlinked: Vec<(PathBuf, usize)>,
    },
    AlreadyLinked,
    /// Keg-only and not forced; `brew link` warns and skips it.
    KegOnly,
}

/// What `brew link --dry-run` lists, and whether to add the keg-only note.
#[derive(Debug, PartialEq, Eq)]
pub struct LinkDryRun {
    pub paths: Vec<String>,
    pub keg_only: bool,
}

/// Keg-only only because it is a versioned formula, which links anyway.
fn is_versioned(formula: &Formula) -> bool {
    formula
        .keg_only_reason
        .as_ref()
        .is_some_and(|reason| reason.reason == ":versioned_formula")
}

impl Installer {
    /// `brew link`: symlink an installed formula into the prefix, with
    /// `overwrite` deleting the files in the way.
    pub async fn link(
        &mut self,
        name: &str,
        force: bool,
        overwrite: bool,
    ) -> Result<LinkOutcome, Error> {
        let (keg, version) = self.formula_keg(name)?;
        if !self.linker.collect_linked_files(&keg)?.is_empty() {
            return Ok(LinkOutcome::AlreadyLinked);
        }
        let formula = self.api_client.get_formula(name).await?;
        if !force && formula.is_keg_only() && !is_versioned(&formula) {
            return Ok(LinkOutcome::KegOnly);
        }
        let candidates = self
            .overwrite_candidates(&formula)
            .await
            .unwrap_or_default();
        let unlinked = self.unlink_overwritten(formula.is_keg_only(), &candidates)?;
        let linked = if overwrite {
            // A directory in the way stops it part way; Homebrew then
            // unlinks what it linked.
            self.linker.link_keg_overwrite(&keg).inspect_err(|_| {
                let _ = self.linker.unlink_files(&keg);
            })?
        } else {
            self.linker.link_keg(&keg)?
        };
        self.record_linked_files(name, &version, &linked);
        Ok(LinkOutcome::Linked {
            count: linked.len(),
            unlinked,
        })
    }

    /// `brew link --dry-run`: what linking would create, or with `overwrite`
    /// delete, changing nothing. `None` when the keg is already linked.
    pub async fn link_dry_run(
        &self,
        name: &str,
        overwrite: bool,
    ) -> Result<Option<LinkDryRun>, Error> {
        let (keg, _) = self.formula_keg(name)?;
        if !self.linker.collect_linked_files(&keg)?.is_empty() {
            return Ok(None);
        }
        let formula = self.api_client.get_formula(name).await?;
        let paths = if overwrite {
            self.linker.would_overwrite(&keg)
        } else {
            self.linker
                .would_link(&keg)
                .iter()
                .map(|p| p.display().to_string())
                .collect()
        };
        Ok(Some(LinkDryRun {
            paths,
            keg_only: formula.is_keg_only() && !is_versioned(&formula),
        }))
    }

    /// Homebrew's `auto_link_versioned_keg_only?` for a formula asked for
    /// by name and not installed before: a keg-only `:versioned_formula`
    /// links unless a related formula is installed (another version, the
    /// `-full` twin, the unversioned one, and theirs in turn, aliases
    /// resolved), or the unversioned formula is keg-only as well.
    pub(super) async fn links_versioned_keg_only(&self, formula: &Formula) -> Result<bool, Error> {
        let versioned = formula
            .keg_only_reason
            .as_ref()
            .is_some_and(|reason| reason.reason == ":versioned_formula");
        if !formula.is_keg_only() || !versioned {
            return Ok(false);
        }
        let related = self.link_overwrite_formulae(formula).await?;
        let unversioned = unversioned_name(&formula.name);
        Ok(!related.iter().any(|other| {
            self.is_installed(&other.name)
                || (unversioned.as_ref() == Some(&other.name) && other.is_keg_only())
        }))
    }

    /// `Formula#link_overwrite_formulae`: other versions of the formula,
    /// its `-full` twin, the unversioned one, and theirs in turn, aliases
    /// resolved.
    async fn link_overwrite_formulae(&self, formula: &Formula) -> Result<Vec<Formula>, Error> {
        let names = self.api_client.formula_names().await?;
        let related_names = |name: &str| {
            let prefix = unversioned_name(name).unwrap_or_else(|| name.to_string());
            let mut related: Vec<String> = names
                .iter()
                .filter(|other| *other != name && unversioned_name(other).as_ref() == Some(&prefix))
                .cloned()
                .collect();
            let full = match name.strip_suffix("-full") {
                Some(short) => short.to_string(),
                None => format!("{name}-full"),
            };
            if names.contains(&full) {
                related.push(full);
            }
            related.extend(unversioned_name(name));
            related
        };

        let mut seen: Vec<String> = Vec::new();
        let mut related: Vec<Formula> = Vec::new();
        let mut pending = related_names(&formula.name);
        while let Some(name) = pending.pop() {
            if name == formula.name || seen.contains(&name) {
                continue;
            }
            seen.push(name.clone());
            let Ok(other) = self.api_client.get_formula(&name).await else {
                continue;
            };
            pending.extend(related_names(&other.name));
            if other.name != formula.name && !related.iter().any(|r| r.name == other.name) {
                related.push(other);
            }
        }
        Ok(related)
    }

    /// The related formulae linking `formula` may have to unlink first, and
    /// whether each is keg-only.
    pub(super) async fn overwrite_candidates(
        &self,
        formula: &Formula,
    ) -> Result<Vec<(String, bool)>, Error> {
        Ok(self
            .link_overwrite_formulae(formula)
            .await?
            .into_iter()
            .map(|other| {
                let keg_only = other.is_keg_only();
                (other.name, keg_only)
            })
            .collect())
    }

    /// `Unlink.unlink_link_overwrite_formulae`: before `formula` links, the
    /// linked related formulae are unlinked (only the keg-only ones when it
    /// is not keg-only itself). Returns each keg unlinked and its count.
    pub(super) fn unlink_overwritten(
        &mut self,
        keg_only: bool,
        candidates: &[(String, bool)],
    ) -> Result<Vec<(PathBuf, usize)>, Error> {
        let mut unlinked = Vec::new();
        for (other, other_keg_only) in candidates {
            if !keg_only && !other_keg_only {
                continue;
            }
            if !self.is_installed(other) || !self.is_linked(other)? {
                continue;
            }
            let (keg, _) = self.formula_keg(other)?;
            let count = self.unlink(other)?;
            unlinked.push((keg, count));
        }
        Ok(unlinked)
    }

    /// Unlink what Homebrew would have unlinked before `item` links, given
    /// that it installs in [`PlannedInstall::link_rank`] order. `false` when
    /// a related formula ranked later is already linked and would have
    /// unlinked this one: then it stays unlinked.
    pub(super) fn make_room_to_link(
        &mut self,
        item: &super::PlannedInstall,
        report: &impl Fn(crate::progress::InstallProgress),
    ) -> Result<bool, Error> {
        let keg_only = item.formula.is_keg_only();
        let superseded = item.link_overwrite.iter().any(|(other, other_keg_only)| {
            item.link_later.contains(other)
                && (*other_keg_only || keg_only)
                && self.is_installed(other)
                && self.is_linked(other).unwrap_or(false)
        });
        if superseded {
            return Ok(false);
        }
        let earlier: Vec<(String, bool)> = item
            .link_overwrite
            .iter()
            .filter(|(other, _)| !item.link_later.contains(other))
            .cloned()
            .collect();
        for (keg, count) in self.unlink_overwritten(keg_only, &earlier)? {
            report(crate::progress::InstallProgress::Unlinked {
                keg: keg.display().to_string(),
                count,
            });
        }
        Ok(true)
    }

    /// Whether an installed formula's keg is linked into the prefix.
    pub fn is_linked(&self, name: &str) -> Result<bool, Error> {
        let (keg, _) = self.formula_keg(name)?;
        Ok(!self.linker.collect_linked_files(&keg)?.is_empty())
    }

    /// Whether a formula is keg-only here.
    pub async fn is_keg_only(&self, name: &str) -> Result<bool, Error> {
        Ok(self.api_client.get_formula(name).await?.is_keg_only())
    }

    /// `brew unlink`: remove an installed formula's symlinks from the
    /// prefix. Returns how many were removed.
    pub fn unlink(&mut self, name: &str) -> Result<usize, Error> {
        let (keg, _) = self.formula_keg(name)?;
        let removed = self.linker.unlink_files(&keg)?;
        let tx = self.db.transaction()?;
        tx.clear_keg_file_records(name)?;
        tx.commit()?;
        Ok(removed.len())
    }

    /// The installed keg of a formula, for the commands that only take one.
    pub fn formula_keg(&self, name: &str) -> Result<(PathBuf, String), Error> {
        if name.starts_with("cask:") {
            return Err(Error::InvalidArgument {
                message: "nextbrew does not link or unlink casks".to_string(),
            });
        }
        let keg = self.db.get_installed(name).ok_or(Error::NotInstalled {
            name: name.to_string(),
        })?;
        let path = self.cellar.keg_path(formula_token(name), &keg.version);
        Ok((path, keg.version))
    }
}

/// `Formula#unversioned_formula_name`: the name without its `@version`,
/// keeping a `-full` suffix (`foo@1.2-full` is `foo-full`).
fn unversioned_name(name: &str) -> Option<String> {
    let (base, rest) = name.split_once('@')?;
    let (version, full) = match rest.strip_suffix("-full") {
        Some(version) => (version, "-full"),
        None => (rest, ""),
    };
    (!version.is_empty() && version.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .then(|| format!("{base}{full}"))
}

#[cfg(test)]
mod tests {
    use super::unversioned_name;

    #[test]
    fn unversioned_names_follow_homebrew() {
        assert_eq!(unversioned_name("openssl@3.5").as_deref(), Some("openssl"));
        assert_eq!(unversioned_name("foo@1-full").as_deref(), Some("foo-full"));
        assert_eq!(unversioned_name("openssl"), None);
        assert_eq!(unversioned_name("foo@bar"), None);
    }
}
