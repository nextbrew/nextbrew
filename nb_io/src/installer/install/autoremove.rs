use std::collections::HashSet;

use nb_core::{Error, formula_token};
use serde_json::Value;

use super::Installer;
use super::cask_app::CASK_JSON;
use crate::installer::cask::for_current_platform;

impl Installer {
    /// Formulae installed as dependencies that no requested formula and no
    /// cask needs any more, as `brew autoremove` finds them.
    ///
    // ponytail: dependencies come from the current API, not from what each
    // keg was built against (bottles carry no receipt). A dependency dropped
    // upstream counts as unneeded even if an old keg still links it; record
    // runtime deps at install time if that bites.
    pub async fn unneeded_formulae(&self) -> Result<Vec<String>, Error> {
        let installed = self.db.list_installed()?;
        let mut roots = Vec::new();
        for keg in &installed {
            if keg.name.starts_with("cask:") {
                roots.extend(self.cask_formula_dependencies(&keg.name, &keg.version));
            } else if keg.on_request {
                roots.push(keg.name.clone());
            }
        }

        let mut needed = HashSet::new();
        if !roots.is_empty() {
            // Fails as a whole when any lookup fails: removing on a partial
            // view would delete something that is needed.
            for (name, formula) in self.fetch_all_formulas(&roots).await? {
                needed.insert(name);
                needed.insert(formula.name);
            }
        }

        Ok(installed
            .into_iter()
            .filter(|keg| !keg.name.starts_with("cask:") && !keg.on_request)
            .filter(|keg| !needed.contains(&keg.name) && !needed.contains(formula_token(&keg.name)))
            .map(|keg| keg.name)
            .collect())
    }

    /// A cask's `depends_on formula:`, from the JSON saved in its keg. Casks
    /// installed before nextbrew tracked requests have no unrequested
    /// dependencies, so a keg without it needs nothing here.
    pub(super) fn cask_formula_dependencies(&self, name: &str, version: &str) -> Vec<String> {
        let keg = self.cellar.keg_path(formula_token(name), version);
        std::fs::read(keg.join(CASK_JSON))
            .ok()
            .and_then(|json| serde_json::from_slice::<Value>(&json).ok())
            .map(|json| {
                super::cask_deps::names(&for_current_platform(&json)["depends_on"]["formula"])
            })
            .unwrap_or_default()
    }
}
