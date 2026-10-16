use crate::ui::StdUi;

/// `nb autoremove`: uninstall formulae that were only installed as
/// dependencies and are no longer needed, with `brew autoremove`'s output.
pub async fn execute(
    installer: &mut nb_io::Installer,
    dry_run: bool,
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    let unneeded = installer.unneeded_formulae().await?;
    if unneeded.is_empty() {
        return Ok(());
    }
    let verb = if dry_run {
        "Would autoremove"
    } else {
        "Autoremoving"
    };
    let plural = if unneeded.len() == 1 { "" } else { "e" };
    ui.heading(format!(
        "{verb} {} unneeded formula{plural}:",
        unneeded.len()
    ))
    .map_err(ui_error)?;
    for name in &unneeded {
        ui.println(name).map_err(ui_error)?;
    }
    if dry_run {
        return Ok(());
    }
    super::uninstall::execute(installer, unneeded, false, false, ui)
}

/// The autoremove `nb uninstall` ends with. Its failure, such as the API
/// being unreachable, does not fail the uninstall that already happened.
pub async fn after_uninstall(installer: &mut nb_io::Installer, ui: &mut StdUi) {
    if let Err(e) = execute(installer, false, ui).await {
        eprintln!(
            "{} could not autoremove unneeded formulae: {e}",
            console::style("Warning:").yellow().bold()
        );
    }
}

fn ui_error(err: std::io::Error) -> nb_core::Error {
    nb_core::Error::FileError {
        message: format!("failed to write CLI output: {err}"),
    }
}
