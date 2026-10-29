use crate::ui::{PromptDefault, StdUi};
use console::style;
use nb_io::HomebrewPackage;
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

pub async fn execute(
    installer: &mut nb_io::Installer,
    yes: bool,
    force: bool,
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    ui.heading("Fetching installed Homebrew packages...")
        .map_err(ui_error)?;

    let packages = nb_io::get_homebrew_packages()?;

    if packages.formulas.is_empty()
        && packages.non_core_formulas.is_empty()
        && packages.casks.is_empty()
        && packages.non_core_casks.is_empty()
    {
        ui.println("No Homebrew packages installed.")
            .map_err(ui_error)?;
        return Ok(());
    }

    // Those from other taps are named in full (`user/repo/name`).
    let formulas: Vec<&HomebrewPackage> = packages
        .formulas
        .iter()
        .chain(&packages.non_core_formulas)
        .collect();
    let casks: Vec<&HomebrewPackage> = packages
        .casks
        .iter()
        .chain(&packages.non_core_casks)
        .collect();
    let services: Vec<String> = packages
        .services
        .iter()
        .filter(|name| formulas.iter().any(|f| &f.name == *name))
        .cloned()
        .collect();
    ui.println(format!(
        "{} formulas and {} casks to migrate, {} services to restart",
        style(formulas.len()).green(),
        style(casks.len()).green(),
        style(services.len()).green()
    ))
    .map_err(ui_error)?;
    ui.blank_line().map_err(ui_error)?;

    if !formulas.is_empty() {
        ui.println("These formulas will be installed with nextbrew:")
            .map_err(ui_error)?;
        for pkg in &formulas {
            let pinned = if pkg.pinned { " (pinned)" } else { "" };
            ui.bullet(format!("{}{pinned}", pkg.full_name()))
                .map_err(ui_error)?;
        }
        ui.blank_line().map_err(ui_error)?;
    }
    if !casks.is_empty() {
        ui.println(
            "These casks will move from Homebrew to nextbrew (downloaded first; their data stays):",
        )
        .map_err(ui_error)?;
        for pkg in &casks {
            ui.bullet(pkg.full_name()).map_err(ui_error)?;
        }
        ui.blank_line().map_err(ui_error)?;
    }
    let taps = other_taps(formulas.iter().chain(&casks).copied());
    if !taps.is_empty() {
        ui.println("These taps will be tapped with nextbrew, and what moves from them trusted:")
            .map_err(ui_error)?;
        for tap in &taps {
            match packages.tap_remotes.get(*tap) {
                Some(remote) => ui.bullet(format!("{tap} ({remote})")),
                None => ui.bullet(*tap),
            }
            .map_err(ui_error)?;
        }
        ui.blank_line().map_err(ui_error)?;
    }
    if !services.is_empty() {
        ui.println("These services will be stopped in Homebrew and started with nextbrew:")
            .map_err(ui_error)?;
        for name in &services {
            ui.bullet(name).map_err(ui_error)?;
        }
        ui.blank_line().map_err(ui_error)?;
    }

    if !yes
        && !ui
            .prompt_yes_no("Continue with migration? [y/N]", PromptDefault::No)
            .map_err(ui_error)?
    {
        ui.println("Aborted.").map_err(ui_error)?;
        return Ok(());
    }

    let (formulas, casks) =
        prepare_taps(installer, &taps, formulas, casks, &packages.tap_remotes, ui).await?;

    migrate_casks(installer, &casks, ui).await?;

    if formulas.is_empty() {
        return Ok(());
    }

    ui.blank_line().map_err(ui_error)?;
    ui.heading(format!(
        "Migrating {} formulas to nextbrew...",
        style(formulas.len()).green().bold()
    ))
    .map_err(ui_error)?;

    let formula_names: Vec<String> = formulas.iter().map(|f| f.full_name()).collect();

    let (plan, planning_failures) = installer.plan_best_effort(&formula_names, false).await;
    if !planning_failures.is_empty() {
        ui.note(format!(
            "Skipped {} formula(s) that could not be planned:",
            planning_failures.len()
        ))
        .map_err(ui_error)?;
        for failure in &planning_failures {
            ui.bullet(format!("{} ({})", failure.name, failure.error))
                .map_err(ui_error)?;
        }
        ui.blank_line().map_err(ui_error)?;
    }

    if !plan.items.is_empty() {
        crate::commands::install::execute_formula_plan(
            installer,
            &formula_names,
            plan,
            false, // no_link
            ui,
        )
        .await
        .ok();
    }

    let (successfully_installed, failed_installed) =
        check_install_status(installer, &formula_names)?;
    let success_count = successfully_installed.len();

    ui.blank_line().map_err(ui_error)?;
    ui.heading(format!(
        "Migrated {} of {} formulas to nextbrew",
        style(success_count).green().bold(),
        formulas.len()
    ))
    .map_err(ui_error)?;

    if !failed_installed.is_empty() {
        ui.note(format!(
            "Failed to migrate {} formula(s):",
            failed_installed.len()
        ))
        .map_err(ui_error)?;
        for name in &failed_installed {
            ui.bullet(name).map_err(ui_error)?;
        }
        ui.blank_line().map_err(ui_error)?;
    }

    if success_count == 0 {
        ui.println("No formulas were successfully migrated. Skipping uninstall from Homebrew.")
            .map_err(ui_error)?;
        return Ok(());
    }

    // nextbrew keeps them by name, as Homebrew's Cellar does.
    let migrated: Vec<&HomebrewPackage> = formulas
        .iter()
        .filter(|pkg| successfully_installed.contains(&pkg.full_name()))
        .copied()
        .collect();
    for pkg in migrated.iter().filter(|pkg| pkg.pinned) {
        installer.set_pinned(&pkg.name, true)?;
    }
    let migrated_names: Vec<String> = migrated.iter().map(|pkg| pkg.name.clone()).collect();
    migrate_services(installer, &services, &migrated_names, ui).await?;

    ui.blank_line().map_err(ui_error)?;
    if !yes
        && !ui
            .prompt_yes_no(
                &format!(
                    "Uninstall {} formula(s) from Homebrew? [y/N]",
                    style(success_count).green()
                ),
                PromptDefault::No,
            )
            .map_err(ui_error)?
    {
        ui.println("Skipped uninstall from Homebrew.")
            .map_err(ui_error)?;
        return Ok(());
    }

    ui.blank_line().map_err(ui_error)?;
    ui.heading("Uninstalling from Homebrew...")
        .map_err(ui_error)?;

    if successfully_installed.is_empty() {
        return Ok(());
    }

    ui.step_start(format!(
        "uninstalling {} formulas combined",
        successfully_installed.len()
    ))
    .map_err(ui_error)?;

    // Homebrew will not uninstall a pinned formula; the pin is nextbrew's now.
    let pinned: Vec<&str> = migrated
        .iter()
        .filter(|pkg| pkg.pinned)
        .map(|pkg| pkg.name.as_str())
        .collect();
    if !pinned.is_empty() {
        let _ = Command::new("brew").arg("unpin").args(&pinned).status();
    }

    let mut args = vec!["uninstall"];
    if force {
        args.push("--force");
    }
    for target in &successfully_installed {
        args.push(target);
    }

    let status = Command::new("brew")
        .args(&args)
        .status()
        .map_err(|e| format!("Failed to run brew uninstall: {}", e));

    // brew can report success and still leave some behind, so ask it what
    // is left.
    match &status {
        Ok(s) if s.success() => ui.step_ok().map_err(ui_error)?,
        res => {
            ui.step_fail().map_err(ui_error)?;
            if let Err(e) = res {
                ui.error(e).map_err(ui_error)?;
            }
        }
    }
    let mut uninstall_failed = successfully_installed.clone();
    if let Ok(output) = Command::new("brew").args(["list", "--formula"]).output()
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let still_installed: std::collections::HashSet<&str> = stdout.lines().collect();
        // It lists the Cellar's names, without the tap.
        uninstall_failed
            .retain(|target| still_installed.contains(target.rsplit('/').next().unwrap_or(target)));
    }

    let uninstalled = successfully_installed.len() - uninstall_failed.len();
    ui.blank_line().map_err(ui_error)?;
    ui.heading(format!(
        "Uninstalled {} of {} formula(s) from Homebrew",
        style(uninstalled).green().bold(),
        success_count
    ))
    .map_err(ui_error)?;

    if !uninstall_failed.is_empty() {
        ui.note(format!(
            "Failed to uninstall {} formula(s) from Homebrew:",
            uninstall_failed.len()
        ))
        .map_err(ui_error)?;
        for name in &uninstall_failed {
            ui.bullet(name).map_err(ui_error)?;
        }
        ui.println("You may need to uninstall these manually with:")
            .map_err(ui_error)?;
        ui.println("    brew uninstall --force <formula>")
            .map_err(ui_error)?;
    }

    Ok(())
}

