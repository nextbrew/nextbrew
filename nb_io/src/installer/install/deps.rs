use std::collections::BTreeSet;

use nb_core::{Error, formula_token};

use super::Installer;

impl Installer {
    /// A formula's runtime dependencies, as `brew deps` lists them: the
    /// whole closure, or with `direct` only the ones it names itself.
    pub async fn dependencies(&self, name: &str, direct: bool) -> Result<BTreeSet<String>, Error> {
        if direct {
            let formula = self.api_client.get_formula(name).await?;
            return Ok(formula.runtime_dependencies().into_iter().collect());
        }
        let closure = self.fetch_all_formulas(&[name.to_string()]).await?;
        Ok(closure.into_keys().filter(|dep| dep != name).collect())
    }

    /// Installed formulae that depend on every one of `names`, as `brew uses
    /// --installed` finds them: directly, or anywhere in their closure with
    /// `recursive`.
    pub async fn installed_dependents(
        &self,
        names: &[String],
        recursive: bool,
    ) -> Result<Vec<String>, Error> {
        // Aliases and old names resolve to the name dependencies are listed by.
        let mut targets = Vec::with_capacity(names.len());
        for name in names {
            targets.push(self.api_client.get_formula(name).await?.name);
        }
        let mut dependents = Vec::new();
        for keg in self.db.list_installed()? {
            if keg.name.starts_with("cask:") || targets.contains(&keg.name) {
                continue;
            }
            let deps = self.dependencies(&keg.name, !recursive).await?;
            let uses = |target: &String| {
                deps.iter()
                    .any(|dep| dep == target || formula_token(dep) == target)
            };
            if targets.iter().all(uses) {
                dependents.push(keg.name);
            }
        }
        Ok(dependents)
    }
}
