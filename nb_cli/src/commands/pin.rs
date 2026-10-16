use console::style;

use crate::utils::normalize_formula_name;

/// `nb pin` / `nb unpin`, with `brew pin`'s messages: an uninstalled
/// formula is an error, one already in the requested state a warning.
pub fn execute(
    installer: &nb_io::Installer,
    formulas: Vec<String>,
    pin: bool,
) -> Result<(), nb_core::Error> {
    for formula in &formulas {
        let name = normalize_formula_name(formula)?;
        if !installer.set_pinned(&name, pin)? {
            let state = if pin { "already pinned" } else { "not pinned" };
            eprintln!("{} {name} {state}", style("Warning:").yellow().bold());
        }
    }
    Ok(())
}