/// Move each cask from Homebrew to nextbrew: nextbrew checks and downloads
/// it first, Homebrew uninstalls it (no zap, so its data stays), nextbrew
/// installs it. If that fails, Homebrew installs it again.
async fn migrate_casks(
    installer: &mut nb_io::Installer,
    casks: &[&HomebrewPackage],
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    if casks.is_empty() {
        return Ok(());
    }
    ui.blank_line().map_err(ui_error)?;
    ui.heading(format!(
        "Moving {} casks to nextbrew...",
        style(casks.len()).green().bold()
    ))
    .map_err(ui_error)?;
    let mut moved = 0;
    let mut kept: Vec<(String, String)> = Vec::new();
    for cask in casks {
        let token = &cask.name;
        let full_name = cask.full_name();
        if let Err(e) = installer.prepare_cask(&full_name).await {
            kept.push((full_name, e.to_string()));
            continue;
        }
        let removed = Command::new("brew")
            .args(["uninstall", "--cask", token])
            .status()
            .is_ok_and(|status| status.success());
        if !removed {
            kept.push((full_name, "brew uninstall --cask failed".to_string()));
            continue;
        }
        let name = format!("cask:{full_name}");
        let _ = crate::commands::install::execute(installer, vec![name.clone()], false, false, ui)
            .await;
        if installer.get_installed_by_full_name(&name).is_some() {
            moved += 1;
            continue;
        }
        let restored = Command::new("brew")
            .args(["install", "--cask", &full_name])
            .status()
            .is_ok_and(|status| status.success());
        let what = if restored {
            "nextbrew could not install it; reinstalled with Homebrew"
        } else {
            "nextbrew could not install it, and reinstalling with Homebrew failed too"
        };
        kept.push((full_name, what.to_string()));
    }
    ui.blank_line().map_err(ui_error)?;
    ui.heading(format!(
        "Moved {} of {} casks to nextbrew",
        style(moved).green().bold(),
        casks.len()
    ))
    .map_err(ui_error)?;
    if !kept.is_empty() {
        ui.note("Left with Homebrew:").map_err(ui_error)?;
        for (token, why) in kept {
            ui.bullet(format!("{token}: {why}")).map_err(ui_error)?;
        }
    }
    Ok(())
}

