use chrono::NaiveDate;
use nb_core::{Error, PostInstallStep};
use serde_json::Value;

/// A file or bundle a cask puts in place: `source` inside the download,
/// `target` the name it gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaskArtifact {
    pub source: String,
    pub target: String,
}

/// An artifact moved into place (`font`, `qlplugin`, `artifact`, …) or, when
/// `linked`, symlinked there (`manpage`, shell completions). `target` is the
/// path the API resolved, placeholders and all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedArtifact {
    pub kind: String,
    pub source: String,
    pub target: String,
    pub linked: bool,
}

/// A `command_wrapper`: a script named `name` in `bin`, either `content` as
/// given or one that runs `executable` with `args` and `env`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandWrapper {
    pub name: String,
    pub content: Option<String>,
    pub executable: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// A `generate_completions_from_executable`: `commands` run once per shell
/// to print its completions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GeneratedCompletion {
    pub commands: Vec<String>,
    pub base_name: Option<String>,
    /// Homebrew's `shell_parameter_format`; `None` passes the shell's name.
    pub format: Option<String>,
    pub shells: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedCask {
    pub install_name: String,
    pub token: String,
    pub version: String,
    pub url: String,
    /// The download's checksum; for a `no_check` cask, a name for its blob
    /// made from the URL and version.
    pub sha256: String,
    /// `sha256 :no_check`: the download is not verified, as Homebrew does
    /// for its own casks.
    pub no_check: bool,
    pub binaries: Vec<CaskArtifact>,
    pub apps: Vec<CaskArtifact>,
    pub hooks: CaskHooks,
    /// The `installer` and `pkg` artifacts, as published: run, not placed.
    pub installers: Vec<Value>,
    pub placed: Vec<PlacedArtifact>,
    pub wrappers: Vec<CommandWrapper>,
    /// `generated_script`s: a path in the staged directory and its content.
    pub generated_scripts: Vec<(String, String)>,
    pub completions: Vec<GeneratedCompletion>,
}

/// Artifacts moved into place, in Homebrew's install order.
const MOVED_KINDS: &[&str] = &[
    "artifact",
    "app_image",
    "colorpicker",
    "prefpane",
    "qlplugin",
    "mdimporter",
    "dictionary",
    "font",
    "service",
    "input_method",
    "internet_plugin",
    "keyboard_layout",
    "audio_unit_plugin",
    "vst_plugin",
    "vst3_plugin",
    "screen_saver",
];

/// Artifacts symlinked into place, in Homebrew's install order.
const LINKED_KINDS: &[&str] = &[
    "manpage",
    "bash_completion",
    "zsh_completion",
    "fish_completion",
    "pwsh_completion",
];

/// Every other key nextbrew handles. `target` is the API's resolved target
/// beside an artifact, not an artifact.
const HANDLED_KEYS: &[&str] = &[
    "app",
    "suite",
    "binary",
    "installer",
    "pkg",
    "uninstall",
    "zap",
    "stage_only",
    "command_wrapper",
    "generated_script",
    "generate_completions_from_executable",
    "preflight_steps",
    "postflight_steps",
    "uninstall_preflight_steps",
    "uninstall_postflight_steps",
    "target",
];

/// A cask's `*flight` blocks, which the API publishes as install steps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaskHooks {
    pub preflight: Vec<PostInstallStep>,
    pub postflight: Vec<PostInstallStep>,
    pub uninstall_preflight: Vec<PostInstallStep>,
    pub uninstall_postflight: Vec<PostInstallStep>,
}

impl CaskHooks {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// The cask as Homebrew sees it on `tag`: that tag's `variations` entry
/// merged over the top-level keys (`Homebrew::API.merge_variations`).
/// Without an entry for `tag` the top level is used as is; entries for
/// other OS versions never apply.
pub fn merge_variations(cask: &Value, tag: Option<&str>) -> Value {
    let mut merged = cask.clone();
    let Some(fields) = merged.as_object_mut() else {
        return merged;
    };
    let variations = fields.remove("variations");
    if let Some(Value::Object(variation)) = tag.and_then(|tag| variations?.get(tag).cloned()) {
        fields.extend(variation);
    }
    merged
}

/// [`merge_variations`] for the running system.
pub fn for_current_platform(cask: &Value) -> Value {
    merge_variations(cask, nb_core::current_platform_tag().as_deref())
}

/// What Homebrew's `check_deprecate_disable` makes of a cask: a deprecated
/// one installs with a warning (even when also disabled), a disabled one
/// does not install. Each carries Homebrew's message.
#[derive(Debug, PartialEq, Eq)]
pub enum DeprecateDisable {
    Deprecated(String),
    Disabled(String),
}

/// See [`DeprecateDisable`]; the wording follows `DeprecateDisable.message`.
/// Takes cask JSON, or formula JSON (told apart by `token`, which only
/// casks have, as the reason codes differ).
pub fn deprecate_disable(cask: &Value, today: NaiveDate) -> Option<DeprecateDisable> {
    let flag = |key: &str| cask[key].as_bool() == Some(true);
    let text = |key: &str| cask[key].as_str().filter(|s| !s.is_empty());
    let date = |key: &str| text(key).and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());

