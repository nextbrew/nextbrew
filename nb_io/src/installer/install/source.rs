use std::fs;
use std::path::{Path, PathBuf};

use nb_core::{BuildPlan, Error};
use tracing::warn;

use crate::progress::InstallProgress;

use super::{Installer, PlannedInstall, dependency_cellar_path};

impl Installer {
    /// `Build#expand_deps` as superenv gets them: the formula's
    /// dependencies, build ones too, each after its own run-time ones, and
    /// whether each is keg-only.
    async fn build_env_deps(&self, formula: &nb_core::Formula) -> Vec<crate::build::BuildDep> {
        let mut deps = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // (name, its dependencies have been put first)
        let mut stack: Vec<(String, bool)> = formula
            .runtime_dependencies()
            .into_iter()
            .chain(formula.all_build_dependencies())
            .rev()
            .map(|name| (name, false))
            .collect();
        let mut found = std::collections::HashMap::new();
        // `run_time_deps`: those reached without going through a build one.
        let mut run_time = std::collections::HashSet::new();
        let mut todo = formula.runtime_dependencies();
        while let Some(name) = todo.pop() {
            if !run_time.insert(name.clone()) {
                continue;
            }
            let dep = self.formula(&name).await.ok();
            if let Some(dep) = &dep {
                todo.extend(dep.runtime_dependencies());
            }
            found.insert(name, dep);
        }
        while let Some((name, expanded)) = stack.pop() {
            if seen.contains(&name) {
                continue;
            }
            if !found.contains_key(&name) {
                found.insert(name.clone(), self.formula(&name).await.ok());
            }
            let dep = found[&name].as_ref();
            if expanded {
                seen.insert(name.clone());
                deps.push(crate::build::BuildDep {
                    run_time: run_time.contains(&name),
                    name,
                    keg_only: dep.is_some_and(nb_core::Formula::is_keg_only),
                    dependencies: dep
                        .map(|dep| {
                            let mut all = dep.runtime_dependencies();
                            all.extend(dep.all_build_dependencies());
                            all
                        })
                        .unwrap_or_default(),
                });
                continue;
            }
            stack.push((name.clone(), true));
            if let Some(dep) = dep {
                for child in dep.runtime_dependencies().into_iter().rev() {
                    if !seen.contains(&child) {
                        stack.push((child, false));
                    }
                }
            }
        }
        deps
    }

