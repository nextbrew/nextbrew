use console::style;

use crate::ui::StdUi;
use crate::utils::normalize_formula_name;

/// `zb postinstall`: run the post-install steps of installed formulae
/// again, like `brew postinstall`. Useful after a step failed, or for
/// packages installed before zerobrew ran post-install at all.
pub async fn execute(
    installer: &mut zb_io::Installer,
    formulas: Vec<String>,
    ui: &mut StdUi,
) -> Result<(), zb_core::Error> {
    let mut first_error = None;
    for formula in &formulas {
        let name = normalize_formula_name(formula)?;
        ui.step_start(&name).map_err(ui_error)?;
        match installer.post_install(&name).await {
            Ok(notices) => {
                ui.step_ok().map_err(ui_error)?;
                for notice in notices {
                    ui.note(format!("{}: {notice}", style(&name).bold()))
                        .map_err(ui_error)?;
                }
            }
            Err(e) => {
                ui.step_fail().map_err(ui_error)?;
                ui.error(&e).map_err(ui_error)?;
                first_error.get_or_insert(e);
            }
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn ui_error(err: std::io::Error) -> zb_core::Error {
    zb_core::Error::FileError {
        message: format!("failed to write CLI output: {err}"),
    }
}
