use std::io::IsTerminal;

use console::style;

/// `nb search`: matching formulae and casks under `brew search`'s headings,
/// installed ones marked with a check.
pub async fn execute(
    installer: &nb_io::Installer,
    query: String,
    formula_only: bool,
    cask_only: bool,
) -> Result<(), nb_core::Error> {
    let formulae = if cask_only {
        Vec::new()
    } else {
        installer.search_formulas(&query).await?
    };
    // Casks are macOS-only in nextbrew.
    let casks = if formula_only || !cfg!(target_os = "macos") {
        Vec::new()
    } else {
        installer.search_casks(&query).await?
    };
    if formulae.is_empty() && casks.is_empty() {
        return Err(nb_core::Error::InvalidArgument {
            message: format!("No formulae or casks found for {query:?}."),
        });
    }
    // brew prints the headings and checks only to a terminal.
    let terminal = std::io::stdout().is_terminal();
    let print = |heading: &str, names: &[String], prefix: &str| {
        if terminal {
            println!("{} {}", style("==>").blue().bold(), style(heading).bold());
        }
        for name in names {
            if terminal && installer.is_installed(&format!("{prefix}{name}")) {
                println!("{name} {}", style("✔").green());
            } else {
                println!("{name}");
            }
        }
    };
    if !formulae.is_empty() {
        print("Formulae", &formulae, "");
    }
    if !casks.is_empty() {
        if !formulae.is_empty() {
            println!();
        }
        print("Casks", &casks, "cask:");
    }
    Ok(())
}