    let (kind, reason) = if flag("deprecated") {
        ("deprecated", text("deprecation_reason"))
    } else if flag("disabled") {
        ("disabled", text("disable_reason"))
    } else {
        return None;
    };
    let token = text("token");
    let name = token.or(text("name")).unwrap_or_default();
    let mut message = format!("{name} has been {kind}");
    match reason {
        Some(reason) if token.is_some() => {
            message += &format!(" because it {}!", reason_text(reason))
        }
        Some(reason) => message += &format!(" because it {}!", formula_reason_text(reason)),
        None => message += "!",
    }

    let disable_date = date("disable_date")
        .or_else(|| date("deprecation_date")?.checked_add_months(chrono::Months::new(12)));
    if let Some(disable_date) = disable_date {
        let tense = if disable_date < today {
            "was"
        } else {
            "will be"
        };
        message += &format!(" It {tense} disabled on {disable_date}.");
    }

    let prefix = if flag("disabled") {
        "disable"
    } else {
        "deprecation"
    };
    let formula = text(&format!("{prefix}_replacement_formula"));
    let replacement = match (formula, text(&format!("{prefix}_replacement_cask"))) {
        (Some(formula), _) => Some(formula.to_string()),
        (None, Some(cask)) => Some(format!("cask:{cask}")),
        (None, None) => None,
    };
    if let Some(replacement) = replacement {
        message += &format!("\nReplacement:\n  nb install {replacement}\n");
    }

    Some(if kind == "deprecated" {
        DeprecateDisable::Deprecated(message)
    } else {
        DeprecateDisable::Disabled(message)
    })
}

/// Warn about a deprecated cask and refuse a disabled one, before anything
/// is downloaded (Homebrew's `Cask::Installer#prelude`).
pub fn check_deprecate_disable(cask: &Value) -> Result<(), Error> {
    match deprecate_disable(cask, chrono::Local::now().date_naive()) {
        Some(DeprecateDisable::Deprecated(message)) => {
            tracing::warn!("{message}");
            Ok(())
        }
        Some(DeprecateDisable::Disabled(message)) => Err(Error::InvalidArgument { message }),
        None => Ok(()),
    }
}

/// Warn about a deprecated formula and refuse a disabled one, as Homebrew's
/// `FormulaInstaller#prelude` does for each formula it installs.
pub fn check_formula_deprecate_disable(formula: &nb_core::Formula) -> Result<(), Error> {
    let mut json = serde_json::to_value(&formula.lifecycle).unwrap_or_default();
    json["name"] = Value::from(formula.name.as_str());
    check_deprecate_disable(&json)
}

/// Homebrew's wording for its formula reason codes; anything else is free text.
fn formula_reason_text(reason: &str) -> &str {
    match reason {
        "does_not_build" => "does not build",
        "no_license" => "has no license",
        "repo_archived" => "has an archived upstream repository",
        "repo_removed" => "has a removed upstream repository",
        "unmaintained" => "is not maintained upstream",
        "unreachable" => "is no longer reliably reachable upstream",
        "unsupported" => "is not supported upstream",
        "deprecated_upstream" => "is deprecated upstream",
        "versioned_formula" => "is a versioned formula",
        "checksum_mismatch" => {
            "was built with an initially released source file that had a different checksum \
             than the current one. Upstream's repository might have been compromised. We can \
             re-package this once upstream has confirmed that they retagged their release"
        }
        other => other,
    }
}

/// Homebrew's wording for its cask reason codes; anything else is free text.
fn reason_text(reason: &str) -> &str {
    match reason {
        "discontinued" => "is discontinued upstream",
        "moved_to_mas" => "is now exclusively distributed on the Mac App Store",
        "no_longer_available" => "is no longer available upstream",
        "no_longer_meets_criteria" => "no longer meets the criteria for acceptable casks",
        "unmaintained" => "is not maintained upstream",
        "fails_gatekeeper_check" => "does not pass the macOS Gatekeeper check",
        "unreachable" => "is no longer reliably reachable upstream",
        other => other,
    }
}

