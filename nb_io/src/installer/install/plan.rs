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
        let mut formulas = self.fetch_all_formulas(names).await?;
        let ordered = nb_core::resolve_closure(names, &formulas)?;

        let roots = root_install_names(names, &formulas);
        for formula in requested_formulae(names, &formulas) {
            crate::installer::cask::check_formula_deprecate_disable(formula)?;
        }
        let mut items = Vec::with_capacity(ordered.len());
        let mut planned = HashSet::new();
        for requested in ordered {
            let formula = formulas.get(&requested).cloned().unwrap();
            let install_name = install_name_for(&requested, &formula);
            if !planned.insert(install_name.clone()) {
                continue;
            }
            let root = roots.contains(&install_name);
            if !root && self.has_installed_keg(&install_name) {
                continue;
            }
            // As `--build-from-source` does in Homebrew: what was named is
            // built, its dependencies are poured from bottles where they can be.
            items.push(self.plan_item(install_name, formula, build_from_source && root, !root)?);
        }
        self.plan_build_dependencies(&mut items, &mut formulas)
            .await?;

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
        let (mut formulas, fetch_failures) = self.fetch_all_formulas_best_effort(names).await;
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

        let lifecycle_failures: HashSet<String> = requested_formulae(&valid_roots, &formulas)
            .into_iter()
            .filter_map(|formula| {
                let error =
                    crate::installer::cask::check_formula_deprecate_disable(formula).err()?;
                failures.push(PlanFailure {
                    name: formula.name.clone(),
                    error,
                });
                Some(formula.name.clone())
            })
            .collect();
        valid_roots.retain(|name| {
            formulas
                .get(name)
                .is_none_or(|formula| !lifecycle_failures.contains(&formula.name))
        });

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
                        let root = roots.contains(&install_name);
                        if !root && self.has_installed_keg(&install_name) {
                            continue;
                        }
                        match self.plan_item(
                            install_name.clone(),
                            formula,
                            build_from_source && root,
                            !root,
                        ) {
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

        if let Err(error) = self
            .plan_build_dependencies(&mut items, &mut formulas)
            .await
        {
            // What needs building can't be built without them.
            let (building, rest): (Vec<_>, Vec<_>) = items
                .into_iter()
                .partition(|item| matches!(item.method, InstallMethod::Source(_)));
            items = rest;
            failures.extend(building.into_iter().map(|item| PlanFailure {
                name: item.install_name,
                error: error.clone(),
            }));
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

    /// `FormulaInstaller#compute_dependencies` for a build: Homebrew installs
    /// the build dependencies of what it builds from source (from bottles,
    /// with their own dependencies) before building it, and leaves them
    /// installed as dependencies. They go first in the plan.
    async fn plan_build_dependencies(
        &self,
        items: &mut Vec<PlannedInstall>,
        formulas: &mut BTreeMap<String, Formula>,
    ) -> Result<(), Error> {
        let mut checked = HashSet::new();
        loop {
            let planned: HashSet<String> =
                items.iter().map(|item| item.install_name.clone()).collect();
            let missing: Vec<String> = items
                .iter()
                .filter_map(|item| match &item.method {
                    InstallMethod::Source(plan) => Some(plan.build_dependencies.iter()),
                    InstallMethod::Bottle(_) => None,
                })
                .flatten()
                .filter(|dep| checked.insert((*dep).clone()))
                .filter(|dep| !planned.contains(*dep) && !self.has_installed_keg(dep))
                .cloned()
                .collect();
            if missing.is_empty() {
                return Ok(());
            }
            let fetched = self.fetch_all_formulas(&missing).await?;
            let mut added: Vec<PlannedInstall> = Vec::new();
            for requested in nb_core::resolve_closure(&missing, &fetched)? {
                let formula = fetched.get(&requested).cloned().unwrap();
                let install_name = install_name_for(&requested, &formula);
                if planned.contains(&install_name)
                    || added.iter().any(|item| item.install_name == install_name)
                    || self.has_installed_keg(&install_name)
                {
                    continue;
                }
                added.push(self.plan_item(install_name, formula, false, true)?);
            }
            for (name, formula) in fetched {
                formulas.entry(name).or_insert(formula);
            }
            items.splice(0..0, added);
        }
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
        check_lifecycle: bool,
    ) -> Result<PlannedInstall, Error> {
        if check_lifecycle {
            crate::installer::cask::check_formula_deprecate_disable(&formula)?;
        }
        check_clt_for_bottle(&formula)?;
        let method = if build_from_source {
            match BuildPlan::from_formula(&formula, &self.prefix) {
                Some(plan) => InstallMethod::Source(Box::new(plan)),
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
                    Some(plan) => InstallMethod::Source(Box::new(plan)),
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
            as_earlier_dependency: false,
            link_later: Vec::new(),
        })
    }

    /// Select a bottle that can be poured into this prefix.
    ///
    /// Bottles pinned to a shorter Cellar than ours (Intel bottles built for
    /// `/usr/local`, Linux ones for `/home/linuxbrew/.linuxbrew`) have
    /// hardcoded paths that can't be rewritten, so they're treated as
    /// unavailable and the formula is built from source instead, as Homebrew
    /// does for a non-default prefix.
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
        if !bottle.is_pourable_into(&self.prefix) {
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

/// The formulae asked for, each once, in the order asked: Homebrew warns
/// about (or refuses) these before it looks at their dependencies.
fn requested_formulae<'a>(
    names: &[String],
    formulas: &'a BTreeMap<String, Formula>,
) -> Vec<&'a Formula> {
    let mut seen = HashSet::new();
    names
        .iter()
        .filter_map(|name| formulas.get(name))
        .filter(|formula| seen.insert(formula.name.clone()))
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
    let mut earlier_dependencies = HashSet::new();
    for name in names {
        if let Some(formula) = formulas.get(name.as_str())
            && seen.contains(&formula.name)
        {
            earlier_dependencies.insert(formula.name.clone());
        }
        visit(name, formulas, &mut seen, &mut order);
    }
    for item in items {
        item.link_rank = order
            .iter()
            .position(|name| *name == item.formula.name)
            .unwrap_or(usize::MAX);
        item.as_earlier_dependency = earlier_dependencies.contains(&item.formula.name);
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
    /// builds it, after its build dependencies are poured (with theirs).
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
        for (name, deps) in [("pkgconf", vec!["libfoo"]), ("libfoo", vec![])] {
            Mock::given(method("GET"))
                .and(path(format!("/formula/{name}.json")))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(bottled_formula(name, &deps)),
                )
                .mount(&mock_server)
                .await;
        }

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

        let names: Vec<&str> = plan
            .items
            .iter()
            .map(|item| item.formula.name.as_str())
            .collect();
        assert_eq!(names, ["libfoo", "pkgconf", "nobottle"]);
        assert!(matches!(
            plan.items[0].method,
            nb_core::InstallMethod::Bottle(_)
        ));
        assert!(matches!(
            plan.items[1].method,
            nb_core::InstallMethod::Bottle(_)
        ));
        // Dependencies, not asked for.
        assert_eq!(plan.roots, ["nobottle"]);

        if let nb_core::InstallMethod::Source(ref bp) = plan.items[2].method {
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
    async fn a_requested_formula_is_checked_before_its_dependencies() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let disabled = |name: &str, deps: &[&str]| {
            bottled_formula(name, deps).replacen(
                r#""versions""#,
                r#""disabled": true, "versions""#,
                1,
            )
        };
        let installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("app", disabled("app", &["lib"])),
                ("lib", disabled("lib", &[])),
            ],
        )
        .await;

        let error = installer.plan(&["app".to_string()]).await.unwrap_err();
        assert!(
            error.to_string().contains("app has been disabled"),
            "{error}"
        );

        let (plan, failures) = installer
            .plan_best_effort(&["app".to_string()], false)
            .await;
        assert!(plan.items.is_empty());
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "app");
        assert!(
            failures[0]
                .error
                .to_string()
                .contains("app has been disabled")
        );
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
    async fn only_the_named_formula_is_built_from_source() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let with_source = |name: &str, deps: &[&str]| {
            bottled_formula(name, deps).replacen(
                r#""versions""#,
                &format!(
                    r#""urls": {{ "stable": {{ "url": "https://example.com/{name}-1.0.0.tar.gz" }} }}, "versions""#
                ),
                1,
            )
        };
        let installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("app", with_source("app", &["lib"])),
                ("lib", with_source("lib", &[])),
            ],
        )
        .await;

        let plan = installer
            .plan_with_options(&["app".to_string()], true)
            .await
            .unwrap();

        let methods: Vec<_> = plan
            .items
            .iter()
            .map(|i| {
                let source = matches!(i.method, nb_core::InstallMethod::Source(_));
                (i.install_name.as_str(), source)
            })
            .collect();
        assert_eq!(methods, [("lib", false), ("app", true)]);
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

    /// Homebrew's autoremove keeps the build dependencies of what was built
    /// from source; those of a bottle can go.
    #[tokio::test]
    async fn autoremove_keeps_build_dependencies_of_source_builds() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let tool = bottled_formula("tool", &[]).replacen(
            r#""versions""#,
            r#""build_dependencies": ["go"], "versions""#,
            1,
        );
        let mut installer = installer_serving(
            &mock_server,
            &tmp,
            &[("tool", tool), ("go", bottled_formula("go", &[]))],
        )
        .await;
        let record = |installer: &mut Installer, tool_key: &str| {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("tool", "1.0.0", tool_key).unwrap();
            tx.record_install("go", "1.0.0", "go-bottle").unwrap();
            tx.commit().unwrap();
            installer.mark_on_request(&["tool".to_string()]).unwrap();
        };

        record(&mut installer, "source:tool:1.0.0");
        assert!(installer.unneeded_formulae().await.unwrap().is_empty());

        record(&mut installer, "tool-bottle");
        assert_eq!(installer.unneeded_formulae().await.unwrap(), ["go"]);
    }

    #[tokio::test]
    async fn a_name_asked_for_after_one_that_needs_it_goes_in_as_its_dependency() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("app", bottled_formula("app", &["lib"])),
                ("lib", bottled_formula("lib", &[])),
            ],
        )
        .await;
        async fn marked(installer: &Installer, names: &[&str]) -> Vec<(String, bool)> {
            let names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
            let plan = installer.plan(&names).await.unwrap();
            let mut marked: Vec<(String, bool)> = plan
                .items
                .iter()
                .map(|item| (item.formula.name.clone(), item.as_earlier_dependency))
                .collect();
            marked.sort();
            marked
        }
        assert_eq!(
            marked(&installer, &["app", "lib"]).await,
            [("app".into(), false), ("lib".into(), true)]
        );
        assert_eq!(
            marked(&installer, &["lib", "app"]).await,
            [("app".into(), false), ("lib".into(), false)]
        );
    }
}
