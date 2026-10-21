use crate::ui::StdUi;
use crate::utils::{normalize_formula_name, resolve_installed_name};
use console::style;

/// `nb uninstall`. Unless `ignore_dependencies`, refuses, as `brew
/// uninstall` does, to remove a formula another installed package needs
/// (removing nothing), or a cask another installed cask needs (removing
/// the formulae only). A pinned formula is left with an error, unless
/// `force`, which also passes over the names not installed.
pub async fn execute(
    installer: &mut nb_io::Installer,
    formulas: Vec<String>,
    all: bool,
    zap: bool,
    ignore_dependencies: bool,
    force: bool,
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    let named_args = formulas.clone();
    let mut formulas = if all {
        let installed = installer.list_installed()?;
        if installed.is_empty() {
            ui.info("No formulas installed.").map_err(ui_error)?;
            return Ok(());
        }
        installed.into_iter().map(|k| k.name).collect()
    } else {
        let mut normalized = Vec::with_capacity(formulas.len());
        for formula in formulas {
            normalized.push(resolve_installed_name(
                installer,
                normalize_formula_name(&formula)?,
            ));
        }
        normalized
    };
    if force {
        formulas.retain(|name| installer.is_installed(name));
    }

    let mut refused_casks = None;
    if !all && !ignore_dependencies {
        let casks = installer.cask_dependents(&formulas)?;
        if let Some(blocked) = installer.formula_dependents(&formulas).await? {
            let mut message = blocked.message(&named_args);
            if let Some(casks) = casks {
                message = format!("{message}\n{}", casks.message(&named_args));
            }
            return Err(nb_core::Error::ExecutionError { message });
        }
        if let Some(casks) = casks {
            formulas.retain(|name| !name.starts_with("cask:"));
            refused_casks = Some(nb_core::Error::ExecutionError {
                message: casks.message(&named_args),
            });
        }
    }
    // `Uninstall.uninstall_kegs`: an error, but not a failure.
    if !force {
        formulas.retain(|name| {
            let pinned = !name.starts_with("cask:")
                && installer.get_installed(name).is_some_and(|keg| keg.pinned);
            if pinned {
                eprintln!(
                    "{} {} is pinned. You must unpin it to uninstall.",
                    style("Error:").red().bold(),
                    installer.installed_full_name(name)
                );
            }
            !pinned
        });
    }
    if formulas.is_empty() {
        return refused_casks.map_or(Ok(()), Err);
    }

    // Homebrew reads a tap's cask from its file while the tap is there, so
    // the file's deprecation warnings show here too.
    if let Ok(source) = installer.tap_source() {
        for name in &formulas {
            if let Some(token) = name.strip_prefix("cask:")
                && let Some(tap) = installer.get_installed(name).and_then(|keg| keg.tap)
                && let Ok(tap) = nb_io::tap::TapName::parse(&tap)
                && source.taps.is_installed(&tap)
            {
                let _ = source.cask(&tap, token);
            }
        }
    }

    ui.heading(format!(
        "Uninstalling {}...",
        style(formulas.join(", ")).bold()
    ))
    .map_err(ui_error)?;

    let mut errors: Vec<(String, nb_core::Error)> = Vec::new();
    let uninstall = |installer: &mut nb_io::Installer, name: &str| {
        if zap && name.starts_with("cask:") {
            installer.zap(name)
        } else {
            installer.uninstall(name)
        }
    };

    if formulas.len() > 1 {
        for name in &formulas {
            ui.step_start(name).map_err(ui_error)?;
            match uninstall(installer, name) {
                Ok(()) => ui.step_ok().map_err(ui_error)?,
                Err(e) => {
                    ui.step_fail().map_err(ui_error)?;
                    errors.push((name.clone(), e));
                }
            }
        }
    } else if let Err(e) = uninstall(installer, &formulas[0]) {
        errors.push((formulas[0].clone(), e));
    }

    if errors.is_empty() {
        refused_casks.map_or(Ok(()), Err)
    } else {
        for (name, err) in &errors {
            ui.error(format!(
                "Failed to uninstall {}: {}",
                style(name).bold(),
                err
            ))
            .map_err(ui_error)?;
        }
        // Return just the first error up. TODO: don't return errors from this fn?
        Err(errors.remove(0).1)
    }
}

fn ui_error(err: std::io::Error) -> nb_core::Error {
    nb_core::Error::StoreCorruption {
        message: format!("failed to write CLI output: {err}"),
    }
}
