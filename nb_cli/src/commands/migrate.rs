use crate::ui::{PromptDefault, StdUi};
use console::style;
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

    let services: Vec<String> = packages
        .services
        .iter()
        .filter(|name| packages.formulas.iter().any(|f| &f.name == *name))
        .cloned()
        .collect();
    ui.println(format!(
        "{} formulas and {} casks to migrate, {} services to restart",
        style(packages.formulas.len()).green(),
        style(packages.casks.len()).green(),
        style(services.len()).green()
    ))
    .map_err(ui_error)?;
    ui.blank_line().map_err(ui_error)?;

    let foreign: Vec<&nb_io::HomebrewPackage> = packages
        .non_core_formulas
        .iter()
        .chain(&packages.non_core_casks)
        .collect();
    if !foreign.is_empty() {
        ui.note("From taps nextbrew does not support; left with Homebrew:")
            .map_err(ui_error)?;
        for pkg in foreign {
            ui.bullet(format!("{} ({})", pkg.name, pkg.tap))
                .map_err(ui_error)?;
        }
        ui.blank_line().map_err(ui_error)?;
    }

    if packages.formulas.is_empty() && packages.casks.is_empty() {
        ui.println("Nothing to migrate.").map_err(ui_error)?;
        return Ok(());
    }
    if !packages.formulas.is_empty() {
        ui.println("These formulas will be installed with nextbrew:")
            .map_err(ui_error)?;
        for pkg in &packages.formulas {
            let pinned = if pkg.pinned { " (pinned)" } else { "" };
            ui.bullet(format!("{}{pinned}", pkg.name))
                .map_err(ui_error)?;
        }
        ui.blank_line().map_err(ui_error)?;
    }
    if !packages.casks.is_empty() {
        ui.println(
            "These casks will move from Homebrew to nextbrew (downloaded first; their data stays):",
        )
        .map_err(ui_error)?;
        for pkg in &packages.casks {
            ui.bullet(&pkg.name).map_err(ui_error)?;
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

    migrate_casks(installer, &packages.casks, ui).await?;

    if packages.formulas.is_empty() {
        return Ok(());
    }

    ui.blank_line().map_err(ui_error)?;
    ui.heading(format!(
        "Migrating {} formulas to nextbrew...",
        style(packages.formulas.len()).green().bold()
    ))
    .map_err(ui_error)?;

    let formula_names: Vec<String> = packages.formulas.iter().map(|f| f.name.clone()).collect();

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
        packages.formulas.len()
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

    for pkg in packages.formulas.iter().filter(|pkg| pkg.pinned) {
        if successfully_installed.contains(&pkg.name) {
            installer.set_pinned(&pkg.name, true)?;
        }
    }
    migrate_services(installer, &services, &successfully_installed, ui).await?;

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
    let pinned: Vec<&str> = packages
        .formulas
        .iter()
        .filter(|pkg| pkg.pinned && successfully_installed.contains(&pkg.name))
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
        uninstall_failed.retain(|target| still_installed.contains(target.as_str()));
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
    casks: &[nb_io::HomebrewPackage],
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
        if let Err(e) = installer.prepare_cask(token).await {
            kept.push((token.clone(), e.to_string()));
            continue;
        }
        let removed = Command::new("brew")
            .args(["uninstall", "--cask", token])
            .status()
            .is_ok_and(|status| status.success());
        if !removed {
            kept.push((token.clone(), "brew uninstall --cask failed".to_string()));
            continue;
        }
        let name = format!("cask:{token}");
        let _ = crate::commands::install::execute(installer, vec![name.clone()], false, false, ui)
            .await;
        if installer.is_installed(&name) {
            moved += 1;
            continue;
        }
        let restored = Command::new("brew")
            .args(["install", "--cask", token])
            .status()
            .is_ok_and(|status| status.success());
        let what = if restored {
            "nextbrew could not install it; reinstalled with Homebrew"
        } else {
            "nextbrew could not install it, and reinstalling with Homebrew failed too"
        };
        kept.push((token.clone(), what.to_string()));
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

// FIXME: Abstract this return type to a more structured type (e.g., a struct)
fn check_install_status(
    installer: &nb_io::Installer,
    formula_names: &[String],
) -> Result<(Vec<String>, Vec<String>), nb_core::Error> {
    let mut successfully_installed = Vec::new();
    let mut failed_installed = Vec::new();

    let installed_kegs =
        installer
            .list_installed()
            .map_err(|e| nb_core::Error::StoreCorruption {
                message: format!("Failed to verify installation status: {}", e),
            })?;

    let installed_names: std::collections::HashSet<String> =
        installed_kegs.into_iter().map(|k| k.name).collect();
    for name in formula_names {
        if !installed_names.contains(name) {
            failed_installed.push(name.clone());
        } else {
            successfully_installed.push(name.clone());
        }
    }

    Ok((successfully_installed, failed_installed))
}

fn ui_error(err: std::io::Error) -> nb_core::Error {
    nb_core::Error::StoreCorruption {
        message: format!("failed to write CLI output: {err}"),
    }
}
