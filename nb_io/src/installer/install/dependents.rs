//! What stops an uninstall, as `brew uninstall` checks it without
//! `--ignore-dependencies`: installed formulae and casks that still need a
//! formula being removed (`InstalledDependents.find_some_installed_dependents`),
//! and installed casks that need a cask being removed
//! (`Cask::Uninstall.check_dependent_casks`).

use std::collections::BTreeSet;

use nb_core::{Error, formula_token};
use serde_json::Value;

use super::Installer;
use super::cask_app::CASK_JSON;
use crate::installer::cask::for_current_platform;

/// Packages being removed that others still need, and those others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredBy {
    /// What is needed: keg paths for formulae, tokens for casks, as
    /// Homebrew names them.
    pub required: Vec<String>,
    /// What needs them, sorted.
    pub dependents: Vec<String>,
}

impl RequiredBy {
    /// `DependentsMessage#output`, with `named_args` as given.
    pub fn message(&self, named_args: &[String]) -> String {
        let one = self.required.len() == 1;
        format!(
            "Refusing to uninstall {}\n\
             because {} {} required by {}, which {} currently installed.\n\
             You can override this and force removal with:\n  \
             nb uninstall --ignore-dependencies {}",
            to_sentence(&self.required),
            if one { "it" } else { "they" },
            if one { "is" } else { "are" },
            to_sentence(&self.dependents),
            if self.dependents.len() == 1 {
                "is"
            } else {
                "are"
            },
            named_args.join(" ")
        )
    }
}