/// The other taps (not homebrew/core or homebrew/cask) `packages` come
/// from, each once.
fn other_taps<'a>(packages: impl Iterator<Item = &'a HomebrewPackage>) -> BTreeSet<&'a str> {
    packages
        .filter(|pkg| pkg.from_other_tap())
        .map(|pkg| pkg.tap.as_str())
        .collect()
}

/// Tap each of `taps` with nextbrew, from where Homebrew fetches it, then
/// trust each formula and cask moving from them, as `nb install
/// user/repo/name` does. Returns the formulae and casks that can go on;
/// the rest stay with Homebrew, and why is shown.
async fn prepare_taps<'a>(
    installer: &mut nb_io::Installer,
    taps: &BTreeSet<&str>,
    formulas: Vec<&'a HomebrewPackage>,
    casks: Vec<&'a HomebrewPackage>,
    remotes: &BTreeMap<String, String>,
    ui: &mut StdUi,
) -> Result<(Vec<&'a HomebrewPackage>, Vec<&'a HomebrewPackage>), nb_core::Error> {
    if taps.is_empty() {
        return Ok((formulas, casks));
    }
    ui.blank_line().map_err(ui_error)?;
    let mut failed_taps = BTreeMap::new();
    for tap in taps {
        let tapped = async {
            let name = nb_io::tap::TapName::parse(tap)?;
            nb_io::tap::Taps::check_tappable(&name, false)?;
            super::tap::ensure_tapped(installer, &name, remotes.get(*tap).map(String::as_str)).await
        }
        .await;
        if let Err(e) = tapped {
            failed_taps.insert(tap.to_string(), e.to_string());
        }
    }
    let mut kept: Vec<(String, String)> = Vec::new();
    let mut ready = (Vec::new(), Vec::new());
    for pkg in formulas.into_iter().chain(casks) {
        if pkg.from_other_tap() {
            if let Some(e) = failed_taps.get(&pkg.tap) {
                kept.push((pkg.full_name(), format!("tapping {} failed: {e}", pkg.tap)));
                continue;
            }
            let name = if pkg.is_cask {
                format!("cask:{}", pkg.full_name())
            } else {
                pkg.full_name()
            };
            if let Err(e) = super::tap::prepare_full_names(installer, &[name]).await {
                kept.push((pkg.full_name(), e.to_string()));
                continue;
            }
        }
        if pkg.is_cask {
            ready.1.push(pkg);
        } else {
            ready.0.push(pkg);
        }
    }
    if !kept.is_empty() {
        ui.note("Left with Homebrew:").map_err(ui_error)?;
        for (name, why) in kept {
            ui.bullet(format!("{name}: {why}")).map_err(ui_error)?;
        }
    }
    Ok(ready)
}

