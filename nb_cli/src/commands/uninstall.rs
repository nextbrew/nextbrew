use crate::ui::StdUi;
use crate::utils::normalize_formula_name;
use console::style;

pub fn execute(
    installer: &mut nb_io::Installer,
    formulas: Vec<String>,
    all: bool,
    zap: bool,
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    let formulas = if all {
        let installed = installer.list_installed()?;
        if installed.is_empty() {
            ui.info("No formulas installed.").map_err(ui_error)?;
            return Ok(());
        }
        installed.into_iter().map(|k| k.name).collect()
    } else {
        let mut normalized = Vec::with_capacity(formulas.len());
        for formula in formulas {
            normalized.push(normalize_formula_name(&formula)?);
        }
        normalized
    };

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
        Ok(())
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
