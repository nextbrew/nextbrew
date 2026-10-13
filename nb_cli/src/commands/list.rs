use console::style;

pub fn execute(installer: &mut nb_io::Installer) -> Result<(), nb_core::Error> {
    let installed = installer.list_installed()?;

    if installed.is_empty() {
        println!("No formulas installed.");
    } else {
        for keg in installed {
            println!("{} {}", style(&keg.name).bold(), style(&keg.version).dim());
        }
    }

    Ok(())
}