/// `Utils::Text.to_sentence`.
fn to_sentence(values: &[String]) -> String {
    match values {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn push_unique(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}

impl Installer {
    /// Installed packages outside `names` that need a formula in `names`,
    /// through their runtime dependencies (a cask's through its
    /// `depends_on formula:` and theirs). `names` are install names, casks
    /// as `cask:<token>`; the casks among them are not checked here.
    pub async fn formula_dependents(&self, names: &[String]) -> Result<Option<RequiredBy>, Error> {
        let removing: Vec<&str> = names
            .iter()
            .filter(|name| !name.starts_with("cask:"))
            .map(|name| formula_token(name))
            .collect();
        if removing.is_empty() {
            return Ok(None);
        }

        let mut required = Vec::new();
        let mut dependents = BTreeSet::new();
        for keg in self.db.list_installed()? {
            if names.contains(&keg.name) || removing.contains(&formula_token(&keg.name)) {
                continue;
            }
            let (dependent, roots) = match keg.name.strip_prefix("cask:") {
                Some(token) => (
                    token.to_string(),
                    self.cask_formula_dependencies(&keg.name, &keg.version),
                ),
                None => (keg.name.clone(), vec![keg.name.clone()]),
            };
            // A cask needs its formulae themselves as well as their closure.
            let mut needs: BTreeSet<String> = if keg.name.starts_with("cask:") {
                roots.iter().cloned().collect()
            } else {
                BTreeSet::new()
            };
            for root in &roots {
                match self.dependencies(root, false).await {
                    Ok(deps) => needs.extend(deps),
                    // A formula the API no longer has: nothing to go on,
                    // as Homebrew skips what it cannot load.
                    Err(Error::MissingFormula { .. }) => {}
                    Err(e) => return Err(e),
                }
            }
            let mut found = false;
            for dep in needs {
                let token = formula_token(&dep);
                if !removing.contains(&token) {
                    continue;
                }
                if let Some(installed) = self.db.get_installed(token) {
                    let path = self.cellar.keg_path(token, &installed.version);
                    push_unique(&mut required, path.display().to_string());
                    found = true;
                }
            }
            if found {
                dependents.insert(dependent);
            }
        }
        Ok((!required.is_empty()).then(|| RequiredBy {
            required,
            dependents: dependents.into_iter().collect(),
        }))
    }

    /// Installed casks outside `names` whose `depends_on cask:`, followed
    /// through the installed casks, reaches a cask in `names`.
    pub fn cask_dependents(&self, names: &[String]) -> Result<Option<RequiredBy>, Error> {
        let removing: Vec<&str> = names
            .iter()
            .filter_map(|name| name.strip_prefix("cask:"))
            .collect();
        if removing.is_empty() {
            return Ok(None);
        }
        let installed = self.db.list_installed()?;
        let cask_deps = |token: &str| -> Vec<String> {
            installed
                .iter()
                .find(|keg| keg.name.strip_prefix("cask:") == Some(token))
                .and_then(|keg| {
                    let path = self.cellar.keg_path(token, &keg.version).join(CASK_JSON);
                    let json: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
                    Some(super::cask_deps::names(
                        &for_current_platform(&json)["depends_on"]["cask"],
                    ))
                })
                .unwrap_or_default()
        };

        let mut required = Vec::new();
        let mut dependents = Vec::new();
        for keg in &installed {
            let Some(token) = keg.name.strip_prefix("cask:") else {
                continue;
            };
            if removing.contains(&token) {
                continue;
            }
            let mut seen = BTreeSet::new();
            let mut queue = cask_deps(token);
            while let Some(dep) = queue.pop() {
                if seen.insert(dep.clone()) {
                    queue.extend(cask_deps(&dep));
                }
            }
            let found: Vec<&str> = removing
                .iter()
                .copied()
                .filter(|r| seen.contains(*r))
                .collect();
            if found.is_empty() {
                continue;
            }
            for r in found {
                push_unique(&mut required, r.to_string());
            }
            dependents.push(token.to_string());
        }
        dependents.sort();
        Ok((!required.is_empty()).then_some(RequiredBy {
            required,
            dependents,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Linker;
    use crate::cellar::Cellar;
    use crate::network::api::ApiClient;
    use crate::storage::blob::BlobCache;
    use crate::storage::db::Database;
    use crate::storage::store::Store;
    use tempfile::TempDir;

    /// Casks installed with the `depends_on cask:` of each.
    fn installer_with_casks(tmp: &TempDir, casks: &[(&str, &[&str])]) -> Installer {
        let root = tmp.path().join("nextbrew");
        std::fs::create_dir_all(root.join("db")).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let mut db = Database::open(&root.join("db/nb.sqlite3")).unwrap();
        let tx = db.transaction().unwrap();
        for (token, deps) in casks {
            let keg = cellar.keg_path(token, "1.0");
            std::fs::create_dir_all(&keg).unwrap();
            let json = serde_json::json!({"token": token, "depends_on": {"cask": deps}});
            std::fs::write(keg.join(CASK_JSON), json.to_string()).unwrap();
            tx.record_install(&format!("cask:{token}"), "1.0", "")
                .unwrap();
        }
        tx.commit().unwrap();
        Installer::new(
            ApiClient::with_base_url("http://127.0.0.1:9/formula".to_string()).unwrap(),
            BlobCache::new(&root.join("cache")).unwrap(),
            Store::new(&root).unwrap(),
            cellar,
            Linker::new(&tmp.path().join("prefix")).unwrap(),
            db,
            tmp.path().join("prefix"),
            root.join("locks"),
        )
    }

    #[test]
    fn a_cask_needed_by_another_directly_or_through_a_third_is_kept() {
        let tmp = TempDir::new().unwrap();
        let installer = installer_with_casks(
            &tmp,
            &[
                ("app", &["helper"]),
                ("helper", &["base"]),
                ("base", &[]),
                ("other", &[]),
            ],
        );
        let names = |tokens: &[&str]| -> Vec<String> {
            tokens.iter().map(|t| format!("cask:{t}")).collect()
        };

        let blocked = installer
            .cask_dependents(&names(&["base"]))
            .unwrap()
            .unwrap();
        assert_eq!(blocked.required, ["base"]);
        assert_eq!(blocked.dependents, ["app", "helper"]);

        let blocked = installer
            .cask_dependents(&names(&["helper", "base"]))
            .unwrap()
            .unwrap();
        assert_eq!(blocked.dependents, ["app"]);

        assert!(
            installer
                .cask_dependents(&names(&["app", "helper", "base"]))
                .unwrap()
                .is_none()
        );
        assert!(
            installer
                .cask_dependents(&names(&["other"]))
                .unwrap()
                .is_none()
        );
        assert!(
            installer
                .cask_dependents(&["jq".to_string()])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_message_reads_as_homebrews() {
        let one = RequiredBy {
            required: vec!["/opt/nextbrew/Cellar/oniguruma/6.9.10".into()],
            dependents: vec!["jq".into()],
        };
        assert_eq!(
            one.message(&["oniguruma".into()]),
            "Refusing to uninstall /opt/nextbrew/Cellar/oniguruma/6.9.10\n\
             because it is required by jq, which is currently installed.\n\
             You can override this and force removal with:\n  \
             nb uninstall --ignore-dependencies oniguruma"
        );

        let many = RequiredBy {
            required: vec!["a".into(), "b".into()],
            dependents: vec!["x".into(), "y".into(), "z".into()],
        };
        assert!(many.message(&[]).starts_with(
            "Refusing to uninstall a and b\n\
             because they are required by x, y and z, which are currently installed."
        ));
    }
}