    pub(super) async fn install_from_source(
        &mut self,
        item: &PlannedInstall,
        build_plan: &BuildPlan,
        link: bool,
        report: &impl Fn(InstallProgress),
    ) -> Result<(), Error> {
        let install_name = &item.install_name;
        let formula_name = &item.formula.name;
        let version = item.formula.effective_version();

        let formula_rb = match item.formula.third_party_tap() {
            // A tap formula is built from its file in the tap.
            Some(tap) => self.tap_formula_rb(tap, formula_name)?,
            None => {
                let ruby_source_path =
                    item.formula.ruby_source_path.as_deref().ok_or_else(|| {
                        Error::ExecutionError {
                            message: format!("no ruby_source_path for formula '{formula_name}'"),
                        }
                    })?;

                let cache_dir = self.prefix.join("tmp").join("rb_cache");
                let formula_rb_checksum = item
                    .formula
                    .ruby_source_checksum
                    .as_ref()
                    .map(|checksum| checksum.sha256.as_str());

                self.api_client
                    .fetch_formula_rb(
                        ruby_source_path,
                        item.formula.tap_git_head.as_deref(),
                        &cache_dir,
                        formula_rb_checksum,
                    )
                    .await?
            }
        };

        let mut installed_deps = std::collections::HashMap::new();
        for dep_name in &build_plan.runtime_dependencies {
            if let Some(keg) = self.db.get_installed(dep_name) {
                installed_deps.insert(
                    dep_name.clone(),
                    crate::build::DepInfo {
                        cellar_path: dependency_cellar_path(&self.cellar, &keg.name, &keg.version),
                    },
                );
            }
        }

        let keg_path = self.cellar.keg_path(formula_name, &version);
        let previous_keg_backup =
            Self::backup_existing_source_keg(&keg_path, formula_name, &version)?;

        let deps = self.build_env_deps(&item.formula).await;
        // Homebrew's own Ruby reads formulae as Homebrew does; without it,
        // the one on `PATH` (or the system's) may be too old for some.
        let ruby = match self.portable_ruby().await {
            Ok(ruby) => Some(ruby),
            Err(e) => {
                eprintln!(
                    "Warning: could not get Homebrew's portable Ruby ({e}); building with the ruby on PATH."
                );
                None
            }
        };
        // Where the formula's own `Patches/` are: its tap's checkout, or
        // homebrew-core online.
        let patch_base = match item.formula.third_party_tap() {
            Some(_) => formula_rb
                .ancestors()
                .find(|dir| dir.join(".git").exists())
                .or_else(|| formula_rb.parent())
                .map(|dir| dir.display().to_string())
                .unwrap_or_default(),
            None => {
                crate::network::api::homebrew_core_raw_base(item.formula.tap_git_head.as_deref())
            }
        };
        let executor = crate::build::BuildExecutor::new(self.prefix.clone());
        if let Err(build_err) = executor
            .execute(
                build_plan,
                &formula_rb,
                &installed_deps,
                &deps,
                ruby,
                &patch_base,
            )
            .await
        {
            if let Some(backup_path) = previous_keg_backup.as_ref() {
                Self::restore_source_keg_from_backup(
                    &keg_path,
                    backup_path,
                    formula_name,
                    &version,
                )?;
            } else {
                // Like Homebrew, leave no keg behind from a failed build.
                Self::cleanup_materialized(&self.cellar, formula_name, &version);
            }
            return Err(match build_err {
                Error::BuildFailed { version, .. } => Error::BuildFailed {
                    name: match item.formula.third_party_tap() {
                        Some(tap) => format!("{tap}/{formula_name}"),
                        None => formula_name.clone(),
                    },
                    version,
                    report: self.build_failure_report(&item.formula),
                },
                other => other,
            });
        }

        if let Some(backup_path) = previous_keg_backup.as_ref() {
            Self::remove_source_keg_backup(backup_path, formula_name, &version)?;
        }

        report(InstallProgress::UnpackCompleted {
            name: formula_name.clone(),
        });

        let store_key = format!("source:{formula_name}:{version}");

        let tx = self.db.transaction().inspect_err(|_| {
            Self::cleanup_materialized(&self.cellar, formula_name, &version);
        })?;

        if let Err(e) = tx.record_install(install_name, &version, &store_key) {
            drop(tx);
            Self::cleanup_materialized(&self.cellar, formula_name, &version);
            return Err(e);
        }

        if let Err(e) = tx.commit() {
            Self::cleanup_materialized(&self.cellar, formula_name, &version);
            return Err(e);
        }
        self.record_tap(install_name, &item.formula)?;

        // Aliases only: Homebrew refreshes an old name's opt link that a
        // rename left behind, but never makes a new one.
        if let Err(e) = self.linker.link_opt(&keg_path, &item.formula.aliases) {
            warn!(formula = %install_name, error = %e, "failed to create opt link");
        }

        let should_link = link && (!item.formula.is_keg_only() || item.link_keg_only);

        if should_link && self.make_room_to_link(item, report)? {
            report(InstallProgress::LinkStarted {
                name: formula_name.clone(),
            });
            match self.linker.link_keg(&keg_path) {
                Ok(files) => {
                    report(InstallProgress::LinkCompleted {
                        name: formula_name.clone(),
                    });
                    if !files.is_empty()
                        && let Ok(tx) = self.db.transaction()
                    {
                        let mut ok = true;
                        for linked in &files {
                            if tx
                                .record_linked_file(
                                    install_name,
                                    &version,
                                    &linked.link_path.to_string_lossy(),
                                    &linked.target_path.to_string_lossy(),
                                )
                                .is_err()
                            {
                                ok = false;
                                break;
                            }
                        }
                        if ok {
                            let _ = tx.commit();
                        }
                    }
                }
                Err(e) => {
                    // Installed but unlinked, as `brew install` leaves it on
                    // a conflict: `opt/` stays for the dependents.
                    let _ = self.linker.unlink_files(&keg_path);
                    report(InstallProgress::InstallCompleted {
                        name: formula_name.clone(),
                    });
                    return Err(e);
                }
            }
        } else if link {
            let reason = match &item.formula.keg_only {
                // Kept unlinked for a related formula linked after it.
                _ if !item.formula.is_keg_only() || item.link_keg_only => {
                    "another version is linked".to_string()
                }
                nb_core::KegOnly::Reason(s) => format!("keg-only ({s})"),
                _ if item.formula.name.contains('@') => "keg-only (versioned formula)".to_string(),
                _ => "keg-only".to_string(),
            };
            report(InstallProgress::LinkSkipped {
                name: formula_name.clone(),
                reason,
            });
        }

        report(InstallProgress::InstallCompleted {
            name: formula_name.clone(),
        });
        Ok(())
    }

