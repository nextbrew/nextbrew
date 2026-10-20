use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use nb_core::{
    BuildPlan, Error, Formula, InstallMethod, SelectedBottle, formula_token, select_bottle,
};
use tracing::warn;

use super::{InstallPlan, Installer, PlanFailure, PlannedInstall};

impl Installer {
    pub async fn plan(&self, names: &[String]) -> Result<InstallPlan, Error> {
        self.plan_with_options(names, false).await
    }

    pub async fn plan_with_options(
        &self,
        names: &[String],
        build_from_source: bool,
    ) -> Result<InstallPlan, Error> {
        let formulas = self.fetch_all_formulas(names).await?;
        let ordered = nb_core::resolve_closure(names, &formulas)?;

        let roots = root_install_names(names, &formulas);
        let mut items = Vec::with_capacity(ordered.len());
        let mut planned = HashSet::new();
        for requested in ordered {
            let formula = formulas.get(&requested).cloned().unwrap();
            let install_name = install_name_for(&requested, &formula);
            if !planned.insert(install_name.clone()) {
                continue;
            }
            if !roots.contains(&install_name) && self.has_installed_keg(&install_name) {
                continue;
            }
            items.push(self.plan_item(install_name, formula, build_from_source)?);
        }

        rank_like_homebrew(&mut items, names, &formulas);
        Ok(InstallPlan {
            items,
            roots: roots.into_iter().collect(),
        })
    }

    pub async fn plan_best_effort(
        &self,
        names: &[String],
        build_from_source: bool,
    ) -> (InstallPlan, Vec<PlanFailure>) {
        let (formulas, fetch_failures) = self.fetch_all_formulas_best_effort(names).await;
        let mut items = Vec::new();
        let mut failures = Vec::new();
        let mut valid_roots = Vec::new();
        let mut seen_roots = HashSet::new();
        let mut plan_roots = Vec::new();

        for name in names {
            if !seen_roots.insert(name.clone()) {
                continue;
            }

            if let Some(error) = fetch_failures.get(name) {
                failures.push(PlanFailure {
                    name: name.clone(),
                    error: error.clone(),
                });
                continue;
            }

            if !formulas.contains_key(name) {
                failures.push(PlanFailure {
                    name: name.clone(),
                    error: Error::MissingFormula { name: name.clone() },
                });
                continue;
            }

            if let Some(failure) = root_dependency_failure(name, &formulas, &fetch_failures) {
                failures.push(failure);
                continue;
            }

            valid_roots.push(name.clone());
        }

        if !valid_roots.is_empty() {
            match nb_core::resolve_closure(&valid_roots, &formulas) {
                Ok(ordered) => {
                    let roots = root_install_names(&valid_roots, &formulas);
                    plan_roots.extend(roots.iter().cloned());
                    let mut planned = HashSet::new();
                    for requested in ordered {
                        let formula = formulas.get(&requested).cloned().unwrap();
                        let install_name = install_name_for(&requested, &formula);
                        if !planned.insert(install_name.clone()) {
                            continue;
                        }
                        if !roots.contains(&install_name) && self.has_installed_keg(&install_name) {
                            continue;
                        }
                        match self.plan_item(install_name.clone(), formula, build_from_source) {
                            Ok(item) => items.push(item),
                            Err(error) => failures.push(PlanFailure {
                                name: install_name,
                                error,
                            }),
                        }
                    }
                }
                Err(error) => {
                    failures.extend(valid_roots.into_iter().map(|name| PlanFailure {
                        name,
                        error: error.clone(),
                    }));
                }
            }
        }

        rank_like_homebrew(&mut items, names, &formulas);
        (
            InstallPlan {
                items,
                roots: plan_roots,
            },
            failures,
        )
    }

    /// Whether a package is recorded as installed and its keg is on disk, at
    /// any version.
    ///
    /// Dependencies that are already installed are left out of the plan:
    /// installing a package must never upgrade other packages as a side
    /// effect. Only the packages named on the command line are brought to
    /// their latest version. A record whose keg has gone missing doesn't
    /// count, so the dependency is installed again.
    fn has_installed_keg(&self, install_name: &str) -> bool {
        self.db.get_installed(install_name).is_some_and(|keg| {
            self.cellar
                .has_keg(formula_token(install_name), &keg.version)
        })
    }

