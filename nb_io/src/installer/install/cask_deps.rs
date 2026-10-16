//! A cask's `depends_on`, as Homebrew's `Cask::Installer` handles it: the
//! system requirements (`macos`, `maximum_macos`, `arch`) are checked before
//! anything is downloaded, and missing `cask` and `formula` dependencies are
//! installed once the cask is downloaded and before it is staged, casks
//! first. A cask that `conflicts_with` an installed one is refused.

use nb_core::Error;
use serde_json::Value;
use tracing::warn;

use super::Installer;

/// The running system, as far as cask requirements care.
#[derive(Debug, Clone, Copy)]
struct System {
    /// Major version (`26`), or `None` when not on macOS.
    macos: Option<u32>,
    /// `arm` or `intel`, Homebrew's `Hardware::CPU.type`.
    arch: &'static str,
}

impl System {
    fn current() -> Self {
        #[cfg(target_os = "macos")]
        let macos = nb_core::macos_major_version();
        #[cfg(not(target_os = "macos"))]
        let macos = None;
        Self {
            macos,
            arch: if cfg!(target_arch = "aarch64") {
                "arm"
            } else {
                "intel"
            },
        }
    }
}

/// Refuse a cask whose `depends_on` the running system does not meet
/// (Homebrew's `check_requirements`).
pub(super) fn check_requirements(cask: &Value) -> Result<(), Error> {
    match unmet_requirement(cask, System::current()) {
        Some(reason) => Err(Error::InvalidArgument {
            message: format!("{}: {reason}", cask["token"].as_str().unwrap_or_default()),
        }),
        None => Ok(()),
    }
}

fn unmet_requirement(cask: &Value, system: System) -> Option<String> {
    let depends_on = &cask["depends_on"];
    if depends_on.get("linux").is_some() && cfg!(not(target_os = "linux")) {
        return Some("This cask requires Linux.".to_string());
    }
    for key in ["macos", "maximum_macos"] {
        let Some(requirement) = depends_on.get(key).filter(|v| !v.is_null()) else {
            continue;
        };
        let Some(current) = system.macos else {
            return Some("This cask requires macOS.".to_string());
        };
        if let Some(reason) = unmet_macos(requirement, current) {
            return Some(reason);
        }
    }
    let arches = depends_on["arch"].as_array().filter(|a| !a.is_empty())?;
    let matches = |arch: &Value| {
        arch["type"] == system.arch
            && (arch["bits"] == 64
                || arch["bits"]
                    .as_array()
                    .is_some_and(|b| b.contains(&64.into())))
    };
    if arches.iter().any(matches) {
        return None;
    }
    let wanted: Vec<String> = arches
        .iter()
        .map(|a| {
            format!(
                "{{type: :{}, bits: {}}}",
                a["type"].as_str().unwrap_or_default(),
                a["bits"]
            )
        })
        .collect();
    Some(format!(
        "This cask depends on hardware architecture being one of [{}], \
         but you are running {{type: :{}, bits: 64}}.",
        wanted.join(", "),
        system.arch
    ))
}

/// `{">=": ["12"]}`, `{"<=": ["14"]}` or `{"==": ["11", "12"]}`, with
/// `MacOSRequirement#message`'s wording when `current` falls outside it.
fn unmet_macos(requirement: &Value, current: u32) -> Option<String> {
    let (comparator, versions) = requirement.as_object()?.iter().next()?;
    let versions: Vec<&str> = match versions {
        Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
        Value::String(one) => vec![one.as_str()],
        _ => return None,
    };
    let current = [current, 0];
    let satisfied = versions.iter().any(|v| {
        let wanted = version_parts(v);
        match comparator.as_str() {
            ">=" => current >= wanted,
            "<=" => current <= wanted,
            "==" => current == wanted,
            _ => false,
        }
    });
    if satisfied {
        return None;
    }
    let names: Vec<&str> = versions.iter().map(|v| pretty_name(v)).collect();
    Some(match comparator.as_str() {
        ">=" => format!(
            "This cask does not run on macOS versions older than {}.",
            names[0]
        ),
        "<=" => format!(
            "This cask does not run on macOS versions newer than {}.",
            names[0]
        ),
        _ => match names.split_last() {
            Some((last, rest)) if !rest.is_empty() => format!(
                "This cask does not run on macOS versions other than {} and {last}.",
                rest.join(", ")
            ),
            _ => format!(
                "This cask does not run on macOS versions other than {}.",
                names.first().copied().unwrap_or("Dunno")
            ),
        },
    })
}

