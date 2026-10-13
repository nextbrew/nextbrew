use console::style;

pub async fn execute(installer: &mut nb_io::Installer) -> Result<(), nb_core::Error> {
    let formulas = installer.update_api_cache().await?;
    println!(
        "{} Refreshed the formula index: {} formulas.",
        style("==>").cyan().bold(),
        style(formulas).green().bold()
    );
    println!(
        "{}",
        style("Run `nb outdated` to check package updates.").dim()
    );
    println!(
        "{}",
        style("This does not update the nb binary; use the installer or Homebrew for that.").dim()
    );
    Ok(())
}
