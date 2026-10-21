use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use nb_io::{InstallProgress, ProgressCallback};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::ui::StdUi;
use crate::utils::{
    normalize_formula_name, report_build_failure, resolve_install_name, suggest_homebrew,
    suggest_missing_formula_matches,
};

pub async fn execute(
    installer: &mut nb_io::Installer,
    formulas: Vec<String>,
    no_link: bool,
    build_from_source: bool,
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    let start = Instant::now();

    let mut normalized_names = Vec::new();
    let mut cask_names = Vec::new();
    for formula in &formulas {
        match normalize_formula_name(formula) {
            Ok(name) => {
                let name = resolve_install_name(installer, name).await;
                if name.starts_with("cask:") {
                    cask_names.push(name);
                } else {
                    normalized_names.push(name);
                }
            }
            Err(e) => {
                suggest_homebrew(formula, &e);
                return Err(e);
            }
        }
    }

    // `Install.install_formula?`: an installed formula stays as it is, or is
    // upgraded when outdated.
    let mut to_upgrade = Vec::new();
    let mut fresh = Vec::new();
    for name in normalized_names {
        match already_installed(installer, &name).await {
            Installed::No => fresh.push(name),
            Installed::Upgrade(installed) => to_upgrade.push(installed),
            Installed::Leave => {}
        }
    }
    let normalized_names = fresh;
    if !to_upgrade.is_empty() {
        super::upgrade::execute(installer, to_upgrade, build_from_source, no_link, ui).await?;
    }
    if normalized_names.is_empty() && cask_names.is_empty() {
        return Ok(());
    }
    let names: Vec<&str> = normalized_names
        .iter()
        .chain(&cask_names)
        .map(String::as_str)
        .collect();
    ui.heading(format!("Installing {}...", style(names.join(", ")).bold()))
        .map_err(ui_error)?;

    let mut installed_count = 0usize;
    let mut notices = Vec::new();
    let mut post_install_failures = Vec::new();

    if !normalized_names.is_empty() {
        let plan = match installer
            .plan_with_options(&normalized_names, build_from_source)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                let handled_missing = suggest_missing_formula_matches(installer, &e).await;

                if !handled_missing {
                    for formula in &formulas {
                        suggest_homebrew(formula, &e);
                    }
                }
                return Err(e);
            }
        };

        let result = execute_formula_plan(installer, &formulas, plan, no_link, ui).await?;
        installed_count += result.installed;
        notices.extend(result.notices);
        post_install_failures.extend(result.post_install_failures);
    }

    if !cask_names.is_empty() {
        ui.heading(format!(
            "Installing casks ({} packages)...",
            cask_names.len()
        ))
        .map_err(ui_error)?;
        let result = installer.install_casks(&cask_names, !no_link).await?;
        installed_count += result.installed;
    }

    let elapsed = start.elapsed();
    ui.blank_line().map_err(ui_error)?;
    ui.heading(format!(
        "Installed {} packages in {:.2}s",
        style(installed_count).green().bold(),
        elapsed.as_secs_f64()
    ))
    .map_err(ui_error)?;

    report_post_install(ui, &notices, post_install_failures)
}

/// What `nb install` does with a formula that may be installed already.
enum Installed {
    No,
    /// Upgrade it (by its installed name).
    Upgrade(String),
    /// It is there as it should be, which has been said.
    Leave,
}

/// `Install.install_formula?` for `name`: the current version installed is
/// left alone (said with a warning, and marked as asked for); an older one
/// is upgraded, unless pinned or `$HOMEBREW_NO_INSTALL_UPGRADE` is set,
/// which is an error that, as Homebrew's, does not fail the command.
async fn already_installed(installer: &nb_io::Installer, name: &str) -> Installed {
    let Some(keg) = installer.get_installed_by_full_name(name) else {
        return Installed::No;
    };
    let Ok(formula) = installer.formula(name).await else {
        return Installed::No;
    };
    let full_name = match formula.third_party_tap() {
        Some(tap) => format!("{tap}/{}", formula.name),
        None => formula.name.clone(),
    };
    let current = formula.effective_version();
    let keg_only = formula.is_keg_only();
    let linked = installer.is_linked(&keg.name).unwrap_or(false);
    let warn = |message: String| {
        eprintln!("{} {message}", style("Warning:").yellow().bold());
    };
    if keg.version == current {
        if keg_only || linked {
            warn(format!(
                "{full_name} {current} is already installed and up-to-date.\n\
                 To reinstall {current}, run:\n  nb reinstall {}",
                formula.name
            ));
        } else {
            warn(format!(
                "{full_name} {current} is already installed, it's just not linked.\n\
                 To link this version, run:\n  nb link {full_name}"
            ));
        }
        let _ = installer.mark_on_request(std::slice::from_ref(&keg.name));
        return Installed::Leave;
    }
    if !keg_only && !linked {
        return Installed::Upgrade(keg.name);
    }
    let no_upgrade = std::env::var_os("HOMEBREW_NO_INSTALL_UPGRADE").is_some_and(|v| !v.is_empty());
    if !no_upgrade && !keg.pinned {
        println!(
            "{} {} is already installed but outdated (so it will be upgraded).",
            formula.name, keg.version
        );
        return Installed::Upgrade(keg.name);
    }
    let unpin = if keg.pinned {
        format!("nb unpin {full_name} && ")
    } else {
        String::new()
    };
    let installed = if keg_only {
        format!("{full_name} {} is already installed.", keg.version)
    } else {
        format!("{} {} is already installed", formula.name, keg.version)
    };
    eprintln!(
        "{} {installed}\nTo upgrade to {current}, run:\n  {unpin}nb upgrade {full_name}",
        style("Error:").red().bold()
    );
    Installed::Leave
}

