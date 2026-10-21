//! What Homebrew has installed, read through its own commands, for
//! `nb migrate`: `brew info --json=v2 --installed`, `brew tap-info --json
//! --installed` and `brew services list --json`.

use std::collections::BTreeMap;
use std::process::Command;

use nb_core::Error;
use serde_json::Value;

/// A formula or cask Homebrew has installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomebrewPackage {
    pub name: String,
    pub tap: String,
    pub is_cask: bool,
    /// `brew pin`ned.
    pub pinned: bool,
    /// Installed by name rather than as a dependency.
    pub on_request: bool,
}

impl HomebrewPackage {
    /// From a tap other than homebrew/core and homebrew/cask.
    pub fn from_other_tap(&self) -> bool {
        !matches!(self.tap.as_str(), "homebrew/core" | "homebrew/cask")
    }

    /// `user/repo/name` for one from another tap, as nb is asked for it;
    /// otherwise the name.
    pub fn full_name(&self) -> String {
        if self.from_other_tap() {
            format!("{}/{}", self.tap, self.name)
        } else {
            self.name.clone()
        }
    }
}

/// What `nb migrate` can and cannot take over.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HomebrewMigrationPackages {
    /// homebrew/core formulae installed by name; their dependencies follow.
    pub formulas: Vec<HomebrewPackage>,
    /// Formulae from other taps, installed by name.
    pub non_core_formulas: Vec<HomebrewPackage>,
    /// homebrew/cask casks.
    pub casks: Vec<HomebrewPackage>,
    /// Casks from other taps.
    pub non_core_casks: Vec<HomebrewPackage>,
    /// Formulae whose Homebrew service is started.
    pub services: Vec<String>,
    /// The taps Homebrew fetches from somewhere other than GitHub's
    /// `user/homebrew-repo`, and where from.
    pub tap_remotes: BTreeMap<String, String>,
}

/// The formulae and casks in `brew info --json=v2 --installed`.
pub fn parse_installed(json: &Value) -> Vec<HomebrewPackage> {
    let mut packages = Vec::new();
    for formula in json["formulae"].as_array().into_iter().flatten() {
        let Some(name) = formula["name"].as_str() else {
            continue;
        };
        let on_request = formula["installed"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|keg| keg["installed_on_request"].as_bool() == Some(true));
        packages.push(HomebrewPackage {
            name: name.to_string(),
            tap: formula["tap"]
                .as_str()
                .unwrap_or("homebrew/core")
                .to_string(),
            is_cask: false,
            pinned: formula["pinned"].as_bool() == Some(true),
            on_request,
        });
    }
    for cask in json["casks"].as_array().into_iter().flatten() {
        let Some(token) = cask["token"].as_str() else {
            continue;
        };
        packages.push(HomebrewPackage {
            name: token.to_string(),
            tap: cask["tap"].as_str().unwrap_or("homebrew/cask").to_string(),
            is_cask: true,
            pinned: false,
            on_request: true,
        });
    }
    packages
}

/// The formulae `brew services list --json` shows as started.
pub fn parse_started_services(json: &Value) -> Vec<String> {
    json.as_array()
        .into_iter()
        .flatten()
        .filter(|service| service["status"].as_str() == Some("started"))
        .filter_map(|service| service["name"].as_str().map(str::to_string))
        .collect()
}

/// The taps in `brew tap-info --json --installed` with a custom remote,
/// and that remote.
pub fn parse_custom_remotes(json: &Value) -> BTreeMap<String, String> {
    json.as_array()
        .into_iter()
        .flatten()
        .filter(|tap| tap["custom_remote"].as_bool() == Some(true))
        .filter_map(|tap| {
            let name = tap["name"].as_str()?;
            let remote = tap["remote"].as_str().filter(|r| !r.is_empty())?;
            Some((name.to_string(), remote.to_string()))
        })
        .collect()
}

/// Sort what Homebrew has into what nb can take over.
pub fn categorize_packages(
    packages: Vec<HomebrewPackage>,
    services: Vec<String>,
) -> HomebrewMigrationPackages {
    let mut sorted = HomebrewMigrationPackages {
        services,
        ..Default::default()
    };
    for pkg in packages {
        match (pkg.is_cask, pkg.tap.as_str()) {
            (true, "homebrew/cask") => sorted.casks.push(pkg),
            (true, _) => sorted.non_core_casks.push(pkg),
            (false, _) if !pkg.on_request => {}
            (false, "homebrew/core") => sorted.formulas.push(pkg),
            (false, _) => sorted.non_core_formulas.push(pkg),
        }
    }
    sorted
}

