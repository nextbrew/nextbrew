use std::path::PathBuf;

use nb_core::{Error, formula_token};

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