pub fn resolve_cask(token: &str, cask: &Value) -> Result<ResolvedCask, Error> {
    let cask = &for_current_platform(cask);
    let url = required_string(cask, "url")?;
    let sha256 = required_string(cask, "sha256")?;
    let version = required_string(cask, "version")?;

    let no_check = sha256 == "no_check";
    let sha256 = if no_check {
        use sha2::{Digest, Sha256};
        let key = Sha256::new().chain_update(format!("no_check\0{url}\0{version}"));
        crate::checksum::sha256_hex(key)
    } else {
        sha256
    };

    let artifacts = cask
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidArgument {
            message: "failed to parse cask JSON: missing artifacts array".to_string(),
        })?;

    // A binary given a path in the prefix as its target (a completion in
    // `share`, headers in `include`) or in the home folder (a terminfo
    // entry) is linked there, not into `bin`. One elsewhere goes into `bin`
    // (spec, chapter 4).
    let mut binaries = Vec::new();
    let mut linked_binaries = Vec::new();
    for (binary, path_target) in parse_artifacts(artifacts, "binary")?
        .into_iter()
        .zip(binary_path_targets(artifacts))
    {
        match path_target {
            Some(target) => linked_binaries.push(PlacedArtifact {
                kind: "binary".to_string(),
                source: binary.source,
                target,
                linked: true,
            }),
            None => binaries.push(binary),
        }
    }
    // A suite is a folder of apps, moved into the Applications folder the
    // same way. One given a path as its target goes where that says.
    let mut apps = Vec::new();
    let mut moved_apps = Vec::new();
    for kind in ["app", "suite"] {
        for (app, path_target) in parse_artifacts(artifacts, kind)?
            .into_iter()
            .zip(path_targets(artifacts, kind))
        {
            match path_target {
                Some(target) => moved_apps.push(PlacedArtifact {
                    kind: kind.to_string(),
                    source: app.source,
                    target,
                    linked: false,
                }),
                None => apps.push(app),
            }
        }
    }
    // Homebrew runs `installer` before `pkg`.
    let installers: Vec<Value> = ["installer", "pkg"]
        .iter()
        .flat_map(|kind| artifacts.iter().filter(move |a| a.get(*kind).is_some()))
        .cloned()
        .collect();
    if let Some(kind) = artifacts
        .iter()
        .filter_map(Value::as_object)
        .flat_map(|artifact| artifact.keys())
        .find(|key| {
            let key = key.as_str();
            !HANDLED_KEYS.contains(&key)
                && !MOVED_KINDS.contains(&key)
                && !LINKED_KINDS.contains(&key)
        })
    {
        return Err(Error::InvalidArgument {
            message: format!(
                "cask '{token}' has a '{kind}' artifact, which nextbrew does not support"
            ),
        });
    }
    let mut placed = moved_apps;
    placed.extend(parse_placed(token, artifacts)?);
    placed.extend(linked_binaries);
    let wrappers = parse_wrappers(token, artifacts)?;
    let generated_scripts = parse_generated_scripts(token, artifacts)?;
    let completions = parse_completions(artifacts);
    if binaries.is_empty()
        && apps.is_empty()
        && installers.is_empty()
        && placed.is_empty()
        && wrappers.is_empty()
    {
        let found = artifact_types(cask);
        return Err(Error::InvalidArgument {
            message: format!("cask '{token}' has nothing to install (found: {found})"),
        });
    }

    Ok(ResolvedCask {
        install_name: format!("cask:{token}"),
        token: token.to_string(),
        version,
        url,
        sha256,
        no_check,
        binaries,
        apps,
        hooks: CaskHooks {
            preflight: parse_steps(artifacts, "preflight_steps")?,
            postflight: parse_steps(artifacts, "postflight_steps")?,
            uninstall_preflight: parse_steps(artifacts, "uninstall_preflight_steps")?,
            uninstall_postflight: parse_steps(artifacts, "uninstall_postflight_steps")?,
        },
        installers,
        placed,
        wrappers,
        generated_scripts,
        completions,
    })
}

/// The moved and linked artifacts, in Homebrew's order.
fn parse_placed(token: &str, artifacts: &[Value]) -> Result<Vec<PlacedArtifact>, Error> {
    let mut placed = Vec::new();
    for (kinds, linked) in [(MOVED_KINDS, false), (LINKED_KINDS, true)] {
        for kind in kinds {
            for artifact in artifacts {
                let Some(args) = artifact.get(*kind).and_then(Value::as_array) else {
                    continue;
                };
                let invalid = || Error::InvalidArgument {
                    message: format!("cask '{token}' has a malformed '{kind}' artifact"),
                };
                let source = args.first().and_then(Value::as_str).ok_or_else(invalid)?;
                let target = artifact["target"].as_str().ok_or_else(invalid)?;
                placed.push(PlacedArtifact {
                    kind: kind.to_string(),
                    source: source.to_string(),
                    target: target.to_string(),
                    linked,
                });
            }
        }
    }
    Ok(placed)
}