fn brew_json(args: &[&str]) -> Result<Value, Error> {
    let output = Command::new("brew")
        .args(args)
        .output()
        .map_err(Error::exec(&format!(
            "failed to run 'brew {}'",
            args.join(" ")
        )))?;
    if !output.status.success() {
        return Err((Error::exec(&format!("brew {} failed", args.join(" "))))(
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(Error::exec("failed to parse brew's JSON"))
}

/// What Homebrew has installed and which of its services run, sorted for
/// migration.
pub fn get_homebrew_packages() -> Result<HomebrewMigrationPackages, Error> {
    let installed = brew_json(&["info", "--json=v2", "--installed"])?;
    // Without `brew services` (or with nothing to list) there are none.
    let services = brew_json(&["services", "list", "--json"])
        .map(|json| parse_started_services(&json))
        .unwrap_or_default();
    let mut sorted = categorize_packages(parse_installed(&installed), services);
    // Without it, each tap is fetched from GitHub as `nb tap` would.
    sorted.tap_remotes = brew_json(&["tap-info", "--json", "--installed"])
        .map(|json| parse_custom_remotes(&json))
        .unwrap_or_default();
    Ok(sorted)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shaped like `brew info --json=v2 --installed` on Homebrew HEAD.
    const INSTALLED: &str = r#"{
      "formulae": [
        {"name": "jq", "tap": "homebrew/core", "pinned": true,
         "installed": [{"version": "1.8.2", "installed_on_request": true}]},
        {"name": "oniguruma", "tap": "homebrew/core", "pinned": false,
         "installed": [{"version": "6.9.10", "installed_on_request": false}]},
        {"name": "terraform", "tap": "hashicorp/tap", "pinned": false,
         "installed": [{"version": "1.10.0", "installed_on_request": true}]}
      ],
      "casks": [
        {"token": "font-0xproto", "tap": "homebrew/cask", "installed": "2.502"},
        {"token": "microsoft-git", "tap": "microsoft/git", "installed": "2.50"}
      ]
    }"#;

    #[test]
    fn sorts_what_homebrew_has() {
        let json: Value = serde_json::from_str(INSTALLED).unwrap();
        let services: Value = serde_json::from_str(
            r#"[{"name": "redis", "status": "started"}, {"name": "x", "status": "none"}]"#,
        )
        .unwrap();
        let sorted = categorize_packages(parse_installed(&json), parse_started_services(&services));
        let names =
            |list: &[HomebrewPackage]| list.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
        // A dependency comes along with what needs it; it is not taken by name.
        assert_eq!(names(&sorted.formulas), ["jq"]);
        assert!(sorted.formulas[0].pinned);
        assert_eq!(names(&sorted.non_core_formulas), ["terraform"]);
        assert_eq!(names(&sorted.casks), ["font-0xproto"]);
        assert_eq!(names(&sorted.non_core_casks), ["microsoft-git"]);
        assert_eq!(sorted.services, ["redis"]);
        assert_eq!(sorted.formulas[0].full_name(), "jq");
        assert_eq!(
            sorted.non_core_formulas[0].full_name(),
            "hashicorp/tap/terraform"
        );
        assert_eq!(sorted.casks[0].full_name(), "font-0xproto");
        assert_eq!(
            sorted.non_core_casks[0].full_name(),
            "microsoft/git/microsoft-git"
        );
    }

    #[test]
    fn keeps_the_remotes_of_taps_not_on_github() {
        // Shaped like `brew tap-info --json --installed`.
        let json: Value = serde_json::from_str(
            r#"[
              {"name": "hashicorp/tap", "installed": true,
               "remote": "https://github.com/hashicorp/homebrew-tap", "custom_remote": false},
              {"name": "me/tools", "installed": true,
               "remote": "https://gitlab.com/me/homebrew-tools.git", "custom_remote": true}
            ]"#,
        )
        .unwrap();
        let remotes = parse_custom_remotes(&json);
        assert_eq!(
            remotes.into_iter().collect::<Vec<_>>(),
            [(
                "me/tools".to_string(),
                "https://gitlab.com/me/homebrew-tools.git".to_string()
            )]
        );
    }
}
