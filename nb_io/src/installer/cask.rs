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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCask {
    pub install_name: String,
    pub token: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub binaries: Vec<CaskArtifact>,
    pub apps: Vec<CaskArtifact>,
    pub hooks: CaskHooks,
    /// The `installer` and `pkg` artifacts, as published: run, not placed.
    pub installers: Vec<Value>,
}

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
    let mut message = format!("{} has been {kind}", text("token").unwrap_or_default());
    match reason {
        Some(reason) => message += &format!(" because it {}!", reason_text(reason)),
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

    if sha256 == "no_check" {
        return Err(Error::InvalidArgument {
            message: format!("cask '{token}' uses an unsupported checksum mode: no_check"),
        });
    }

    let artifacts = cask
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidArgument {
            message: "failed to parse cask JSON: missing artifacts array".to_string(),
        })?;

    let binaries = parse_artifacts(artifacts, "binary")?;
    // A suite is a folder of apps, moved into the Applications folder the
    // same way.
    let mut apps = parse_artifacts(artifacts, "app")?;
    apps.extend(parse_artifacts(artifacts, "suite")?);
    // Homebrew runs `installer` before `pkg`.
    let installers: Vec<Value> = ["installer", "pkg"]
        .iter()
        .flat_map(|kind| artifacts.iter().filter(move |a| a.get(*kind).is_some()))
        .cloned()
        .collect();
    if binaries.is_empty() && apps.is_empty() && installers.is_empty() {
        let found = artifact_types(cask);
        return Err(Error::InvalidArgument {
            message: format!(
                "cask '{token}' has no binary or app artifacts (found: {found}); \
                 only casks with 'binary' or 'app' artifacts are currently supported"
            ),
        });
    }

    Ok(ResolvedCask {
        install_name: format!("cask:{token}"),
        token: token.to_string(),
        version,
        url,
        sha256,
        binaries,
        apps,
        hooks: CaskHooks {
            preflight: parse_steps(artifacts, "preflight_steps")?,
            postflight: parse_steps(artifacts, "postflight_steps")?,
            uninstall_preflight: parse_steps(artifacts, "uninstall_preflight_steps")?,
            uninstall_postflight: parse_steps(artifacts, "uninstall_postflight_steps")?,
        },
        installers,
    })
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

    #[test]
    fn resolve_cask_no_installable_artifacts_lists_found_types() {
        let cask = serde_json::json!({
            "token": "font-x",
            "version": "1.0.0",
            "url": "https://example.com/font.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "font": ["X.ttf"] },
                { "zap": [{ "trash": ["~/.config/x/"] }] }
            ]
        });

        let err = resolve_cask("font-x", &cask).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no binary or app artifacts"), "got: {msg}");
        assert!(msg.contains("font"), "got: {msg}");
        assert!(msg.contains("zap"), "got: {msg}");
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
