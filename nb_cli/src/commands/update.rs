use console::style;

pub async fn execute(installer: &mut nb_io::Installer) -> Result<(), nb_core::Error> {
    let formulas = installer.update_api_cache().await?;
    println!(
        "{} Refreshed the formula index: {} formulas.",
        style("==>").cyan().bold(),
        style(formulas).green().bold()
    );
    // `Updated 2 taps (a/b and c/d).`, as `brew update` reports them.
    let taps = installer.update_taps().await?;
    if !taps.is_empty() {
        let names: Vec<String> = taps.iter().map(ToString::to_string).collect();
        let list = match names.as_slice() {
            [one] => one.clone(),
            [init @ .., last] => format!("{} and {last}", init.join(", ")),
            [] => String::new(),
        };
        println!(
            "Updated {} tap{} ({list}).",
            names.len(),
            if names.len() == 1 { "" } else { "s" }
        );
    }
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
