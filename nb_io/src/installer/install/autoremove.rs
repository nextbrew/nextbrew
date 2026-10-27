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

        // What was built from source keeps its build dependencies, as in
        // Homebrew's `Utils::Autoremove` (only bottles ignore them).
        let built: HashSet<&str> = installed
            .iter()
            .filter(|keg| keg.store_key.starts_with("source:"))
            .map(|keg| keg.name.as_str())
            .collect();
        let mut needed = HashSet::new();
        let mut wanted = roots;
        while !wanted.is_empty() {
            // Fails as a whole when any lookup fails: removing on a partial
            // view would delete something that is needed.
            let mut build_deps = Vec::new();
            for (name, formula) in self.fetch_all_formulas(&wanted).await? {
                if built.contains(formula.name.as_str()) {
                    build_deps.extend(formula.build_dependencies.iter().cloned());
                }
                needed.insert(name);
                needed.insert(formula.name);
            }
            wanted = build_deps
                .into_iter()
                .filter(|dep| !needed.contains(dep))
                .collect();
        }

        Ok(installed
            .into_iter()
            .filter(|keg| !keg.name.starts_with("cask:") && !keg.on_request)
            .filter(|keg| !needed.contains(&keg.name) && !needed.contains(formula_token(&keg.name)))
            .map(|keg| keg.name)
            .collect())
    }

    /// `brew leaves`: installed formulae that no other installed formula
    /// or cask depends on, sorted.
    pub async fn leaves(&self) -> Result<Vec<String>, Error> {
        let installed = self.db.list_installed()?;
        let (casks, formulae): (Vec<_>, Vec<_>) = installed
            .iter()
            .partition(|keg| keg.name.starts_with("cask:"));
        let names: Vec<String> = formulae.iter().map(|keg| keg.name.clone()).collect();
        let mut used: HashSet<String> = casks
            .iter()
            .flat_map(|keg| self.cask_formula_dependencies(&keg.name, &keg.version))
            .collect();
        for (name, formula) in self.fetch_all_formulas(&names).await? {
            if names.contains(&name) || names.contains(&formula.name) {
                used.extend(formula.runtime_dependencies());
            }
        }
        let mut leaves: Vec<String> = names
            .into_iter()
            .filter(|name| !used.contains(name) && !used.contains(formula_token(name)))
            .collect();
        leaves.sort();
        Ok(leaves)
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
