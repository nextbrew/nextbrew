use std::path::PathBuf;

use nb_core::{Error, Formula, formula_token};

use super::Installer;

#[derive(Debug, PartialEq, Eq)]
pub enum LinkOutcome {
    Linked(usize),
    AlreadyLinked,
    /// Keg-only and not forced; `brew link` warns and skips it.
    KegOnly,
}

impl Installer {
    /// `brew link`: symlink an installed formula into the prefix.
    pub async fn link(&mut self, name: &str, force: bool) -> Result<LinkOutcome, Error> {
        let (keg, version) = self.formula_keg(name)?;
        if !self.linker.collect_linked_files(&keg)?.is_empty() {
            return Ok(LinkOutcome::AlreadyLinked);
        }
        if !force {
            let formula = self.api_client.get_formula(name).await?;
            // Versioned formulae link without --force, as in Homebrew.
            let versioned = formula
                .keg_only_reason
                .as_ref()
                .is_some_and(|reason| reason.reason == ":versioned_formula");
            if formula.is_keg_only() && !versioned {
                return Ok(LinkOutcome::KegOnly);
            }
        }
        let linked = self.linker.link_keg(&keg)?;
        self.record_linked_files(name, &version, &linked);
        Ok(LinkOutcome::Linked(linked.len()))
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
        let unversioned = unversioned_name(&formula.name);
        Ok(!related.iter().any(|other| {
            self.is_installed(&other.name)
                || (unversioned.as_ref() == Some(&other.name) && other.is_keg_only())
        }))
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
