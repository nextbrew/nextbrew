//! What nextbrew makes of the whole Homebrew API (spec chapter 6): the keys,
//! cask artifacts and step types the API uses, and the formulae and casks
//! nextbrew's own parsers cannot handle.
//!
//! ```text
//! compat_report <formula.json> <cask.json> [--baseline <report.json>] [--diff <out.md>]
//! ```
//!
//! Prints the report as JSON. With `--baseline` (an earlier report), it also
//! compares: anything new since then (a key, an artifact, a step type, or a
//! formula or cask that became unsupported) is written to `--diff` as
//! Markdown and the exit status is 2. The `homebrew-changes` workflow runs
//! it daily against the previous day's report and opens an issue on 2.

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;

use nb_core::{Formula, PostInstallStep, StepAction};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Report {
    formula_keys: BTreeSet<String>,
    cask_keys: BTreeSet<String>,
    cask_artifacts: BTreeSet<String>,
    step_types: BTreeSet<String>,
    /// A problem nextbrew has, and the formulae or casks that have it.
    unsupported: BTreeMap<String, BTreeSet<String>>,
}

impl Report {
    fn flag(&mut self, problem: String, name: &str) {
        self.unsupported
            .entry(problem)
            .or_default()
            .insert(name.to_string());
    }

    fn steps(&mut self, prefix: &str, name: &str, steps: &[PostInstallStep]) {
        for step in steps {
            if let StepAction::Unsupported { kind, .. } = &step.action {
                self.flag(format!("{prefix}:{kind}"), name);
            }
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let option = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let (Some(formula_path), Some(cask_path)) = (args.first(), args.get(1)) else {
        eprintln!(
            "usage: compat_report <formula.json> <cask.json> [--baseline <report.json>] [--diff <out.md>]"
        );
        return ExitCode::from(1);
    };
    let read = |path: &str| -> Vec<Value> {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
    };

    let report = report(&read(formula_path), &read(cask_path));
    println!("{}", serde_json::to_string_pretty(&report).unwrap());

    let Some(baseline_path) = option("--baseline") else {
        return ExitCode::SUCCESS;
    };
    let baseline: Report = serde_json::from_str(
        &std::fs::read_to_string(&baseline_path).unwrap_or_else(|e| panic!("{baseline_path}: {e}")),
    )
    .unwrap_or_else(|e| panic!("{baseline_path}: {e}"));
    let diff = diff(&baseline, &report);
    if diff.is_empty() {
        eprintln!("nothing new since the baseline");
        return ExitCode::SUCCESS;
    }
    match option("--diff") {
        Some(out) => std::fs::write(&out, &diff).unwrap_or_else(|e| panic!("{out}: {e}")),
        None => eprintln!("{diff}"),
    }
    ExitCode::from(2)
}

fn report(formulae: &[Value], casks: &[Value]) -> Report {
    let mut report = Report::default();
    for json in formulae {
        let name = json["name"].as_str().unwrap_or("<unnamed>");
        report.formula_keys.extend(keys(json));
        if json["disabled"].as_bool() == Some(true) {
            continue;
        }
        for step in json["post_install_steps"].as_array().into_iter().flatten() {
            report
                .step_types
                .extend(step["type"].as_str().map(String::from));
        }
        match serde_json::from_value::<Formula>(json.clone()) {
            Ok(formula) => report.steps("formula_step", name, &formula.post_install_steps),
            Err(e) => report.flag(format!("formula: {}", first_line(&e.to_string())), name),
        }
    }
    for json in casks {
        let token = json["token"].as_str().unwrap_or("<unnamed>");
        report.cask_keys.extend(keys(json));
        for artifact in json["artifacts"].as_array().into_iter().flatten() {
            report.cask_artifacts.extend(keys(artifact));
            for (key, hooks) in artifact.as_object().into_iter().flatten() {
                if !key.ends_with("_steps") {
                    continue;
                }
                for hook in hooks.as_array().into_iter().flatten() {
                    for step in hook["steps"].as_array().into_iter().flatten() {
                        report
                            .step_types
                            .extend(step["type"].as_str().map(String::from));
                    }
                }
            }
        }
        if json["disabled"].as_bool() == Some(true) {
            continue;
        }
        // Homebrew refuses these before reading the download ("This cask
        // requires macOS."), as nb's install does; off macOS their null
        // `url` and `sha256` are not a parser gap.
        // ponytail: so the ubuntu report does not check their macOS data.
        if cfg!(not(target_os = "macos")) && !json["depends_on"]["macos"].is_null() {
            continue;
        }
        match nb_io::installer::resolve_cask(token, json) {
            Ok(cask) => {
                let hooks = &cask.hooks;
                for steps in [
                    &hooks.preflight,
                    &hooks.postflight,
                    &hooks.uninstall_preflight,
                    &hooks.uninstall_postflight,
                ] {
                    report.steps("cask_step", token, steps);
                }
            }
            Err(e) => {
                let message = e.to_string().replace(&format!("'{token}'"), "<token>");
                report.flag(format!("cask: {}", first_line(&message)), token);
            }
        }
    }
    report
}

fn keys(json: &Value) -> impl Iterator<Item = String> + '_ {
    json.as_object().into_iter().flat_map(|o| o.keys().cloned())
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

/// What `now` has that `baseline` did not, as Markdown; empty when nothing.
fn diff(baseline: &Report, now: &Report) -> String {
    let mut out = String::new();
    let mut section = |title: &str, items: Vec<String>| {
        if items.is_empty() {
            return;
        }
        out.push_str(&format!("### {title}\n\n"));
        for item in items {
            out.push_str(&format!("- {item}\n"));
        }
        out.push('\n');
    };
    let added = |old: &BTreeSet<String>, new: &BTreeSet<String>| -> Vec<String> {
        new.difference(old).map(|k| format!("`{k}`")).collect()
    };
    section(
        "New formula keys",
        added(&baseline.formula_keys, &now.formula_keys),
    );
    section("New cask keys", added(&baseline.cask_keys, &now.cask_keys));
    section(
        "New cask artifacts",
        added(&baseline.cask_artifacts, &now.cask_artifacts),
    );
    section(
        "New step types",
        added(&baseline.step_types, &now.step_types),
    );
    let empty = BTreeSet::new();
    section(
        "Newly unsupported",
        now.unsupported
            .iter()
            .filter_map(|(problem, names)| {
                let before = baseline.unsupported.get(problem).unwrap_or(&empty);
                let new: Vec<&String> = names.difference(before).collect();
                (!new.is_empty()).then(|| {
                    let list: Vec<String> = new.iter().map(|n| format!("`{n}`")).collect();
                    format!("{problem}: {}", list.join(", "))
                })
            })
            .collect(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_steps_and_artifacts_are_reported_and_diffed() {
        let formulae = [json!({
            "name": "foo", "versions": {"stable": "1"}, "dependencies": [], "bottle": {"stable": {"files": {}}},
            "post_install_steps": [{"type": "mkdir_p", "path": {"path": "/x"}}, {"type": "brand_new"}]
        })];
        let casks = [json!({
            "token": "bar", "url": "https://example.com/bar.zip", "sha256": "no_check",
            "version": "1", "artifacts": [{"brand_new_artifact": ["Bar"]}]
        })];
        let now = report(&formulae, &casks);
        assert!(now.step_types.contains("brand_new"));
        assert!(now.cask_artifacts.contains("brand_new_artifact"));
        assert_eq!(
            *now.unsupported
                .get("formula_step:brand_new")
                .unwrap_or_else(|| panic!("{:?}", now.unsupported)),
            BTreeSet::from(["foo".to_string()])
        );
        assert!(
            now.unsupported
                .keys()
                .any(|k| k.contains("<token> has a 'brand_new_artifact' artifact"))
        );

        assert!(diff(&now, &now).is_empty());
        let text = diff(&Report::default(), &now);
        assert!(text.contains("### New step types\n\n- `brand_new`\n"));
        assert!(text.contains("formula_step:brand_new: `foo`"));
    }
}
