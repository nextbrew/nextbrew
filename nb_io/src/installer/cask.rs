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

/// Artifacts that need an installer or a system location nextbrew does not
/// manage. Stanzas not listed here and not handled (zap, uninstall,
/// manpage, completions, ...) are skipped.
const UNSUPPORTED_ARTIFACTS: &[&str] = &["pkg", "installer", "suite"];

pub fn resolve_cask(token: &str, cask: &Value) -> Result<ResolvedCask, Error> {
    let mut url = required_string(cask, "url")?;
    let mut sha256 = required_string(cask, "sha256")?;
    let version = required_string(cask, "version")?;

    if let Some(variation) = select_platform_variation(cask) {
        if let Some(variation_url) = variation.get("url").and_then(Value::as_str) {
            url = variation_url.to_string();
        }
        if let Some(variation_sha) = variation.get("sha256").and_then(Value::as_str) {
            sha256 = variation_sha.to_string();
        }
    }

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

    if let Some(kind) = UNSUPPORTED_ARTIFACTS
        .iter()
        .find(|kind| artifacts.iter().any(|a| a.get(**kind).is_some()))
    {
        return Err(Error::InvalidArgument {
            message: format!("cask '{token}' uses '{kind}' artifacts, which are not supported yet"),
        });
    }

    let binaries = parse_artifacts(artifacts, "binary")?;
    let apps = parse_artifacts(artifacts, "app")?;
    if binaries.is_empty() && apps.is_empty() {
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

fn select_platform_variation(cask: &Value) -> Option<&Value> {
    let variations = cask.get("variations")?;
    preferred_variation_keys()
        .iter()
        .find_map(|key| variations.get(key))
}

fn preferred_variation_keys() -> &'static [&'static str] {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        &["x86_64_linux", "arm64_linux"]
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        &["arm64_linux", "x86_64_linux"]
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        &[
            "arm64_tahoe",
            "arm64_sequoia",
            "arm64_sonoma",
            "arm64_ventura",
            "arm64_monterey",
            "arm64_big_sur",
        ]
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        &[
            "tahoe", "sequoia", "sonoma", "ventura", "monterey", "big_sur", "catalina",
        ]
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        &[]
    }
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

    #[test]
    fn resolve_cask_uses_platform_variation_url_and_sha() {
        let cask = serde_json::json!({
            "token": "test",
            "version": "1.0.0",
            "url": "https://example.com/darwin.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [{ "binary": [["op"]] }],
            "variations": {
                "x86_64_linux": {
                    "url": "https://example.com/linux.zip",
                    "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                }
            }
        });

        let _resolved = resolve_cask("test", &cask).unwrap();
        #[cfg(target_os = "linux")]
        {
            assert_eq!(_resolved.url, "https://example.com/linux.zip");
            assert_eq!(
                _resolved.sha256,
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            );
        }
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
    fn resolve_cask_rejects_pkg_artifacts() {
        let cask = serde_json::json!({
            "token": "zoom",
            "version": "1.0.0",
            "url": "https://example.com/zoom.pkg",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [{ "pkg": ["zoomusInstallerFull.pkg"] }]
        });

        let msg = resolve_cask("zoom", &cask).unwrap_err().to_string();
        assert!(msg.contains("'pkg'"), "got: {msg}");
    }
}
