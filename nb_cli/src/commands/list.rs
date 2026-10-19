use console::style;

pub fn execute(installer: &mut nb_io::Installer, pinned: bool) -> Result<(), nb_core::Error> {
    let installed = installer.list_installed()?;

    if pinned {
        // `brew list --pinned`: names only, nothing when none are pinned.
        let mut names: Vec<String> = installed
            .into_iter()
            .filter(|keg| keg.pinned)
            .map(|keg| keg.name)
            .collect();
        names.sort();
        names.iter().for_each(|name| println!("{name}"));
        return Ok(());
    }

    if installed.is_empty() {
        println!("No formulas installed.");
    } else {
        for keg in installed {
            println!("{} {}", style(&keg.name).bold(), style(&keg.version).dim());
        }
    }

    Ok(())
}
