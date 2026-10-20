//! `nb tap`, `nb untap`, `nb trust` and `nb untrust`, as Homebrew's.

use console::style;
use nb_io::tap::TapName;
use nb_io::tap::trust::TrustKind;

fn ohai(message: &str) {
    eprintln!("{} {}", style("==>").blue().bold(), style(message).bold());
}

/// `brew tap [user/repo] [URL]`.
pub async fn tap(
    installer: &nb_io::Installer,
    name: Option<String>,
    remote: Option<String>,
    force: bool,
) -> Result<(), nb_core::Error> {
    let source = installer.tap_source()?;
    let Some(name) = name else {
        for tap in source.taps.installed() {
            println!("{tap}");
        }
        return Ok(());
    };
    let tap = TapName::parse(&name)?;
    nb_io::tap::Taps::check_tappable(&tap, force)?;
    if source.taps.is_installed(&tap) {
        let current = source
            .taps
            .state(&tap)
            .map(|s| s.remote)
            .unwrap_or_default();
        if let Some(remote) = &remote
            && !current.is_empty()
            && *remote != current
        {
            return Err(nb_core::Error::InvalidArgument {
                message: format!("Tap {tap} remote mismatch.\n{current} != {remote}"),
            });
        }
        // Already tapped: nothing to do, as brew says nothing.
        return Ok(());
    }
    ensure_tapped(installer, &tap, remote.as_deref()).await
}

/// `Tap#install`: fetch and report.
pub async fn ensure_tapped(
    installer: &nb_io::Installer,
    tap: &TapName,
    remote: Option<&str>,
) -> Result<(), nb_core::Error> {
    if installer.tap_source()?.taps.is_installed(tap) {
        return Ok(());
    }
    ohai(&format!("Tapping {tap}"));
    let contents = installer.tap(tap, remote).await?;
    eprintln!("Tapped {}.", contents.summary());
    Ok(())
}

/// `brew untap`.
pub async fn untap(
    installer: &mut nb_io::Installer,
    names: Vec<String>,
    force: bool,
    ui: &mut crate::ui::StdUi,
) -> Result<(), nb_core::Error> {
    let mut failed = None;
    for name in names {
        let tap = TapName::parse(&name)?;
        if !installer.tap_source()?.taps.is_installed(&tap) {
            return Err(nb_core::Error::InvalidArgument {
                message: format!("No available tap {tap}."),
            });
        }
        let installed = installer.installed_from_tap(&tap)?;
        if !installed.is_empty() {
            let kind = if installed.iter().all(|n| n.starts_with("cask:")) {
                "casks"
            } else if installed.iter().any(|n| n.starts_with("cask:")) {
                "formulae and casks"
            } else {
                "formulae"
            };
            if !force {
                eprintln!(
                    "{} Refusing to untap {tap} because it contains the following installed {kind}:\n{}",
                    style("Error:").red().bold(),
                    installed.join("\n")
                );
                failed = Some(nb_core::Error::InvalidArgument {
                    message: format!("{tap} still has installed {kind}"),
                });
                continue;
            }
            super::uninstall::execute(installer, installed, false, false, false, ui).await?;
        }
        println!("Untapping {tap}...");
        let contents = installer.untap(&tap)?;
        println!("Untapped {}.", contents.summary());
    }
    failed.map_or(Ok(()), Err)
}

/// The kind `--tap`, `--formula` or `--cask` asks for.
pub fn trust_kind(tap: bool, formula: bool, cask: bool) -> Option<TrustKind> {
    if tap {
        Some(TrustKind::Tap)
    } else if formula {
        Some(TrustKind::Formula)
    } else if cask {
        Some(TrustKind::Cask)
    } else {
        None
    }
}

#[derive(Clone, Copy)]
pub enum TrustAction {
    Trust,
    Untrust,
}

