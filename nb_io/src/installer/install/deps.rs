use std::collections::{BTreeSet, HashMap};

use nb_core::{Error, formula_token};

use super::{Installer, cask_deps};
use crate::installer::cask::for_current_platform;

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

    /// What depends on every one of `names`, as `brew uses` finds it:
    /// formulae and casks, from what is installed or from the whole API,
    /// through direct dependencies or with `recursive` their closure.
    /// Names the API does not know are matched as given and reported.
    pub async fn dependents(&self, names: &[String], scope: UsesScope) -> Result<Uses, Error> {
        // Aliases and old names resolve to the name dependencies are listed by.
        let mut targets = Vec::with_capacity(names.len());
        let mut missing = Vec::new();
        for name in names {
            match self.api_client.get_formula(name).await {
                Ok(formula) => targets.push(formula.name),
                Err(Error::MissingFormula { .. }) => {
                    missing.push(name.clone());
                    targets.push(name.clone());
                }
                Err(e) => return Err(e),
            }
        }
        let uses_all = |deps: &BTreeSet<String>| {
            targets.iter().all(|target| {
                deps.iter()
                    .any(|dep| dep == target || formula_token(dep) == target)
            })
        };
        // brew answers `--installed` from the kegs' whole runtime
        // dependencies, whatever `--recursive` says.
        let formula_closure = scope.recursive || (scope.installed && missing.is_empty());
        let graph = if scope.installed {
            None
        } else {
            Some(
                self.api_client
                    .all_formulas()
                    .await?
                    .into_iter()
                    .map(|formula| {
                        let deps = formula.runtime_dependencies();
                        (formula.name, deps)
                    })
                    .collect::<HashMap<_, _>>(),
            )
        };

        let mut found = Vec::new();
        if scope.formulae {
            match &graph {
                Some(graph) => {
                    for (name, deps) in graph {
                        let deps = if scope.recursive {
                            closure(graph, deps)
                        } else {
                            deps.iter().cloned().collect()
                        };
                        if uses_all(&deps) {
                            found.push(name.clone());
                        }
                    }
                }
                None => {
                    for keg in self.db.list_installed()? {
                        if keg.name.starts_with("cask:") || targets.contains(&keg.name) {
                            continue;
                        }
                        if uses_all(&self.dependencies(&keg.name, !formula_closure).await?) {
                            found.push(keg.name);
                        }
                    }
                }
            }
        }
        if scope.casks && cfg!(target_os = "macos") {
            let casks: Vec<(String, Vec<String>)> = match &graph {
                Some(_) => self
                    .api_client
                    .all_casks()
                    .await?
                    .iter()
                    .filter_map(|cask| {
                        let cask = for_current_platform(cask);
                        let token = cask["token"].as_str()?.to_string();
                        Some((token, cask_deps::names(&cask["depends_on"]["formula"])))
                    })
                    .collect(),
                None => self
                    .db
                    .list_installed()?
                    .into_iter()
                    .filter_map(|keg| {
                        let token = keg.name.strip_prefix("cask:")?.to_string();
                        Some((
                            token,
                            self.cask_formula_dependencies(&keg.name, &keg.version),
                        ))
                    })
                    .collect(),
            };
            for (token, direct) in casks {
                let mut deps: BTreeSet<String> = direct.iter().cloned().collect();
                if scope.recursive {
                    deps = match &graph {
                        Some(graph) => closure(graph, &direct),
                        None => {
                            for dep in &direct {
                                deps.extend(self.dependencies(dep, false).await?);
                            }
                            deps
                        }
                    };
                }
                if uses_all(&deps) {
                    found.push(token);
                }
            }
        }
        found.sort();
        found.dedup();
        Ok(Uses {
            dependents: found,
            missing,
        })
    }
}

/// Which dependents `nb uses` looks at.
#[derive(Debug, Clone, Copy)]
pub struct UsesScope {
    /// Only installed formulae and casks, not the whole API.
    pub installed: bool,
    /// Dependencies of dependencies count too.
    pub recursive: bool,
    pub formulae: bool,
    pub casks: bool,
}

/// What `nb uses` found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Uses {
    /// Formula names and cask tokens, sorted together as brew does.
    pub dependents: Vec<String>,
    /// The asked-for names the API does not know.
    pub missing: Vec<String>,
}

/// Everything `direct` pulls in through `graph`, `direct` included.
fn closure(graph: &HashMap<String, Vec<String>>, direct: &[String]) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut queue: Vec<&String> = direct.iter().collect();
    while let Some(dep) = queue.pop() {
        if seen.insert(dep.clone())
            && let Some(next) = graph.get(formula_token(dep))
        {
            queue.extend(next);
        }
    }
    seen
}