    fn plan_item(
        &self,
        install_name: String,
        formula: Formula,
        build_from_source: bool,
    ) -> Result<PlannedInstall, Error> {
        crate::installer::cask::check_formula_deprecate_disable(&formula)?;
        check_clt_for_bottle(&formula)?;
        let method = if build_from_source {
            match BuildPlan::from_formula(&formula, &self.prefix) {
                Some(plan) => InstallMethod::Source(plan),
                None => match self.select_pourable_bottle(&formula) {
                    Ok(bottle) => InstallMethod::Bottle(bottle),
                    Err(_) => {
                        return Err(Error::UnsupportedBottle {
                            name: formula.name.clone(),
                        });
                    }
                },
            }
        } else {
            match self.select_pourable_bottle(&formula) {
                Ok(bottle) => InstallMethod::Bottle(bottle),
                Err(_)
                    if refuses_without_bottle(
                        &formula,
                        self.db.get_installed(&install_name).is_some(),
                        refuses_fresh_installs_without_bottles(),
                    ) =>
                {
                    return Err(Error::NoBottleAvailable {
                        name: formula.name.clone(),
                    });
                }
                Err(_) => match BuildPlan::from_formula(&formula, &self.prefix) {
                    Some(plan) => InstallMethod::Source(plan),
                    None => {
                        return Err(Error::UnsupportedBottle {
                            name: formula.name.clone(),
                        });
                    }
                },
            }
        };

        check_requirements(&formula, matches!(method, InstallMethod::Source(_)))?;

        Ok(PlannedInstall {
            install_name,
            formula,
            method,
            link_keg_only: false,
            link_overwrite: Vec::new(),
            link_rank: 0,
            link_later: Vec::new(),
        })
    }

    /// Select a bottle that can be poured into this prefix.
    ///
    /// On macOS, bottles pinned to a shorter Cellar than ours (Intel bottles
    /// built for `/usr/local`) have hardcoded paths that can't be rewritten,
    /// so they're treated as unavailable and the formula is built from source
    /// instead, as Homebrew does for a non-default prefix.
    fn select_pourable_bottle(&self, formula: &Formula) -> Result<SelectedBottle, Error> {
        let bottle = select_bottle(formula)?;
        if formula.pour_bottle_only_if.as_deref() == Some("default_prefix")
            && self.prefix != Path::new(DEFAULT_PREFIX)
        {
            warn!(
                formula = %formula.name,
                "the bottle (and many others) needs to be installed into {DEFAULT_PREFIX}"
            );
            return Err(Error::UnsupportedBottle {
                name: formula.name.clone(),
            });
        }
        if cfg!(target_os = "macos") && !bottle.is_pourable_into(&self.prefix) {
            warn!(
                formula = %formula.name,
                cellar = bottle.cellar.as_deref().unwrap_or_default(),
                prefix = %self.prefix.display(),
                "bottle can't be relocated to this prefix; it will be built from source if possible"
            );
            return Err(Error::UnsupportedBottle {
                name: formula.name.clone(),
            });
        }
        Ok(bottle)
    }

    async fn fetch_all_formulas_best_effort(
        &self,
        names: &[String],
    ) -> (BTreeMap<String, Formula>, HashMap<String, Error>) {
        let mut formulas = BTreeMap::new();
        let mut failures = HashMap::new();
        let mut fetched: HashSet<String> = HashSet::new();
        let mut to_fetch: Vec<String> = names.to_vec();

        while !to_fetch.is_empty() {
            let batch: Vec<String> = to_fetch
                .drain(..)
                .filter(|n| !fetched.contains(n))
                .collect();

            if batch.is_empty() {
                break;
            }

            for n in &batch {
                fetched.insert(n.clone());
            }

            let futures: Vec<_> = batch
                .iter()
                .map(|n| self.api_client.get_formula(n))
                .collect();

            let results = futures::future::join_all(futures).await;

            for (i, result) in results.into_iter().enumerate() {
                let fetch_name = batch[i].clone();
                let formula = match result {
                    Ok(f) => f,
                    Err(error) => {
                        failures.insert(fetch_name, error);
                        continue;
                    }
                };

                if select_bottle(&formula).is_err() && !formula.has_source_url() {
                    warn!(
                        formula = %formula.name,
                        "skipping formula with no bottle or source available for this platform"
                    );
                    failures.insert(
                        fetch_name,
                        Error::UnsupportedBottle {
                            name: formula.name.clone(),
                        },
                    );
                    continue;
                }

                for dep in formula.runtime_dependencies() {
                    if !fetched.contains(&dep)
                        && !to_fetch.contains(&dep)
                        && !failures.contains_key(&dep)
                    {
                        to_fetch.push(dep);
                    }
                }

                formulas.insert(fetch_name, formula);
            }
        }

        (formulas, failures)
    }

