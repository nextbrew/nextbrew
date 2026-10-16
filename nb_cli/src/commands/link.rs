use console::style;
use nb_io::LinkOutcome;

use crate::utils::normalize_formula_name;

/// `nb link`, with `brew link`'s messages.
pub async fn link(
    installer: &mut nb_io::Installer,
    formulas: Vec<String>,
    force: bool,
) -> Result<(), nb_core::Error> {
    for formula in &formulas {
        let name = normalize_formula_name(formula)?;
        let (keg, _) = installer.formula_keg(&name)?;
        let keg = keg.display();
        match installer.link(&name, force).await? {
            LinkOutcome::Linked(n) => println!("Linking {keg}... {n} symlinks created."),
            LinkOutcome::AlreadyLinked => {
                warn(format!("Already linked: {keg}"));
                println!("To relink, run:\n  nb unlink {name} && nb link {name}");
            }
            LinkOutcome::KegOnly => warn(format!(
                "{name} is keg-only and must be linked with `--force`."
            )),
        }
    }
    Ok(())
}

/// `nb unlink`, with `brew unlink`'s messages.
pub fn unlink(
    installer: &mut nb_io::Installer,
    formulas: Vec<String>,
) -> Result<(), nb_core::Error> {
    for formula in &formulas {
        let name = normalize_formula_name(formula)?;
        let (keg, _) = installer.formula_keg(&name)?;
        let n = installer.unlink(&name)?;
        println!("Unlinking {}... {n} symlinks removed.", keg.display());
    }
    Ok(())
}

fn warn(message: String) {
    eprintln!("{} {message}", style("Warning:").yellow().bold());
}