/// `brew trust` / `brew untrust`.
pub fn trust(
    installer: &nb_io::Installer,
    names: Vec<String>,
    kind: Option<TrustKind>,
    action: TrustAction,
) -> Result<(), nb_core::Error> {
    let source = installer.tap_source()?;
    let kinds = [TrustKind::Tap, TrustKind::Formula, TrustKind::Cask];
    if names.is_empty() {
        let verb = match action {
            TrustAction::Trust => "Trusted",
            TrustAction::Untrust => return Ok(()),
        };
        println!("All official taps and commands are trusted.");
        let mut printed = false;
        for kind in kinds.into_iter().filter(|k| kind.is_none_or(|w| w == *k)) {
            let entries = source.trust.entries(kind);
            if entries.is_empty() {
                continue;
            }
            let label = match kind {
                TrustKind::Tap => "taps",
                TrustKind::Formula => "formulae",
                TrustKind::Cask => "casks",
            };
            println!("{verb} {label}:");
            for entry in entries {
                println!("  {entry}");
            }
            printed = true;
        }
        if !printed {
            println!("No trusted taps, formulae, casks or commands.");
        }
        return Ok(());
    }
    for name in names {
        let (kind, target) = trust_target(installer, &name, kind)?;
        if kind == TrustKind::Tap && TapName::parse(&target)?.is_official() {
            println!("Official tap {target} is always trusted.");
            continue;
        }
        let label = kind.label();
        match action {
            TrustAction::Trust => {
                let verb = if source.trust.trust(kind, &target)? {
                    "Trusted"
                } else {
                    "Already trusted"
                };
                println!("{verb} {label}: {target}");
            }
            TrustAction::Untrust => {
                let verb = if source.trust.untrust(kind, &target)? {
                    "Untrusted"
                } else {
                    "Not trusted"
                };
                println!("{verb} {label}: {target}");
            }
        }
    }
    Ok(())
}

/// `Homebrew::Trust.target`: a tap for `user/repo`, otherwise the formula
/// or cask of that full name in its tap.
fn trust_target(
    installer: &nb_io::Installer,
    name: &str,
    kind: Option<TrustKind>,
) -> Result<(TrustKind, String), nb_core::Error> {
    if name.matches('/').count() == 1 || kind == Some(TrustKind::Tap) {
        return Ok((TrustKind::Tap, TapName::parse(name)?.name()));
    }
    let Some((tap, item)) = TapName::split_full_name(name) else {
        return Err(nb_core::Error::InvalidArgument {
            message: "Trust targets must be fully-qualified tap, formula, cask or command names."
                .to_string(),
        });
    };
    let taps = &installer.tap_source()?.taps;
    let formula = taps.find_formula(&tap, &item).map(|(n, _)| n);
    let cask = taps.find_cask(&tap, &item).map(|(n, _)| n);
    let pick = |kind: TrustKind, found: Option<String>| {
        (
            kind,
            format!("{tap}/{}", found.unwrap_or(item.to_lowercase())),
        )
    };
    match (kind, formula, cask) {
        (Some(TrustKind::Formula), f, _) => Ok(pick(TrustKind::Formula, f)),
        (Some(TrustKind::Cask), _, c) => Ok(pick(TrustKind::Cask, c)),
        (_, Some(f), None) => Ok(pick(TrustKind::Formula, Some(f))),
        (_, None, Some(c)) => Ok(pick(TrustKind::Cask, Some(c))),
        (_, None, None) => Err(nb_core::Error::InvalidArgument {
            message: format!("No formula, cask or command found for {name}."),
        }),
        _ => Err(nb_core::Error::InvalidArgument {
            message: format!("Ambiguous trust target {name}. Use `--formula` or `--cask`."),
        }),
    }
}

/// Before `nb install user/repo/name`: tap the repository if needed and
/// trust what is named in full, as `brew install` does.
pub async fn prepare_full_names(
    installer: &nb_io::Installer,
    names: &[String],
) -> Result<(), nb_core::Error> {
    for name in names {
        let bare = name.strip_prefix("cask:").unwrap_or(name);
        let bare = bare.strip_prefix("formula:").unwrap_or(bare);
        let Some((tap, item)) = TapName::split_full_name(bare) else {
            continue;
        };
        if tap.is_official() {
            continue;
        }
        ensure_tapped(installer, &tap, None).await?;
        let taps = &installer.tap_source()?.taps;
        let found = if name.starts_with("cask:") {
            taps.find_cask(&tap, &item)
                .map(|(n, _)| (TrustKind::Cask, n))
        } else {
            taps.find_formula(&tap, &item)
                .map(|(n, _)| (TrustKind::Formula, n))
                .or_else(|| {
                    taps.find_cask(&tap, &item)
                        .map(|(n, _)| (TrustKind::Cask, n))
                })
        };
        if let Some((kind, resolved)) = found
            && installer
                .tap_source()?
                .trust
                .trust(kind, &format!("{tap}/{resolved}"))?
        {
            ohai(&format!("Trusted {} {tap}/{item}", kind.label()));
        }
    }
    Ok(())
}