    pub(super) async fn fetch_all_formulas(
        &self,
        names: &[String],
    ) -> Result<BTreeMap<String, Formula>, Error> {
        use std::collections::HashSet;

        let mut formulas = BTreeMap::new();
        let mut fetched: HashSet<String> = HashSet::new();
        let mut to_fetch: Vec<String> = names.to_vec();

        while !to_fetch.is_empty() {
            let batch: Vec<String> = to_fetch
                .drain(..)
                .filter(|n| !fetched.contains(n))
                .collect();

            if batch.is_empty() {
                break;
            }

            for n in &batch {
                fetched.insert(n.clone());
            }

            let futures: Vec<_> = batch
                .iter()
                .map(|n| self.api_client.get_formula(n))
                .collect();

            let results = futures::future::join_all(futures).await;

            for (i, result) in results.into_iter().enumerate() {
                let formula = match result {
                    Ok(f) => f,
                    Err(e) => return Err(e),
                };

                if select_bottle(&formula).is_err() && !formula.has_source_url() {
                    warn!(
                        formula = %formula.name,
                        "skipping formula with no bottle or source available for this platform"
                    );
                    continue;
                }

                for dep in formula.runtime_dependencies() {
                    if !fetched.contains(&dep) && !to_fetch.contains(&dep) {
                        to_fetch.push(dep);
                    }
                }

                formulas.insert(batch[i].clone(), formula);
            }
        }

        Ok(formulas)
    }
}

/// The name to record an install under: the formula's own name, as its keg
/// in the Cellar has it. A formula reached through an alias or old name,
/// like `nb install python` (`python@3.14`), or through its tap,
/// `hashicorp/tap/terraform` (`terraform`), is recorded under that name;
/// the tap is recorded beside it.
fn install_name_for(_requested: &str, formula: &Formula) -> String {
    formula.name.clone()
}

/// The install names of the packages the caller asked for, as opposed to the
/// dependencies pulled in for them.
fn root_install_names(names: &[String], formulas: &BTreeMap<String, Formula>) -> HashSet<String> {
    names
        .iter()
        .filter_map(|name| {
            formulas
                .get(name)
                .map(|formula| install_name_for(name, formula))
        })
        .collect()
}

fn root_dependency_failure(
    root: &str,
    formulas: &BTreeMap<String, Formula>,
    fetch_failures: &HashMap<String, Error>,
) -> Option<PlanFailure> {
    let mut seen = HashSet::new();
    let mut stack = vec![root.to_string()];

    while let Some(name) = stack.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }

        let Some(formula) = formulas.get(&name) else {
            continue;
        };

        for dep in formula.runtime_dependencies() {
            if let Some(error) = fetch_failures.get(&dep) {
                return Some(PlanFailure {
                    name: root.to_string(),
                    error: Error::ExecutionError {
                        message: format!("dependency '{dep}' could not be planned: {error}"),
                    },
                });
            }

            if formulas.contains_key(&dep) {
                stack.push(dep);
            }
        }
    }

    None
}

