//! `nb tap`, `nb untap`, `nb trust` and `nb untrust`, as Homebrew's.

use console::style;
use nb_io::tap::TapName;
use nb_io::tap::trust::TrustKind;

fn ohai(message: &str) {
    eprintln!("{} {}", style("==>").blue().bold(), style(message).bold());
}

/// `brew tap [user/repo] [URL]`; with `custom_remote`, moving an installed
/// tap to URL.
pub async fn tap(
    installer: &nb_io::Installer,
    name: Option<String>,
    remote: Option<String>,
    force: bool,
    custom_remote: bool,
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
            if !custom_remote {
                return Err(nb_core::Error::InvalidArgument {
                    message: format!("Tap {tap} remote mismatch.\n{current} != {remote}"),
                });
            }
            ohai(&format!("{tap}: changed remote from {current} to {remote}"));
            installer.tap(&tap, Some(remote)).await?;
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

/// The kind `--tap`, `--formula`, `--cask` or `--command` asks for.
pub fn trust_kind(tap: bool, formula: bool, cask: bool, command: bool) -> Option<TrustKind> {
    if tap {
        Some(TrustKind::Tap)
    } else if formula {
        Some(TrustKind::Formula)
    } else if cask {
        Some(TrustKind::Cask)
    } else if command {
        Some(TrustKind::Command)
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
    let kinds = [
        TrustKind::Tap,
        TrustKind::Formula,
        TrustKind::Cask,
        TrustKind::Command,
    ];
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
                TrustKind::Command => "commands",
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
        let include_existing = matches!(action, TrustAction::Untrust);
        let (kind, target) = trust_target(installer, &name, kind, include_existing)?;
        if kind == TrustKind::Tap
            && !nb_io::tap::is_remote_reference(&target)
            && TapName::parse(&target)?.is_official()
        {
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
                let mut removed = source.trust.untrust(kind, &target)?;
                // A full name also forgets the entry under a custom remote.
                if kind != TrustKind::Tap
                    && let Some((tap, item)) = TapName::split_full_name(&target)
                {
                    let remote = source.taps.remote(&tap);
                    if tap.uses_custom_remote(remote.as_deref()) {
                        let entry = nb_io::tap::trust::TrustStore::item_trust_name(
                            &tap,
                            &item,
                            remote.as_deref(),
                        );
                        removed |= source.trust.untrust(kind, &entry)?;
                    }
                }
                // Untrusting a tap forgets what of it was trusted.
                if kind == TrustKind::Tap {
                    let prefix = format!("{}/", target.to_lowercase());
                    for item_kind in [TrustKind::Formula, TrustKind::Cask, TrustKind::Command] {
                        for entry in source.trust.entries(item_kind) {
                            if entry.starts_with(&prefix) {
                                removed |= source.trust.untrust(item_kind, &entry)?;
                            }
                        }
                    }
                }
                let verb = if removed { "Untrusted" } else { "Not trusted" };
                println!("{verb} {label}: {target}");
            }
        }
    }
    Ok(())
}