fn parse_wrappers(token: &str, artifacts: &[Value]) -> Result<Vec<CommandWrapper>, Error> {
    artifacts
        .iter()
        .filter_map(|artifact| artifact.get("command_wrapper")?.as_array())
        .map(|args| {
            let invalid = || Error::InvalidArgument {
                message: format!("cask '{token}' has a malformed 'command_wrapper' artifact"),
            };
            let name = args.first().and_then(Value::as_str).ok_or_else(invalid)?;
            let options = &args.get(1).cloned().unwrap_or_default();
            let text = |key: &str| options[key].as_str().map(ToString::to_string);
            let wrapper = CommandWrapper {
                name: name.to_string(),
                content: text("content"),
                executable: text("executable"),
                args: options["args"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect(),
                env: options["env"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                    .collect(),
            };
            if name.contains('/') || wrapper.content.is_some() == wrapper.executable.is_some() {
                return Err(invalid());
            }
            Ok(wrapper)
        })
        .collect()
}

fn parse_generated_scripts(
    token: &str,
    artifacts: &[Value],
) -> Result<Vec<(String, String)>, Error> {
    artifacts
        .iter()
        .filter_map(|artifact| artifact.get("generated_script")?.as_array())
        .map(|args| {
            let path = args.first().and_then(Value::as_str);
            let content = args.get(1).and_then(|o| o["content"].as_str());
            match (path, content) {
                (Some(path), Some(content))
                    if !path.starts_with('/') && !path.split('/').any(|part| part == "..") =>
                {
                    Ok((path.to_string(), content.to_string()))
                }
                _ => Err(Error::InvalidArgument {
                    message: format!("cask '{token}' has a malformed 'generated_script' artifact"),
                }),
            }
        })
        .collect()
}

fn parse_completions(artifacts: &[Value]) -> Vec<GeneratedCompletion> {
    artifacts
        .iter()
        .filter_map(|artifact| {
            artifact
                .get("generate_completions_from_executable")?
                .as_array()
        })
        .map(|args| {
            let options = args
                .iter()
                .find(|a| a.is_object())
                .cloned()
                .unwrap_or_default();
            let format = options["shell_parameter_format"]
                .as_str()
                .map(ToString::to_string);
            let shells = match options["shells"].as_array() {
                Some(shells) => shells
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect(),
                // Homebrew's default_completion_shells.
                None if matches!(format.as_deref(), Some("cobra" | "typer")) => {
                    ["bash", "zsh", "fish", "pwsh"].map(String::from).to_vec()
                }
                None => ["bash", "zsh", "fish"].map(String::from).to_vec(),
            };
            GeneratedCompletion {
                commands: args
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect(),
                base_name: options["base_name"].as_str().map(ToString::to_string),
                format,
                shells,
            }
        })
        .collect()
}

/// Steps of every `{key: [{"steps": [..]}]}` artifact, in order.
fn parse_steps(artifacts: &[Value], key: &str) -> Result<Vec<PostInstallStep>, Error> {
    artifacts
        .iter()
        .filter_map(|a| a.get(key)?.as_array())
        .flatten()
        .filter_map(|block| block.get("steps")?.as_array())
        .flatten()
        .map(|step| {
            serde_json::from_value(step.clone()).map_err(|e| Error::InvalidArgument {
                message: format!("failed to parse cask {key}: {e}"),
            })
        })
        .collect()
}

fn required_string(value: &Value, field: &str) -> Result<String, Error> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| Error::InvalidArgument {
            message: format!("failed to parse cask JSON: missing string field '{field}'"),
        })
}