/// `"10.15"` → `[10, 15]`, `"26"` → `[26, 0]`.
fn version_parts(version: &str) -> [u32; 2] {
    let mut parts = version.split('.').map(|p| p.parse().unwrap_or(0));
    [parts.next().unwrap_or(0), parts.next().unwrap_or(0)]
}

/// `MacOSVersion#pretty_name`.
fn pretty_name(version: &str) -> &'static str {
    match version {
        "27" => "Golden Gate",
        "26" => "Tahoe",
        "15" => "Sequoia",
        "14" => "Sonoma",
        "13" => "Ventura",
        "12" => "Monterey",
        "11" => "Big Sur",
        "10.15" => "Catalina",
        "10.14" => "Mojave",
        "10.13" => "High Sierra",
        "10.12" => "Sierra",
        "10.11" => "El Capitan",
        _ => "Dunno",
    }
}

/// One name or a list of them.
pub(super) fn names(value: &Value) -> Vec<String> {
    match value {
        Value::String(one) => vec![one.clone()],
        Value::Array(list) => list
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

impl Installer {
    /// Refuse a cask whose `conflicts_with` names an installed cask
    /// (Homebrew's `check_conflicts`).
    pub(super) fn check_conflicts(&self, cask: &Value) -> Result<(), Error> {
        let token = cask["token"].as_str().unwrap_or_default();
        match names(&cask["conflicts_with"]["cask"])
            .into_iter()
            .find(|other| self.is_installed(&format!("cask:{other}")))
        {
            Some(other) => Err(Error::InvalidArgument {
                message: format!("Cask '{token}' conflicts with '{other}'."),
            }),
            None => Ok(()),
        }
    }

    /// Install the cask and formula dependencies of `cask` that are not
    /// installed yet (Homebrew's `satisfy_cask_and_formula_dependencies`).
    /// `chain` holds the casks being installed around this one, to catch a
    /// cycle.
    pub(super) async fn install_cask_dependencies(
        &mut self,
        cask: &Value,
        link: bool,
        chain: &mut Vec<String>,
    ) -> Result<(), Error> {
        let token = cask["token"].as_str().unwrap_or_default();
        let casks: Vec<String> = names(&cask["depends_on"]["cask"])
            .into_iter()
            .filter(|dep| !self.is_installed(&format!("cask:{dep}")))
            .collect();
        let formulas: Vec<String> = names(&cask["depends_on"]["formula"])
            .into_iter()
            .filter(|dep| !self.is_installed(dep))
            .collect();
        if casks.is_empty() && formulas.is_empty() {
            return Ok(());
        }
        if let Some(dep) = casks
            .iter()
            .find(|dep| *dep == token || chain.contains(dep))
        {
            return Err(Error::DependencyCycle {
                cycle: chain
                    .iter()
                    .chain([&token.to_string(), dep])
                    .map(|t| format!("cask:{t}"))
                    .collect(),
            });
        }

        let all: Vec<String> = casks.iter().chain(&formulas).cloned().collect();
        eprintln!("==> Installing dependencies: {}", all.join(", "));

        chain.push(token.to_string());
        for dep in &casks {
            Box::pin(self.install_cask_in_chain(dep, link, chain)).await?;
        }
        chain.pop();

        if !formulas.is_empty() {
            let plan = self.plan(&formulas).await?;
            let result = self.execute_inner(plan, link, None).await?;
            for notice in result.notices {
                eprintln!("==> {}: {}", notice.name, notice.message);
            }
            for failure in result.post_install_failures {
                warn!(cask = %token, "dependency post-install failed: {failure}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAHOE_ARM: System = System {
        macos: Some(26),
        arch: "arm",
    };

    fn cask(depends_on: Value) -> Value {
        serde_json::json!({ "token": "x", "depends_on": depends_on })
    }

    #[test]
    fn macos_minimum_and_maximum() {
        let newer = cask(serde_json::json!({ "macos": { ">=": ["27"] } }));
        assert_eq!(
            unmet_requirement(&newer, TAHOE_ARM).as_deref(),
            Some("This cask does not run on macOS versions older than Golden Gate.")
        );
        let older = cask(serde_json::json!({ "maximum_macos": { "<=": ["14"] } }));
        assert_eq!(
            unmet_requirement(&older, TAHOE_ARM).as_deref(),
            Some("This cask does not run on macOS versions newer than Sonoma.")
        );
        let fine = cask(
            serde_json::json!({ "macos": { ">=": ["12"] }, "maximum_macos": { "<=": ["26"] } }),
        );
        assert_eq!(unmet_requirement(&fine, TAHOE_ARM), None);
        assert_eq!(
            unmet_requirement(&cask(serde_json::json!({ "macos": {} })), TAHOE_ARM),
            None
        );
    }

    #[test]
    fn macos_exact_versions_list_every_name() {
        // calhash
        let c =
            cask(serde_json::json!({ "macos": { "==": ["11", "12", "13", "14", "15", "26"] } }));
        assert_eq!(unmet_requirement(&c, TAHOE_ARM), None);
        assert_eq!(
            unmet_requirement(
                &c,
                System {
                    macos: Some(27),
                    arch: "arm"
                }
            )
            .as_deref(),
            Some(
                "This cask does not run on macOS versions other than Big Sur, Monterey, \
                 Ventura, Sonoma, Sequoia and Tahoe."
            )
        );
    }

    #[test]
    fn macos_requirement_off_macos() {
        let c = cask(serde_json::json!({ "macos": {} }));
        assert_eq!(
            unmet_requirement(
                &c,
                System {
                    macos: None,
                    arch: "intel"
                }
            )
            .as_deref(),
            Some("This cask requires macOS.")
        );
    }

    #[test]
    fn arch_requirement() {
        let arm_only = cask(serde_json::json!({ "arch": [{ "type": "arm", "bits": 64 }] }));
        assert_eq!(unmet_requirement(&arm_only, TAHOE_ARM), None);
        assert_eq!(
            unmet_requirement(
                &arm_only,
                System {
                    macos: Some(26),
                    arch: "intel"
                }
            )
            .as_deref(),
            Some(
                "This cask depends on hardware architecture being one of \
                 [{type: :arm, bits: 64}], but you are running {type: :intel, bits: 64}."
            )
        );
    }

    mod install {
        use std::fs;
        use std::path::Path;

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
        use nb_core::Error;

        fn installer(root: &Path, server: &MockServer) -> Installer {
            fs::create_dir_all(root.join("db")).unwrap();
            fs::create_dir_all(root.join("locks")).unwrap();
            let uri = server.uri();
            Installer::new(
                ApiClient::with_base_url(format!("{uri}/formula"))
                    .unwrap()
                    .with_cask_base_url(format!("{uri}/cask")),
                BlobCache::new(&root.join("cache")).unwrap(),
                Store::new(root).unwrap(),
                Cellar::new(root).unwrap(),
                Linker::new(&root.join("prefix")).unwrap(),
                Database::open(&root.join("db/nb.sqlite3")).unwrap(),
                root.join("prefix"),
                root.join("locks"),
            )
        }

        /// A cask that is one shell script, with the given `depends_on`.
        async fn serve_cask(server: &MockServer, token: &str, depends_on: &str) {
            let body = format!("#!/bin/sh\necho {token}\n");
            let sha = sha256_hex(body.as_bytes());
            let uri = server.uri();
            Mock::given(method("GET"))
                .and(path(format!("/cask/{token}.json")))
                .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                    r#"{{"token": "{token}", "version": "1.0", "url": "{uri}/dl/{token}",
                        "sha256": "{sha}", "depends_on": {depends_on},
                        "artifacts": [{{"binary": ["{token}"]}}]}}"#
                )))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path(format!("/dl/{token}")))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(server)
                .await;
        }

        async fn serve_formula(server: &MockServer, name: &str) {
            let bottle = create_bottle_tarball(name);
            let sha = sha256_hex(&bottle);
            let tag = get_test_bottle_tag();
            Mock::given(method("GET"))
                .and(path(format!("/formula/{name}.json")))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(formula_json_with(
                        &server.uri(),
                        name,
                        "1.0.0",
                        &sha,
                        "",
                    )),
                )
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path(format!("/bottles/{name}-1.0.0.{tag}.bottle.tar.gz")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
                .mount(server)
                .await;
        }

        #[tokio::test]
        async fn installs_missing_cask_and_formula_dependencies_first() {
            let server = MockServer::start().await;
            let tmp = TempDir::new().unwrap();
            serve_cask(&server, "helper", "{}").await;
            serve_formula(&server, "deplib").await;
            serve_cask(
                &server,
                "main",
                r#"{"cask": ["helper"], "formula": "deplib"}"#,
            )
            .await;
            let mut installer = installer(tmp.path(), &server);

            installer
                .install(&["cask:main".to_string()], true)
                .await
                .unwrap();

            assert!(installer.is_installed("cask:helper"));
            assert!(installer.is_installed("deplib"));
            assert!(installer.is_installed("cask:main"));

            // The cask keeps its formula dependency until it goes.
            assert!(!installer.get_installed("deplib").unwrap().on_request);
            assert!(installer.unneeded_formulae().await.unwrap().is_empty());
            installer.uninstall("cask:main").unwrap();
            assert_eq!(installer.unneeded_formulae().await.unwrap(), ["deplib"]);
        }

        #[tokio::test]
        async fn refuses_a_cask_that_conflicts_with_an_installed_one() {
            let server = MockServer::start().await;
            let tmp = TempDir::new().unwrap();
            serve_cask(&server, "stable", "{}").await;
            let body = "#!/bin/sh\n";
            Mock::given(method("GET"))
                .and(path("/cask/beta.json"))
                .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                    r#"{{"token": "beta", "version": "1.0", "url": "{}/dl/beta",
                        "sha256": "{}", "conflicts_with": {{"cask": ["stable"]}},
                        "artifacts": [{{"binary": ["beta"]}}]}}"#,
                    server.uri(),
                    sha256_hex(body.as_bytes())
                )))
                .mount(&server)
                .await;
            let mut installer = installer(tmp.path(), &server);
            installer
                .install(&["cask:stable".to_string()], true)
                .await
                .unwrap();

            let err = installer
                .install(&["cask:beta".to_string()], true)
                .await
                .unwrap_err();

            assert!(
                err.to_string()
                    .contains("Cask 'beta' conflicts with 'stable'."),
                "got {err}"
            );
            assert!(!installer.is_installed("cask:beta"));
        }

        #[tokio::test]
        async fn a_cask_from_another_tap_is_refused_by_name() {
            let server = MockServer::start().await;
            let tmp = TempDir::new().unwrap();
            let mut installer = installer(tmp.path(), &server);

            let err = installer
                .install(&["cask:microsoft/git/microsoft-git".to_string()], true)
                .await
                .unwrap_err()
                .to_string();

            assert!(err.contains("'microsoft/git' tap"), "{err}");
        }

        #[tokio::test]
        async fn refuses_a_dependency_cycle() {
            let server = MockServer::start().await;
            let tmp = TempDir::new().unwrap();
            serve_cask(&server, "a", r#"{"cask": ["b"]}"#).await;
            serve_cask(&server, "b", r#"{"cask": ["a"]}"#).await;
            let mut installer = installer(tmp.path(), &server);

            let err = installer
                .install(&["cask:a".to_string()], true)
                .await
                .unwrap_err();

            assert!(
                matches!(&err, Error::DependencyCycle { cycle } if cycle == &["cask:a", "cask:b", "cask:a"]),
                "got {err:?}"
            );
            assert!(!installer.is_installed("cask:a"));
            assert!(!installer.is_installed("cask:b"));
        }
    }

    #[test]
    fn names_accepts_one_or_many() {
        assert_eq!(names(&serde_json::json!("a")), ["a"]);
        assert_eq!(names(&serde_json::json!(["a", "b"])), ["a", "b"]);
        assert!(names(&Value::Null).is_empty());
    }
}