/// `Homebrew::Trust.target`: a tap for `user/repo` or a remote, otherwise
/// the formula or cask of that full name in its tap, named under the tap's
/// remote when it has a custom one. With `include_existing` (untrusting), an
/// entry already in the list under the plain full name is that one.
fn trust_target(
    installer: &nb_io::Installer,
    name: &str,
    kind: Option<TrustKind>,
    include_existing: bool,
) -> Result<(TrustKind, String), nb_core::Error> {
    let source = installer.tap_source()?;
    if nb_io::tap::is_remote_reference(name) && kind.is_none_or(|k| k == TrustKind::Tap) {
        let reference = nb_io::tap::remote_to_reference(name).ok_or_else(|| {
            nb_core::Error::InvalidArgument {
                message: format!("Invalid tap remote URL: {name}"),
            }
        })?;
        return Ok((TrustKind::Tap, reference));
    }
    if name.matches('/').count() == 1 || kind == Some(TrustKind::Tap) {
        let tap = TapName::parse(name)?;
        let remote = source.taps.remote(&tap);
        return Ok((
            TrustKind::Tap,
            tap.reference(remote.as_deref()).to_lowercase(),
        ));
    }
    let Some((tap, item)) = TapName::split_full_name(name) else {
        return Err(nb_core::Error::InvalidArgument {
            message: "Trust targets must be fully-qualified tap, formula, cask or command names."
                .to_string(),
        });
    };
    let remote = source.taps.remote(&tap);
    let formula = source.taps.find_formula(&tap, &item).map(|(n, _)| n);
    let cask = source.taps.find_cask(&tap, &item).map(|(n, _)| n);
    let command = source
        .taps
        .command_files(&tap)
        .contains_key(&item)
        .then(|| item.clone());
    let pick = |kind: TrustKind, found: Option<String>| {
        let item = found.unwrap_or(item.to_lowercase());
        let full = format!("{tap}/{item}");
        if include_existing && source.trust.entries(kind).contains(&full) {
            return (kind, full);
        }
        let entry = nb_io::tap::trust::TrustStore::item_trust_name(&tap, &item, remote.as_deref());
        (kind, entry)
    };
    if let Some(kind) = kind {
        let found = match kind {
            TrustKind::Formula => formula,
            TrustKind::Cask => cask,
            _ => command,
        };
        return Ok(pick(kind, found));
    }
    // `infer_target`: the one kind of that name in the tap (or, untrusting,
    // already trusted).
    let full = format!("{tap}/{}", item.to_lowercase());
    let mut candidates: Vec<(TrustKind, Option<String>)> = Vec::new();
    for (kind, found) in [
        (TrustKind::Formula, formula),
        (TrustKind::Cask, cask),
        (TrustKind::Command, command),
    ] {
        if found.is_some() || (include_existing && source.trust.entries(kind).contains(&full)) {
            candidates.push((kind, found));
        }
    }
    match candidates.as_slice() {
        [(kind, found)] => Ok(pick(*kind, found.clone())),
        [] => Err(nb_core::Error::InvalidArgument {
            message: format!("No formula, cask or command found for {name}."),
        }),
        _ => Err(nb_core::Error::InvalidArgument {
            message: format!(
                "Ambiguous trust target {name}. Use `--formula`, `--cask` or `--command`."
            ),
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
        if let Some((tap, _)) = split_full_name(name) {
            ensure_tapped(installer, &tap, None).await?;
        }
    }
    trust_full_names(installer, names)
}

/// A third-party tap's formula or cask named in full, without the `cask:`
/// or `formula:` mark.
fn split_full_name(name: &str) -> Option<(TapName, String)> {
    let bare = name.strip_prefix("cask:").unwrap_or(name);
    let bare = bare.strip_prefix("formula:").unwrap_or(bare);
    TapName::split_full_name(bare).filter(|(tap, _)| !tap.is_official())
}

/// `Homebrew::Trust.trust_fully_qualified_items!`, which `brew install`,
/// `upgrade` and `reinstall` run first: trust each formula or cask named
/// in full from a tap that is there (a `cask:` name only a cask), saying
/// so the first time.
pub fn trust_full_names(
    installer: &nb_io::Installer,
    names: &[String],
) -> Result<(), nb_core::Error> {
    for name in names {
        let Some((tap, item)) = split_full_name(name) else {
            continue;
        };
        let source = installer.tap_source()?;
        let taps = &source.taps;
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
        let remote = taps.remote(&tap);
        if let Some((kind, resolved)) = found
            && source.trust.trust(
                kind,
                &nb_io::tap::trust::TrustStore::item_trust_name(&tap, &resolved, remote.as_deref()),
            )?
        {
            ohai(&format!("Trusted {} {tap}/{item}", kind.label()));
        }
    }
    Ok(())
}
