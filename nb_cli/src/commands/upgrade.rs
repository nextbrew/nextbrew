use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use nb_io::{InstallProgress, ProgressCallback};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::ui::StdUi;
use crate::utils::{normalize_formula_name, resolve_installed_name};

pub async fn execute(
    installer: &mut nb_io::Installer,
    formulas: Vec<String>,
    build_from_source: bool,
    no_link: bool,
    greedy: nb_io::Greedy,
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    // Reported at the end so explicit-arg upgrades still run; we exit
    // non-zero afterwards rather than discard partial progress.
    let mut missing: Vec<String> = Vec::new();

    let outdated = if formulas.is_empty() {
        ui.heading("Checking for outdated packages...".to_string())
            .map_err(ui_error)?;
        let (outdated, warnings) = installer.check_outdated(greedy).await?;
        for warning in &warnings {
            eprintln!("{} {}", style("Warning:").yellow().bold(), warning);
        }
        if outdated.is_empty() {
            ui.info("All packages are up to date.".to_string())
                .map_err(ui_error)?;
            return Ok(());
        }
        outdated
    } else {
        let mut normalized = Vec::with_capacity(formulas.len());
        for formula in &formulas {
            normalized.push(resolve_installed_name(
                installer,
                normalize_formula_name(formula)?,
            ));
        }
        let mut outdated = Vec::new();
        for name in &normalized {
            match installer.is_outdated(name).await {
                Ok(Some(pkg)) => outdated.push(pkg),
                Ok(None) => {
                    ui.info(format!("{} is already up to date", name))
                        .map_err(ui_error)?;
                }
                Err(nb_core::Error::NotInstalled { .. }) => {
                    ui.error(format!("{} is not installed", name))
                        .map_err(ui_error)?;
                    missing.push(name.clone());
                }
                Err(e) => return Err(e),
            }
        }
        if outdated.is_empty() {
            if let Some(name) = missing.into_iter().next() {
                return Err(nb_core::Error::NotInstalled { name });
            }
            ui.info("All specified packages are up to date.".to_string())
                .map_err(ui_error)?;
            return Ok(());
        }
        outdated
    };

    // As `brew upgrade`: pinned packages are skipped with a warning, and
    // the command fails only when they were named.
    let (pinned, outdated): (Vec<_>, Vec<_>) = outdated.into_iter().partition(|pkg| {
        installer
            .get_installed(&pkg.name)
            .is_some_and(|keg| keg.pinned)
    });
    let mut pinned_error = None;
    if !pinned.is_empty() {
        let message = format!(
            "Not upgrading {} pinned package{}: {}",
            pinned.len(),
            if pinned.len() == 1 { "" } else { "s" },
            pinned
                .iter()
                .map(|pkg| format!("{} {}", pkg.name, pkg.installed_version))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if formulas.is_empty() {
            eprintln!("{} {}", style("Warning:").yellow().bold(), message);
        } else {
            pinned_error = Some(nb_core::Error::InvalidArgument { message });
        }
    }
    if outdated.is_empty() {
        return match (missing.into_iter().next(), pinned_error) {
            (Some(name), _) => Err(nb_core::Error::NotInstalled { name }),
            (None, Some(e)) => Err(e),
            (None, None) => Ok(()),
        };
    }

    let names: Vec<String> = outdated.into_iter().map(|pkg| pkg.name).collect();
    let pending = missing
        .into_iter()
        .next()
        .map(|name| nb_core::Error::NotInstalled { name })
        .or(pinned_error);
    replace_packages(
        installer,
        &names,
        Replace::Upgrade,
        build_from_source,
        no_link,
        pending,
        ui,
    )
    .await
}

/// What `replace_packages` does to each package.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Replace {
    Upgrade,
    /// With `--zap` for casks or not.
    Reinstall {
        zap: bool,
    },
}

impl Replace {
    fn verb(self) -> &'static str {
        match self {
            Replace::Upgrade => "Upgrading",
            Replace::Reinstall { .. } => "Reinstalling",
        }
    }

    fn action(self) -> &'static str {
        match self {
            Replace::Upgrade => "upgrade",
            Replace::Reinstall { .. } => "reinstall",
        }
    }

    fn done(self) -> &'static str {
        match self {
            Replace::Upgrade => "upgraded",
            Replace::Reinstall { .. } => "reinstalled",
        }
    }
}