/// Print post-install notices and failures. Every package is installed and
/// linked whatever happened in post-install, so this comes after the
/// summary, and a failure still makes the command fail.
pub fn report_post_install(
    ui: &mut StdUi,
    notices: &[nb_io::PostInstallNotice],
    mut failures: Vec<nb_core::Error>,
) -> Result<(), nb_core::Error> {
    for notice in notices {
        ui.note(format!(
            "{}: {}",
            style(&notice.name).bold(),
            notice.message
        ))
        .map_err(ui_error)?;
    }
    let Some(last) = failures.pop() else {
        return Ok(());
    };
    ui.blank_line().map_err(ui_error)?;
    for failure in &failures {
        ui.error(failure).map_err(ui_error)?;
    }
    ui.println("The packages are installed, but their post-install steps did not finish.")
        .map_err(ui_error)?;
    ui.println(format!(
        "Retry with {}",
        style("nb postinstall <formula>").bold()
    ))
    .map_err(ui_error)?;
    Err(last)
}

/// The progress bar key for a package's post-install, distinct from its
/// install bar, which is finished by then.
fn post_install_key(name: &str) -> String {
    format!("{name} post-install")
}

pub async fn execute_formula_plan(
    installer: &mut nb_io::Installer,
    requested_formulas: &[String],
    plan: nb_io::InstallPlan,
    no_link: bool,
    ui: &mut StdUi,
) -> Result<nb_io::ExecuteResult, nb_core::Error> {
    ui.heading(format!(
        "Resolving dependencies ({} packages)...",
        plan.items.len()
    ))
    .map_err(ui_error)?;
    for item in &plan.items {
        ui.bullet(format!(
            "{} {}",
            style(&item.formula.name).green(),
            style(&item.formula.versions.stable).dim()
        ))
        .map_err(ui_error)?;
    }

    let multi = MultiProgress::new();
    let bars: Arc<Mutex<HashMap<String, ProgressBar>>> = Arc::new(Mutex::new(HashMap::new()));

    let download_style = ProgressStyle::default_bar()
        .template("    {prefix:<16} {bar:25.cyan/dim} {bytes:>10}/{total_bytes:<10} {eta:>6}")
        .unwrap()
        .progress_chars("━━╸");

    let spinner_style = ProgressStyle::default_spinner()
        .template("    {prefix:<16} {spinner:.cyan} {msg}")
        .unwrap()
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");

    let done_style = ProgressStyle::default_spinner()
        .template("    {prefix:<16} {msg}")
        .unwrap();

    ui.heading("Downloading and installing formulas...")
        .map_err(ui_error)?;

    let bars_clone = bars.clone();
    let multi_clone = multi.clone();
    let download_style_clone = download_style.clone();
    let spinner_style_clone = spinner_style.clone();
    let done_style_clone = done_style.clone();

    let progress_callback: Arc<ProgressCallback> = Arc::new(Box::new(move |event| {
        let mut bars = bars_clone.lock().unwrap();
        match event {
            InstallProgress::DownloadStarted { name, total_bytes } => {
                let pb = if let Some(total) = total_bytes {
                    let pb = multi_clone.add(ProgressBar::new(total));
                    pb.set_style(download_style_clone.clone());
                    pb
                } else {
                    let pb = multi_clone.add(ProgressBar::new_spinner());
                    pb.set_style(spinner_style_clone.clone());
                    pb.set_message("downloading...");
                    pb.enable_steady_tick(std::time::Duration::from_millis(80));
                    pb
                };
                pb.set_prefix(name.clone());
                bars.insert(name, pb);
            }
            InstallProgress::DownloadProgress {
                name,
                downloaded,
                total_bytes,
            } => {
                if let Some(pb) = bars.get(&name)
                    && total_bytes.is_some()
                {
                    pb.set_position(downloaded);
                }
            }
            InstallProgress::DownloadCompleted { name, total_bytes } => {
                if let Some(pb) = bars.get(&name) {
                    if total_bytes > 0 {
                        pb.set_position(total_bytes);
                    }
                    pb.set_style(spinner_style_clone.clone());
                    pb.set_message("unpacking...");
                    pb.enable_steady_tick(std::time::Duration::from_millis(80));
                }
            }
            InstallProgress::UnpackStarted { name } => {
                if let Some(pb) = bars.get(&name) {
                    pb.set_message("unpacking...");
                }
            }
            InstallProgress::UnpackCompleted { name } => {
                if let Some(pb) = bars.get(&name) {
                    pb.set_message("unpacked");
                }
            }
            InstallProgress::LinkStarted { name } => {
                if let Some(pb) = bars.get(&name) {
                    pb.set_message("linking...");
                }
            }
            InstallProgress::LinkCompleted { name } => {
                if let Some(pb) = bars.get(&name) {
                    pb.set_message("linked");
                }
            }
            InstallProgress::Unlinked { keg, count } => {
                let _ =
                    multi_clone.println(format!("Unlinking {keg}... {count} symlinks removed."));
            }
            InstallProgress::LinkSkipped { name, reason } => {
                if let Some(pb) = bars.get(&name) {
                    pb.set_message(reason);
                }
            }
            InstallProgress::InstallCompleted { name } => {
                if let Some(pb) = bars.get(&name) {
                    pb.set_style(done_style_clone.clone());
                    pb.set_message(format!("{} installed", style("✓").green()));
                    pb.finish();
                }
            }
            InstallProgress::PostInstallStarted { name } => {
                let pb = multi_clone.add(ProgressBar::new_spinner());
                pb.set_style(spinner_style_clone.clone());
                pb.set_prefix(name.clone());
                pb.set_message("running post-install...");
                pb.enable_steady_tick(std::time::Duration::from_millis(80));
                bars.insert(post_install_key(&name), pb);
            }
            InstallProgress::PostInstallCompleted { name } => {
                if let Some(pb) = bars.get(&post_install_key(&name)) {
                    pb.set_style(done_style_clone.clone());
                    pb.set_message(format!("{} post-install done", style("✓").green()));
                    pb.finish();
                }
            }
            InstallProgress::PostInstallFailed { name, .. } => {
                if let Some(pb) = bars.get(&post_install_key(&name)) {
                    pb.set_style(done_style_clone.clone());
                    pb.set_message(format!("{} post-install failed", style("✗").red()));
                    pb.finish();
                }
            }
        }
    }));

    let result_val = installer
        .execute_with_progress(plan, !no_link, Some(progress_callback))
        .await;

    {
        let bars = bars.lock().unwrap();
        for (_, pb) in bars.iter() {
            if !pb.is_finished() {
                pb.finish();
            }
        }
    }

    match result_val {
        Ok(result) => Ok(result),
        Err(ref e @ nb_core::Error::LinkConflict { ref conflicts }) => {
            ui.blank_line().map_err(ui_error)?;
            ui.error("The link step did not complete successfully.")
                .map_err(ui_error)?;
            ui.println("The formula was installed, but is not symlinked into the prefix.")
                .map_err(ui_error)?;
            ui.blank_line().map_err(ui_error)?;
            ui.println("Possible conflicting files:")
                .map_err(ui_error)?;
            for c in conflicts {
                if let Some(ref owner) = c.owned_by {
                    ui.println(format!(
                        "  {} (symlink belonging to {})",
                        c.path.display(),
                        style(owner).yellow()
                    ))
                    .map_err(ui_error)?;
                } else {
                    ui.println(format!("  {}", c.path.display()))
                        .map_err(ui_error)?;
                }
            }
            ui.blank_line().map_err(ui_error)?;
            Err(e.clone())
        }
        Err(e) => {
            report_build_failure(&e);
            let handled_missing = suggest_missing_formula_matches(installer, &e).await;

            if !handled_missing {
                for formula in requested_formulas {
                    suggest_homebrew(formula, &e);
                }
            }
            Err(e)
        }
    }
}

fn ui_error(err: std::io::Error) -> nb_core::Error {
    nb_core::Error::FileError {
        message: format!("failed to write CLI output: {err}"),
    }
}