/// Rank items in the order `brew install` would install them: each name
/// asked for, in order, after its dependencies (depth first, as declared).
fn rank_like_homebrew(
    items: &mut [PlannedInstall],
    names: &[String],
    formulas: &BTreeMap<String, Formula>,
) {
    fn visit(
        name: &str,
        formulas: &BTreeMap<String, Formula>,
        seen: &mut HashSet<String>,
        order: &mut Vec<String>,
    ) {
        let Some(formula) = formulas.get(name) else {
            return;
        };
        if !seen.insert(formula.name.clone()) {
            return;
        }
        for dep in formula.runtime_dependencies() {
            visit(&dep, formulas, seen, order);
        }
        order.push(formula.name.clone());
    }
    let mut seen = HashSet::new();
    let mut order = Vec::new();
    for name in names {
        visit(name, formulas, &mut seen, &mut order);
    }
    for item in items {
        item.link_rank = order
            .iter()
            .position(|name| *name == item.formula.name)
            .unwrap_or(usize::MAX);
    }
}

/// `Homebrew::DEFAULT_PREFIX`, the only prefix `pour_bottle? only_if:
/// :default_prefix` bottles work in.
const DEFAULT_PREFIX: &str = if cfg!(target_os = "linux") {
    "/home/linuxbrew/.linuxbrew"
} else if cfg!(target_arch = "aarch64") {
    "/opt/homebrew"
} else {
    "/usr/local"
};

/// `FormulaInstaller#check_install_sanity`: Homebrew refuses a
/// homebrew/core formula with no bottle for this platform unless asked to
/// build it (`--build-from-source`) or some version of it is installed (an
/// upgrade or reinstall builds it). Homebrew does so only in its default
/// prefix; nextbrew pours bottles into its own prefix the same way, so it
/// refuses there too.
fn refuses_without_bottle(formula: &Formula, installed: bool, fresh_install_refused: bool) -> bool {
    fresh_install_refused
        && !installed
        && formula.third_party_tap().is_none()
        && select_bottle(formula).is_err()
}

/// `FormulaInstaller#fresh_install?`'s platforms: Apple Silicon on a
/// supported macOS, and Linux.
#[cfg(target_os = "macos")]
fn refuses_fresh_installs_without_bottles() -> bool {
    cfg!(target_arch = "aarch64")
        && nb_core::macos_major_version()
            .is_some_and(|major| major >= super::diagnostic::MACOS_OLDEST_SUPPORTED)
}

#[cfg(not(target_os = "macos"))]
fn refuses_fresh_installs_without_bottles() -> bool {
    cfg!(target_os = "linux")
}

/// `FormulaInstaller#check_requirements`: refuse a formula whose
/// `depends_on macos:` / `arch:` / `xcode:` / `:linux` this system does not
/// meet. Only for formulae read from a tap: Homebrew does not load the
/// requirements of the formulae it reads from the API, and installs them
/// regardless.
fn check_requirements(formula: &Formula, building: bool) -> Result<(), Error> {
    use nb_core::formula::requirements::unsatisfied_requirements;
    if formula.requirements.is_empty() || formula.third_party_tap().is_none() {
        return Ok(());
    }
    let messages = unsatisfied_requirements(formula, building, current_system());
    if messages.is_empty() {
        return Ok(());
    }
    Err(Error::UnsatisfiedRequirements { messages })
}

/// This system, as requirements see it; looked at once, when first needed.
fn current_system() -> &'static nb_core::formula::requirements::System {
    static SYSTEM: std::sync::OnceLock<nb_core::formula::requirements::System> =
        std::sync::OnceLock::new();
    SYSTEM.get_or_init(|| nb_core::formula::requirements::System {
        #[cfg(target_os = "macos")]
        macos: nb_core::macos_major_version().map(|major| (major, 0)),
        #[cfg(not(target_os = "macos"))]
        macos: None,
        arm: cfg!(target_arch = "aarch64"),
        xcode: if cfg!(target_os = "macos") {
            xcode_version()
        } else {
            None
        },
    })
}