/// Upgrade or reinstall installed packages one by one with a progress
/// display, then report. `pending` is an earlier failure to return once
/// the rest are done.
pub(crate) async fn replace_packages(
    installer: &mut nb_io::Installer,
    names: &[String],
    mode: Replace,
    build_from_source: bool,
    no_link: bool,
    pending: Option<nb_core::Error>,
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    let start = Instant::now();
    ui.heading(format!("{} {}...", mode.verb(), style(names.len()).bold()))
        .map_err(ui_error)?;

    let multi = crate::logging::progress();
    let bars: Arc<Mutex<HashMap<String, ProgressBar>>> = Arc::new(Mutex::new(HashMap::new()));

    let spinner_style = ProgressStyle::default_spinner()
        .template("    {prefix:<16} {spinner:.cyan} {msg}")
        .unwrap()
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");

    let done_style = ProgressStyle::default_spinner()
        .template("    {prefix:<16} {msg}")
        .unwrap();

    let bars_clone = bars.clone();
    let multi_clone = multi.clone();
    let spinner_style_clone = spinner_style.clone();
    let done_style_clone = done_style.clone();

    let progress_callback: Arc<ProgressCallback> = Arc::new(Box::new(move |event| {
        let mut bars = bars_clone.lock().unwrap();
        match event {
            InstallProgress::DownloadStarted { name, total_bytes } => {
                let pb = if let Some(total) = total_bytes {
                    let pb = multi_clone.add(ProgressBar::new(total));
                    pb.set_style(
                        ProgressStyle::default_bar()
                            .template("    {prefix:<16} {bar:25.cyan/dim} {bytes:>10}/{total_bytes:<10} {eta:>6}")
                            .unwrap()
                            .progress_chars("━━╸"),
                    );
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
                    pb.set_message(format!("{} {}", style("✓").green(), mode.done()));
                    pb.finish();
                }
            }
            InstallProgress::PostInstallStarted { name } => {
                let pb = multi_clone.add(ProgressBar::new_spinner());
                pb.set_style(spinner_style_clone.clone());
                pb.set_prefix(name.clone());
                pb.set_message("running post-install...");
                pb.enable_steady_tick(std::time::Duration::from_millis(80));
                bars.insert(format!("{name} post-install"), pb);
            }
            InstallProgress::PostInstallCompleted { name } => {
                if let Some(pb) = bars.get(&format!("{name} post-install")) {
                    pb.set_style(done_style_clone.clone());
                    pb.set_message(format!("{} post-install done", style("✓").green()));
                    pb.finish();
                }
            }
            InstallProgress::PostInstallFailed { name, .. } => {
                if let Some(pb) = bars.get(&format!("{name} post-install")) {
                    pb.set_style(done_style_clone.clone());
                    pb.set_message(format!("{} post-install failed", style("✗").red()));
                    pb.finish();
                }
            }
            InstallProgress::Failed { error } => {
                multi_clone.suspend(|| crate::utils::report_failure(&error));
            }
        }
    }));

    let mut upgraded = 0usize;
    let mut errors: Vec<(String, nb_core::Error)> = Vec::new();
    let mut notices = Vec::new();
    let mut post_install_failures = Vec::new();
    let mut messages = crate::caveats::Messages::default();

    // Every download at once first; each swap below then reads the cache.
    // A failure here is the swap's to report.
    if names.len() > 1 {
        let _ = installer
            .prefetch(names, build_from_source, Some(progress_callback.clone()))
            .await;
        for (_, pb) in bars.lock().unwrap().drain() {
            pb.finish_and_clear();
            multi.remove(&pb);
        }
    }

    for name in names {
        let progress = Some(progress_callback.clone());
        let result = match mode {
            Replace::Upgrade => {
                installer
                    .upgrade(name, build_from_source, !no_link, progress)
                    .await
            }
            Replace::Reinstall { zap } => {
                installer
                    .reinstall(name, build_from_source, !no_link, zap, progress)
                    .await
            }
        };
        // The bars only show the work in progress, on stderr; the step line
        // on stdout is what stays. Written after them, so they don't
        // redraw over it.
        for (_, pb) in bars.lock().unwrap().drain() {
            pb.finish_and_clear();
            multi.remove(&pb);
        }
        // Around the bars, which otherwise redraw over what stdout wrote.
        let ok = result.is_ok();
        multi
            .suspend(|| {
                ui.step_start(name)?;
                if ok { ui.step_ok() } else { ui.step_fail() }
            })
            .map_err(ui_error)?;
        match result {
            Ok(result) => {
                upgraded += 1;
                match name.strip_prefix("cask:") {
                    Some(token) => messages.record_cask(installer, token).await,
                    None => messages.record_installed(installer).await,
                }
                notices.extend(result.notices);
                post_install_failures.extend(result.post_install_failures);
            }
            Err(e) => {
                messages.record_installed(installer).await;
                errors.push((name.clone(), e));
            }
        }
    }

    let elapsed = start.elapsed();
    ui.blank_line().map_err(ui_error)?;

    for (name, err) in &errors {
        ui.error(format!(
            "Failed to {} {}: {}",
            mode.action(),
            style(name).bold(),
            err
        ))
        .map_err(ui_error)?;
        crate::utils::report_build_failure(err);
    }

    if errors.is_empty() && pending.is_none() {
        ui.heading(format!(
            "{} {} packages in {:.2}s",
            match mode {
                Replace::Upgrade => "Upgraded",
                Replace::Reinstall { .. } => "Reinstalled",
            },
            style(upgraded).green().bold(),
            elapsed.as_secs_f64()
        ))
        .map_err(ui_error)?;
        super::install::show_caveats(ui, &messages)?;
        super::install::report_post_install(ui, &notices, post_install_failures)
    } else if !errors.is_empty() {
        super::install::show_caveats(ui, &messages)?;
        Err(errors.remove(0).1)
    } else {
        super::install::show_caveats(ui, &messages)?;
        Err(pending.expect("a pending failure"))
    }
}

fn ui_error(err: std::io::Error) -> nb_core::Error {
    nb_core::Error::FileError {
        message: format!("failed to write CLI output: {err}"),
    }
}