fn artifact_types(cask: &Value) -> String {
    let types: Vec<&str> = cask
        .get("artifacts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| a.as_object())
        .flat_map(|obj| obj.keys())
        .map(String::as_str)
        .collect();

    if types.is_empty() {
        "none".to_string()
    } else {
        types.join(", ")
    }
}

/// Collect the `kind` stanzas. Each is `[source, {"target": ..}?]`; an
/// array of such arrays is accepted too.
fn parse_artifacts(artifacts: &[Value], kind: &str) -> Result<Vec<CaskArtifact>, Error> {
    let mut parsed = Vec::new();
    for artifact in artifacts {
        let Some(args) = artifact.get(kind).and_then(Value::as_array) else {
            continue;
        };

        if args.first().is_some_and(Value::is_array) {
            for stanza in args.iter().filter_map(Value::as_array) {
                parsed.push(parse_stanza(kind, stanza)?);
            }
        } else {
            parsed.push(parse_stanza(kind, args)?);
        }
    }
    Ok(parsed)
}

/// For each `binary` stanza, in [`parse_artifacts`]'s order, its target when
/// that is a path in the prefix other than `bin` (`Binary#resolve_target`
/// takes a relative one in `bin`) or in the home folder.
fn binary_path_targets(artifacts: &[Value]) -> Vec<Option<String>> {
    path_targets(artifacts, "binary")
        .into_iter()
        .map(|target| {
            let target = target?;
            if target.starts_with("/$HOME/") {
                return Some(target);
            }
            let target = match target.strip_prefix("$APPDIR/") {
                Some(rel) => format!("$HOMEBREW_PREFIX/bin/{rel}"),
                None => target,
            };
            let rel = target.strip_prefix("$HOMEBREW_PREFIX/")?;
            let in_bin = rel
                .strip_prefix("bin/")
                .is_some_and(|name| !name.contains('/'));
            (!in_bin).then_some(target)
        })
        .collect()
}

/// For each `kind` stanza, in [`parse_artifacts`]'s order, its target when
/// that is a path rather than a name, as Homebrew's `resolve_target` takes
/// it: absolute as given, `~` from the home folder, otherwise in `$APPDIR`.
fn path_targets(artifacts: &[Value], kind: &str) -> Vec<Option<String>> {
    artifacts
        .iter()
        .filter_map(|artifact| artifact.get(kind).and_then(Value::as_array))
        .flat_map(|args| {
            if args.first().is_some_and(Value::is_array) {
                args.iter().filter_map(Value::as_array).collect()
            } else {
                vec![args]
            }
        })
        .map(|stanza| {
            let target = stanza.get(1)?.get("target")?.as_str()?;
            if !target.contains('/') {
                return None;
            }
            Some(if let Some(in_home) = target.strip_prefix("~/") {
                format!("/$HOME/{in_home}")
            } else if target.starts_with('/') || target.starts_with('$') {
                target.to_string()
            } else {
                format!("$APPDIR/{target}")
            })
        })
        .collect()
}

fn parse_stanza(kind: &str, args: &[Value]) -> Result<CaskArtifact, Error> {
    let source = args
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidArgument {
            message: format!("unsupported cask {kind} source"),
        })?;

    // Targets may be absolute Homebrew paths ("$HOMEBREW_PREFIX/bin/code",
    // "/usr/local/bin/x"); nextbrew decides the directory, so keep the name.
    let target = args
        .get(1)
        .and_then(|opts| opts.get("target"))
        .and_then(Value::as_str)
        .unwrap_or(source);
    let target = basename(target)?;
    if target.starts_with('$') || target.starts_with('~') {
        return Err(Error::InvalidArgument {
            message: format!("unsupported cask {kind} target '{target}'"),
        });
    }

    Ok(CaskArtifact {
        source: source.to_string(),
        target,
    })
}