    /// `BuildError#dump`: where to report a failed build of a tap's
    /// formula, its issues on GitHub unless it is fetched from elsewhere.
    /// Homebrew sends those of its own taps to its troubleshooting page;
    /// nextbrew's build of them is not Homebrew's, so it says nothing.
    fn build_failure_report(&self, formula: &nb_core::Formula) -> Option<String> {
        let tap = crate::tap::TapName::parse(formula.third_party_tap()?).ok()?;
        if tap.is_official() {
            return None;
        }
        let remote = self
            .tap_source()
            .ok()
            .and_then(|source| source.taps.remote(&tap));
        Some(if tap.uses_custom_remote(remote.as_deref()) {
            format!(
                "If reporting this issue please do so to (not Homebrew/* repositories):\n  {tap}"
            )
        } else {
            format!(
                "If reporting this issue please do so at (not Homebrew/* repositories):\n  {}/issues",
                tap.default_remote()
            )
        })
    }

    fn backup_existing_source_keg(
        keg_path: &Path,
        formula_name: &str,
        version: &str,
    ) -> Result<Option<PathBuf>, Error> {
        if !keg_path.exists() {
            return Ok(None);
        }

        let backup_path = Self::source_keg_backup_path(keg_path);
        if backup_path.exists() {
            fs::remove_dir_all(&backup_path).map_err(|e| Error::StoreCorruption {
                message: format!(
                    "failed to remove stale source-build backup for '{}@{}': {}",
                    formula_name, version, e
                ),
            })?;
        }

        fs::rename(keg_path, &backup_path).map_err(|e| Error::StoreCorruption {
            message: format!(
                "failed to backup existing keg for '{}@{}': {}",
                formula_name, version, e
            ),
        })?;

        Ok(Some(backup_path))
    }

    fn restore_source_keg_from_backup(
        keg_path: &Path,
        backup_path: &Path,
        formula_name: &str,
        version: &str,
    ) -> Result<(), Error> {
        if keg_path.exists() {
            fs::remove_dir_all(keg_path).map_err(|e| Error::StoreCorruption {
                message: format!(
                    "failed to remove failed source-build output for '{}@{}': {}",
                    formula_name, version, e
                ),
            })?;
        }

        fs::rename(backup_path, keg_path).map_err(|e| Error::StoreCorruption {
            message: format!(
                "failed to restore previous keg for '{}@{}': {}",
                formula_name, version, e
            ),
        })
    }

    fn remove_source_keg_backup(
        backup_path: &Path,
        formula_name: &str,
        version: &str,
    ) -> Result<(), Error> {
        if !backup_path.exists() {
            return Ok(());
        }

        fs::remove_dir_all(backup_path).map_err(|e| Error::StoreCorruption {
            message: format!(
                "failed to remove source-build backup for '{}@{}': {}",
                formula_name, version, e
            ),
        })
    }

    fn source_keg_backup_path(keg_path: &Path) -> PathBuf {
        let backup_suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = keg_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "keg".to_string());

        keg_path.with_file_name(format!("{name}.nb-backup-{backup_suffix}"))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::Installer;

    #[test]
    fn source_keg_backup_can_restore_previous_installation() {
        let tmp = TempDir::new().unwrap();
        let keg_path = tmp.path().join("cellar").join("example").join("1.0.0");
        fs::create_dir_all(&keg_path).unwrap();
        fs::write(keg_path.join("old.txt"), "old").unwrap();

        let backup = Installer::backup_existing_source_keg(&keg_path, "example", "1.0.0").unwrap();
        let backup = backup.expect("backup path should exist");

        assert!(!keg_path.exists());
        assert!(backup.exists());

        fs::create_dir_all(&keg_path).unwrap();
        fs::write(keg_path.join("new.txt"), "new").unwrap();

        Installer::restore_source_keg_from_backup(&keg_path, &backup, "example", "1.0.0").unwrap();

        assert!(keg_path.join("old.txt").exists());
        assert!(!keg_path.join("new.txt").exists());
        assert!(!backup.exists());
    }

    #[test]
    fn backup_existing_source_keg_returns_none_when_keg_is_missing() {
        let tmp = TempDir::new().unwrap();
        let missing_keg = tmp.path().join("cellar").join("example").join("1.0.0");

        let backup =
            Installer::backup_existing_source_keg(&missing_keg, "example", "1.0.0").unwrap();

        assert!(backup.is_none());
    }
}