/// `MacOS::Xcode.installed?` and `.version`: the Xcode.app `xcode-select`
/// points into, or /Applications/Xcode.app, and its version (empty when
/// unreadable).
fn xcode_version() -> Option<String> {
    let selected = std::process::Command::new("/usr/bin/xcode-select")
        .arg("-p")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .and_then(|path| {
            path.find(".app/")
                .map(|end| std::path::PathBuf::from(&path[..end + 4]))
        });
    let app = selected
        .or_else(|| Some(std::path::PathBuf::from("/Applications/Xcode.app")))
        .filter(|app| app.join("Contents/Developer").is_dir())?;
    let version = std::process::Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", "Print :CFBundleShortVersionString"])
        .arg(app.join("Contents/version.plist"))
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();
    Some(version)
}

/// `pour_bottle? only_if: :clt_installed` on macOS: Homebrew pours such a
/// bottle only with the Command Line Tools installed, and a build from
/// source needs them too, so their absence ends the install.
fn check_clt_for_bottle(formula: &Formula) -> Result<(), Error> {
    let clt = Path::new("/Library/Developer/CommandLineTools/usr/bin/clang").exists();
    check_clt(formula, !cfg!(target_os = "macos") || clt)
}

fn check_clt(formula: &Formula, clt_installed: bool) -> Result<(), Error> {
    if clt_installed || formula.pour_bottle_only_if.as_deref() != Some("clt_installed") {
        return Ok(());
    }
    Err(Error::InvalidArgument {
        message: format!(
            "{name}: the bottle needs the Xcode Command Line Tools to be installed at /Library/Developer/CommandLineTools.
Development tools provided by Xcode.app are not sufficient.

You can install the Xcode Command Line Tools, if desired, with:
    xcode-select --install

If no compatible bottle is available, you can try to install from source with:
  brew install --build-from-source {name}",
            name = formula.name
        ),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_bottle_for_the_clt_needs_the_clt() {
        let mut formula: nb_core::Formula = serde_json::from_str(include_str!(
            "../../../../nb_core/fixtures/formula_foo.json"
        ))
        .unwrap();
        assert!(super::check_clt(&formula, false).is_ok());
        formula.pour_bottle_only_if = Some("clt_installed".to_string());
        assert!(super::check_clt(&formula, true).is_ok());
        let err = super::check_clt(&formula, false).unwrap_err().to_string();
        assert!(
            err.contains("the bottle needs the Xcode Command Line Tools"),
            "{err}"
        );
    }

    use std::fs;

    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::cellar::Cellar;
    use crate::installer::install::test_support::*;
    use crate::network::api::ApiClient;
    use crate::storage::blob::BlobCache;
    use crate::storage::db::Database;
    use crate::storage::store::Store;
    use crate::{Installer, Linker};

    #[test]
    fn refuses_a_core_formula_without_a_bottle_unless_installed() {
        let mut formula: nb_core::Formula = serde_json::from_str(
            r#"{"name": "nobottle", "versions": {"stable": "1.0"}, "dependencies": [],
                "bottle": {"stable": {"files": {}}}}"#,
        )
        .unwrap();
        assert!(super::refuses_without_bottle(&formula, false, true));
        assert!(!super::refuses_without_bottle(&formula, true, true));
        assert!(!super::refuses_without_bottle(&formula, false, false));
        formula.tap = Some("someone/tools".to_string());
        assert!(!super::refuses_without_bottle(&formula, false, true));
        formula.tap = Some("homebrew/core".to_string());
        assert!(super::refuses_without_bottle(&formula, false, true));

        let error = nb_core::Error::NoBottleAvailable {
            name: "nobottle".to_string(),
        };
        assert_eq!(
            error.to_string(),
            "nobottle: no bottle available!\n\
             If no compatible bottle is available, you can try to install from source with:\n  \
             nb install --build-from-source nobottle"
        );
    }

    /// A tap formula with no bottle is built from its `.rb`, as Homebrew
    /// builds it.
    #[tokio::test]
    async fn falls_back_to_source_when_no_bottle() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let formula_json = r#"{
            "name": "nobottle",
            "tap": "someone/tools",
            "versions": { "stable": "1.0.0" },
            "dependencies": [],
            "build_dependencies": ["pkgconf"],
            "urls": {
                "stable": {
                    "url": "https://example.com/nobottle-1.0.0.tar.gz",
                    "checksum": "abc123"
                }
            },
            "ruby_source_path": "Formula/n/nobottle.rb",
            "bottle": { "stable": { "files": {} } }
        }"#;

        Mock::given(method("GET"))
            .and(path("/formula/nobottle.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/nb.sqlite3")).unwrap();

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let plan = installer.plan(&["nobottle".to_string()]).await.unwrap();

        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].formula.name, "nobottle");
        assert!(matches!(
            plan.items[0].method,
            nb_core::InstallMethod::Source(_)
        ));

        if let nb_core::InstallMethod::Source(ref bp) = plan.items[0].method {
            assert_eq!(bp.source_url, "https://example.com/nobottle-1.0.0.tar.gz");
            assert_eq!(bp.formula_name, "nobottle");
            assert_eq!(bp.build_dependencies, vec!["pkgconf"]);
        }
    }

    /// An installer backed by a mock API serving each `(path name, formula
    /// JSON)`, like an alias whose JSON is the formula it points to.
    async fn installer_serving(
        mock_server: &MockServer,
        tmp: &TempDir,
        formulas: &[(&str, String)],
    ) -> Installer {
        for (name, json) in formulas {
            Mock::given(method("GET"))
                .and(path(format!("/formula/{name}.json")))
                .respond_with(ResponseTemplate::new(200).set_body_string(json))
                .mount(mock_server)
                .await;
        }

        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();
        Installer::new(
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap(),
            BlobCache::new(&root.join("cache")).unwrap(),
            Store::new(&root).unwrap(),
            Cellar::new(&root).unwrap(),
            Linker::new(&prefix).unwrap(),
            Database::open(&root.join("db/nb.sqlite3")).unwrap(),
            prefix,
            root.join("locks"),
        )
    }

    fn bottled_formula(name: &str, dependencies: &[&str]) -> String {
        format!(
            r#"{{
                "name": "{name}",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": {deps:?},
                "bottle": {{ "stable": {{ "files": {{
                    "{tag}": {{ "url": "https://example.com/{name}.tar.gz", "sha256": "aabbccdd" }}
                }} }} }}
            }}"#,
            deps = dependencies,
            tag = get_test_bottle_tag(),
        )
    }

    #[tokio::test]
    async fn aliases_are_planned_under_the_formula_name() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("node", bottled_formula("node", &["python"])),
                // The API resolves the `python` alias to python@3.14.
                ("python", bottled_formula("python@3.14", &[])),
            ],
        )
        .await;

        let plan = installer.plan(&["node".to_string()]).await.unwrap();

        let names: Vec<_> = plan.items.iter().map(|i| i.install_name.as_str()).collect();
        assert_eq!(names, ["python@3.14", "node"]);
    }

    #[tokio::test]
    async fn alias_and_formula_name_are_planned_once() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("python", bottled_formula("python@3.14", &[])),
                ("python@3.14", bottled_formula("python@3.14", &[])),
            ],
        )
        .await;

        let plan = installer
            .plan(&["python".to_string(), "python@3.14".to_string()])
            .await
            .unwrap();

        let names: Vec<_> = plan.items.iter().map(|i| i.install_name.as_str()).collect();
        assert_eq!(names, ["python@3.14"]);
    }

    /// Record `name` as installed at `version`, with its keg on disk.
    fn mark_installed(installer: &mut Installer, name: &str, version: &str) {
        fs::create_dir_all(installer.keg_path(name, version)).unwrap();
        let tx = installer.db.transaction().unwrap();
        tx.record_install(name, version, "oldsha").unwrap();
        tx.commit().unwrap();
    }

    #[tokio::test]
    async fn installed_dependencies_are_not_upgraded() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let mut installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("nmap", bottled_formula("nmap", &["openssl@3", "libssh2"])),
                ("openssl@3", bottled_formula("openssl@3", &[])),
                ("libssh2", bottled_formula("libssh2", &["openssl@3"])),
            ],
        )
        .await;
        // An older openssl@3 is installed; the API now offers 1.0.0.
        mark_installed(&mut installer, "openssl@3", "0.9.0");

        let plan = installer.plan(&["nmap".to_string()]).await.unwrap();

        let names: Vec<_> = plan.items.iter().map(|i| i.install_name.as_str()).collect();
        assert_eq!(names, ["libssh2", "nmap"]);
    }

    #[tokio::test]
    async fn best_effort_plan_does_not_upgrade_installed_dependencies() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let mut installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("nmap", bottled_formula("nmap", &["openssl@3"])),
                ("openssl@3", bottled_formula("openssl@3", &[])),
            ],
        )
        .await;
        mark_installed(&mut installer, "openssl@3", "0.9.0");

        let (plan, failures) = installer
            .plan_best_effort(&["nmap".to_string()], false)
            .await;

        assert!(failures.is_empty());
        let names: Vec<_> = plan.items.iter().map(|i| i.install_name.as_str()).collect();
        assert_eq!(names, ["nmap"]);
    }

    #[tokio::test]
    async fn explicitly_requested_installed_package_is_still_planned() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let mut installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("nmap", bottled_formula("nmap", &["openssl@3"])),
                ("openssl@3", bottled_formula("openssl@3", &[])),
            ],
        )
        .await;
        mark_installed(&mut installer, "openssl@3", "0.9.0");

        let plan = installer
            .plan(&["nmap".to_string(), "openssl@3".to_string()])
            .await
            .unwrap();

        let names: Vec<_> = plan.items.iter().map(|i| i.install_name.as_str()).collect();
        assert_eq!(names, ["openssl@3", "nmap"]);
    }

    #[tokio::test]
    async fn dependency_with_missing_keg_is_reinstalled() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let mut installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("nmap", bottled_formula("nmap", &["openssl@3"])),
                ("openssl@3", bottled_formula("openssl@3", &[])),
            ],
        )
        .await;
        mark_installed(&mut installer, "openssl@3", "0.9.0");
        fs::remove_dir_all(installer.keg_path("openssl@3", "0.9.0")).unwrap();

        let plan = installer.plan(&["nmap".to_string()]).await.unwrap();

        let names: Vec<_> = plan.items.iter().map(|i| i.install_name.as_str()).collect();
        assert_eq!(names, ["openssl@3", "nmap"]);
    }

    #[tokio::test]
    async fn prefers_bottle_over_source() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "hasboth",
                "versions": {{ "stable": "2.0.0" }},
                "dependencies": [],
                "urls": {{
                    "stable": {{
                        "url": "https://example.com/hasboth-2.0.0.tar.gz",
                        "checksum": "def456"
                    }}
                }},
                "ruby_source_path": "Formula/h/hasboth.rb",
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "https://example.com/hasboth.bottle.tar.gz",
                                "sha256": "aabbccdd"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag
        );

        Mock::given(method("GET"))
            .and(path("/formula/hasboth.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/nb.sqlite3")).unwrap();

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let plan = installer.plan(&["hasboth".to_string()]).await.unwrap();

        assert_eq!(plan.items.len(), 1);
        assert!(matches!(
            plan.items[0].method,
            nb_core::InstallMethod::Bottle(_)
        ));
    }

    #[tokio::test]
    async fn errors_when_no_bottle_and_no_source() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let formula_json = r#"{
            "name": "nothing",
            "versions": { "stable": "1.0.0" },
            "dependencies": [],
            "bottle": { "stable": { "files": {} } }
        }"#;

        Mock::given(method("GET"))
            .and(path("/formula/nothing.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/nb.sqlite3")).unwrap();

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let result = installer.plan(&["nothing".to_string()]).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            nb_core::Error::MissingFormula { .. }
        ));
    }

    #[tokio::test]
    async fn plan_best_effort_keeps_valid_formula_when_another_is_missing() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "goodpkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/goodpkg-1.0.0.{}.bottle.tar.gz",
                                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag
        );

        Mock::given(method("GET"))
            .and(path("/formula/goodpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/missingpkg.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/nb.sqlite3")).unwrap();

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let names = vec!["goodpkg".to_string(), "missingpkg".to_string()];
        let (plan, failures) = installer.plan_best_effort(&names, false).await;

        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].install_name, "goodpkg");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "missingpkg");
        assert!(matches!(
            failures[0].error,
            nb_core::Error::MissingFormula { .. }
        ));
    }
}