fn basename(path: &str) -> Result<String, Error> {
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::InvalidArgument {
            message: format!("invalid cask binary path '{path}'"),
        })?;
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apps_given_a_path_target_are_moved_there() {
        let json = serde_json::json!({
            "token": "box-tools",
            "version": "1.0",
            "url": "https://example.com/a.zip",
            "sha256": "aaa",
            "artifacts": [
                { "app": ["A.app"], "target": "/Applications/A.app" },
                { "app": ["B.app", { "target": "~/Library/Box/B.app" }] },
                { "app": ["C.app", { "target": "Box/C.app" }] },
                { "suite": ["sdk", { "target": "$HOMEBREW_PREFIX/share/sdk" }],
                  "target": "/Applications/$HOMEBREW_PREFIX/share/sdk" }
            ]
        });
        let cask = resolve_cask("box-tools", &json).unwrap();

        assert_eq!(cask.apps.len(), 1);
        assert_eq!(cask.apps[0].target, "A.app");
        let placed: Vec<_> = cask
            .placed
            .iter()
            .map(|p| (p.kind.as_str(), p.source.as_str(), p.target.as_str()))
            .collect();
        assert_eq!(
            placed,
            [
                ("app", "B.app", "/$HOME/Library/Box/B.app"),
                ("app", "C.app", "$APPDIR/Box/C.app"),
                ("suite", "sdk", "$HOMEBREW_PREFIX/share/sdk"),
            ]
        );
    }

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn deprecate_disable_refuses_a_disabled_cask() {
        // ampps
        let cask = serde_json::json!({
            "token": "ampps", "deprecated": false, "disabled": true,
            "disable_date": "2026-10-12", "disable_reason": "fails_gatekeeper_check"
        });
        assert_eq!(
            deprecate_disable(&cask, date("2026-10-15")),
            Some(DeprecateDisable::Disabled(
                "ampps has been disabled because it does not pass the macOS Gatekeeper check! \
                 It was disabled on 2026-10-12."
                    .to_string()
            ))
        );
    }

    #[test]
    fn deprecate_disable_only_warns_when_also_deprecated() {
        // 1password-cli@1: Homebrew checks `deprecated?` first.
        let cask = serde_json::json!({
            "token": "1password-cli@1", "deprecated": true, "deprecation_date": "2025-06-11",
            "deprecation_reason": "unmaintained", "disabled": true,
            "disable_date": "2026-06-11", "disable_reason": "unmaintained"
        });
        assert_eq!(
            deprecate_disable(&cask, date("2026-10-15")),
            Some(DeprecateDisable::Deprecated(
                "1password-cli@1 has been deprecated because it is not maintained upstream! \
                 It was disabled on 2026-06-11."
                    .to_string()
            ))
        );
    }

    #[test]
    fn deprecate_disable_dates_from_deprecation_and_names_the_replacement() {
        let cask = serde_json::json!({
            "token": "old", "deprecated": true, "deprecation_date": "2026-03-01",
            "deprecation_reason": "something custom", "deprecation_replacement_cask": "new"
        });
        assert_eq!(
            deprecate_disable(&cask, date("2026-10-15")),
            Some(DeprecateDisable::Deprecated(
                "old has been deprecated because it something custom! \
                 It will be disabled on 2027-03-01.\nReplacement:\n  nb install cask:new\n"
                    .to_string()
            ))
        );
        assert_eq!(
            deprecate_disable(&serde_json::json!({ "token": "fine" }), date("2026-10-15")),
            None
        );
    }

    #[test]
    fn a_disabled_formula_is_refused_with_its_reason() {
        let formula: nb_core::Formula = serde_json::from_value(serde_json::json!({
            "name": "oldlib", "versions": {"stable": "1.0"}, "dependencies": [],
            "bottle": {"stable": {"files": {}}}, "disabled": true, "disable_date": "2020-01-01",
            "disable_reason": "does_not_build", "disable_replacement_formula": "newlib"
        }))
        .unwrap();
        let err = check_formula_deprecate_disable(&formula).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid argument: oldlib has been disabled because it does not build! \
             It was disabled on 2020-01-01.\nReplacement:\n  nb install newlib\n"
        );
    }

    /// Shape of visual-studio-code: arm64 Big Sur is pinned to an old release.
    fn cask_with_old_os_variation() -> Value {
        serde_json::json!({
            "token": "test",
            "version": "2.0.0",
            "url": "https://example.com/2.0.0/arm64.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [{ "binary": [["op"]] }],
            "variations": {
                "tahoe": {
                    "url": "https://example.com/2.0.0/intel.zip",
                    "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                },
                "arm64_big_sur": {
                    "version": "1.0.0",
                    "url": "https://example.com/1.0.0/arm64.zip",
                    "sha256": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                    "artifacts": [{ "binary": [["old-op"]] }]
                }
            }
        })
    }

    #[test]
    fn merge_variations_ignores_other_os_entries() {
        let merged = merge_variations(&cask_with_old_os_variation(), Some("arm64_tahoe"));
        assert_eq!(merged["version"], "2.0.0");
        assert_eq!(merged["url"], "https://example.com/2.0.0/arm64.zip");
        assert!(merged.get("variations").is_none());
    }

    #[test]
    fn merge_variations_applies_exact_tag_over_top_level() {
        let merged = merge_variations(&cask_with_old_os_variation(), Some("arm64_big_sur"));
        let resolved = resolve_cask("test", &merged).unwrap();
        assert_eq!(resolved.version, "1.0.0");
        assert_eq!(resolved.url, "https://example.com/1.0.0/arm64.zip");
        assert_eq!(resolved.sha256, "c".repeat(64));
        assert_eq!(resolved.binaries[0].target, "old-op");

        let merged = merge_variations(&cask_with_old_os_variation(), Some("tahoe"));
        assert_eq!(merged["version"], "2.0.0");
        assert_eq!(merged["url"], "https://example.com/2.0.0/intel.zip");
    }

    #[test]
    fn merge_variations_without_tag_keeps_top_level() {
        let merged = merge_variations(&cask_with_old_os_variation(), None);
        assert_eq!(merged["url"], "https://example.com/2.0.0/arm64.zip");
        assert!(merged.get("variations").is_none());
    }

    #[test]
    fn resolve_cask_parses_binary_targets() {
        let cask = serde_json::json!({
            "token": "test",
            "version": "1.0.0",
            "url": "https://example.com/test.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [{
                "binary": [
                    ["bin/tool"],
                    ["bin/tool2", {"target": "tool-two"}]
                ]
            }]
        });

        let resolved = resolve_cask("test", &cask).unwrap();
        assert_eq!(resolved.binaries.len(), 2);
        assert_eq!(resolved.binaries[0].target, "tool");
        assert_eq!(resolved.binaries[1].target, "tool-two");
    }

    #[test]
    fn a_binary_with_a_path_target_is_linked_there() {
        // aerospace's completions, and alacritty's terminfo entries.
        let cask = serde_json::json!({
            "token": "test",
            "version": "1.0.0",
            "url": "https://example.com/test.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                {"binary": ["bin/test"], "target": "$HOMEBREW_PREFIX/bin/test"},
                {"binary": ["bin/test2", {"target": "$HOMEBREW_PREFIX/bin/test2"}]},
                {"binary": ["zsh/_test", {"target": "$HOMEBREW_PREFIX/share/zsh/site-functions/_test"}]},
                {"binary": ["61/test", {"target": "~/.terminfo/61/x"}],
                 "target": "/$HOME/.terminfo/61/x"},
                {"binary": ["bin/docker", {"target": "/usr/local/bin/docker"}]},
            ]
        });

        let resolved = resolve_cask("test", &cask).unwrap();
        let names: Vec<_> = resolved
            .binaries
            .iter()
            .map(|b| b.target.as_str())
            .collect();
        // Outside the prefix and the home folder, into `bin` (spec, chapter 4).
        assert_eq!(names, ["test", "test2", "docker"]);
        assert_eq!(
            resolved.placed,
            [
                PlacedArtifact {
                    kind: "binary".into(),
                    source: "zsh/_test".into(),
                    target: "$HOMEBREW_PREFIX/share/zsh/site-functions/_test".into(),
                    linked: true,
                },
                PlacedArtifact {
                    kind: "binary".into(),
                    source: "61/test".into(),
                    target: "/$HOME/.terminfo/61/x".into(),
                    linked: true,
                },
            ]
        );
    }

    #[test]
    fn resolve_cask_missing_required_field_is_invalid_argument() {
        let cask = serde_json::json!({
            "token": "test",
            "version": "1.0.0",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [{ "binary": [["op"]] }]
        });

        let err = resolve_cask("test", &cask).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
    }

    #[test]
    fn resolve_cask_missing_artifacts_array_is_invalid_argument() {
        let cask = serde_json::json!({
            "token": "test",
            "version": "1.0.0",
            "url": "https://example.com/test.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        });

        let err = resolve_cask("test", &cask).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
    }

    fn cask_with(artifacts: Value) -> Value {
        serde_json::json!({
            "token": "x", "version": "1.0", "url": "https://example.com/x.zip",
            "sha256": "a".repeat(64), "artifacts": artifacts
        })
    }

    #[test]
    fn an_artifact_nextbrew_cannot_place_is_refused_by_name() {
        let cask = cask_with(serde_json::json!([
            { "brand_new_artifact": ["k"] },
            { "zap": [{ "trash": ["~/.config/x/"] }] }
        ]));
        let msg = resolve_cask("x", &cask).unwrap_err().to_string();
        assert!(msg.contains("'brand_new_artifact'"), "got: {msg}");
    }

    #[test]
    fn moved_and_linked_artifacts_keep_the_target_the_api_resolved() {
        // font-0xproto, alacritty, android-commandlinetools.
        let cask = cask_with(serde_json::json!([
            { "manpage": ["$APPDIR/A.app/a.1.gz"], "target": "$HOMEBREW_PREFIX/share/man/man1/a.1.gz" },
            { "font": ["fonts/X-Regular.otf"], "target": "/$HOME/Library/Fonts/X-Regular.otf" },
            { "artifact": ["cmdline-tools", { "target": "$HOMEBREW_PREFIX/share/t/latest" }],
              "target": "$HOMEBREW_PREFIX/share/t/latest" },
            { "zsh_completion": ["c/_a"], "target": "$HOMEBREW_PREFIX/share/zsh/site-functions/_a" }
        ]));
        let resolved = resolve_cask("x", &cask).unwrap();
        let placed: Vec<_> = resolved
            .placed
            .iter()
            .map(|p| {
                (
                    p.kind.as_str(),
                    p.source.as_str(),
                    p.target.as_str(),
                    p.linked,
                )
            })
            .collect();
        // Homebrew's order: moved kinds first, artifact before font.
        assert_eq!(
            placed,
            [
                (
                    "artifact",
                    "cmdline-tools",
                    "$HOMEBREW_PREFIX/share/t/latest",
                    false
                ),
                (
                    "font",
                    "fonts/X-Regular.otf",
                    "/$HOME/Library/Fonts/X-Regular.otf",
                    false
                ),
                (
                    "manpage",
                    "$APPDIR/A.app/a.1.gz",
                    "$HOMEBREW_PREFIX/share/man/man1/a.1.gz",
                    true
                ),
                (
                    "zsh_completion",
                    "c/_a",
                    "$HOMEBREW_PREFIX/share/zsh/site-functions/_a",
                    true
                ),
            ]
        );
    }

    #[test]
    fn wrappers_scripts_and_generated_completions_are_parsed() {
        let cask = cask_with(serde_json::json!([
            { "command_wrapper": ["keka", { "executable": "$APPDIR/Keka.app/Keka", "args": ["--cli"] }],
              "target": "$HOMEBREW_PREFIX/bin/keka" },
            { "command_wrapper": ["gogs", { "content": "#!/bin/sh\n./gogs \"$@\"\n" }] },
            { "generated_script": ["Setup.sh", { "content": "#!/bin/bash\necho hi\n" }] },
            { "generate_completions_from_executable": ["op", "completion",
                { "base_name": null, "shell_parameter_format": null, "shells": ["bash", "zsh", "fish"] }] }
        ]));
        let resolved = resolve_cask("x", &cask).unwrap();
        assert_eq!(resolved.wrappers.len(), 2);
        assert_eq!(resolved.wrappers[0].name, "keka");
        assert_eq!(
            resolved.wrappers[0].executable.as_deref(),
            Some("$APPDIR/Keka.app/Keka")
        );
        assert_eq!(resolved.wrappers[0].args, ["--cli"]);
        assert_eq!(
            resolved.wrappers[1].content.as_deref(),
            Some("#!/bin/sh\n./gogs \"$@\"\n")
        );
        assert_eq!(
            resolved.generated_scripts,
            [("Setup.sh".to_string(), "#!/bin/bash\necho hi\n".to_string())]
        );
        let completion = &resolved.completions[0];
        assert_eq!(completion.commands, ["op", "completion"]);
        assert_eq!(completion.shells, ["bash", "zsh", "fish"]);
        assert_eq!(completion.format, None);
    }

    #[test]
    fn resolve_cask_parses_app_and_appdir_binaries() {
        // Shape of the formulae.brew.sh API (docker-desktop, ghostty).
        let cask = serde_json::json!({
            "token": "docker-desktop",
            "version": "4.0.0",
            "url": "https://example.com/Docker.dmg",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "uninstall": [{ "quit": "com.docker.docker" }] },
                { "app": ["Docker.app"], "target": "/Applications/Docker.app" },
                {
                    "binary": [
                        "$APPDIR/Docker.app/Contents/Resources/bin/docker-credential-ecr-login",
                        { "target": "/usr/local/bin/docker-credential-ecr-login" }
                    ],
                    "target": "/usr/local/bin/docker-credential-ecr-login"
                },
                { "binary": ["$APPDIR/Docker.app/Contents/Resources/bin/docker"] }
            ]
        });

        let resolved = resolve_cask("docker-desktop", &cask).unwrap();
        assert_eq!(
            resolved.apps,
            vec![CaskArtifact {
                source: "Docker.app".to_string(),
                target: "Docker.app".to_string(),
            }]
        );
        assert_eq!(resolved.binaries.len(), 2);
        assert_eq!(resolved.binaries[0].target, "docker-credential-ecr-login");
        assert_eq!(resolved.binaries[1].target, "docker");
    }

    #[test]
    fn resolve_cask_orders_installers_before_pkgs() {
        let cask = serde_json::json!({
            "token": "zoom",
            "version": "1.0.0",
            "url": "https://example.com/zoom.pkg",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "pkg": ["zoomusInstallerFull.pkg"] },
                { "installer": [{ "manual": "Setup.app" }] }
            ]
        });

        let resolved = resolve_cask("zoom", &cask).unwrap();
        assert!(resolved.installers[0].get("installer").is_some());
        assert!(resolved.installers[1].get("pkg").is_some());
    }
}