/// Restart the migrated formulae's running Homebrew services with
/// nextbrew. Their data and settings are under each prefix's `var` and
/// `etc`, which are not moved.
async fn migrate_services(
    installer: &mut nb_io::Installer,
    services: &[String],
    migrated: &[String],
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    let services: Vec<&String> = services
        .iter()
        .filter(|name| migrated.contains(name))
        .collect();
    if services.is_empty() {
        return Ok(());
    }
    ui.blank_line().map_err(ui_error)?;
    ui.heading("Restarting services with nextbrew...")
        .map_err(ui_error)?;
    for name in &services {
        let _ = Command::new("brew")
            .args(["services", "stop", name])
            .status();
        let start = crate::cli::ServicesCommands::Start {
            formulas: vec![(*name).clone()],
            all: false,
            file: None,
        };
        if let Err(e) = crate::commands::services::execute(installer, Some(start), false).await {
            ui.error(format!("{name}: {e}")).map_err(ui_error)?;
        }
    }
    let brew_prefix = Command::new("brew")
        .arg("--prefix")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|_| "/opt/homebrew".to_string());
    ui.note(format!(
        "Data and settings under {brew_prefix}/var and {brew_prefix}/etc were not moved; nextbrew's services use {}/var and {}/etc. Copy what you need while the service is stopped.",
        installer.prefix().display(),
        installer.prefix().display()
    ))
    .map_err(ui_error)?;
    Ok(())
}

/// The formulae nextbrew now has, and those it does not.
fn check_install_status(
    installer: &nb_io::Installer,
    formula_names: &[String],
) -> Result<(Vec<String>, Vec<String>), nb_core::Error> {
    let installed_names: std::collections::HashSet<String> = installer
        .list_installed()
        .map_err(|e| nb_core::Error::StoreCorruption {
            message: format!("Failed to verify installation status: {}", e),
        })?
        .into_iter()
        .map(|k| k.name)
        .collect();
    // One from another tap must be the one from that tap.
    Ok(formula_names.iter().cloned().partition(|name| {
        if name.contains('/') {
            installer.get_installed_by_full_name(name).is_some()
        } else {
            installed_names.contains(name)
        }
    }))
}

fn ui_error(err: std::io::Error) -> nb_core::Error {
    nb_core::Error::StoreCorruption {
        message: format!("failed to write CLI output: {err}"),
    }
}
