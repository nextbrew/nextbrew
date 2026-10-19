use std::collections::BTreeSet;

use crate::utils::normalize_formula_name;

/// `nb deps`: like `brew deps`, every runtime dependency sorted, and for
/// several formulae only the ones they share unless `union`.
pub async fn deps(
    installer: &nb_io::Installer,
    formulas: Vec<String>,
    direct: bool,
    union: bool,
) -> Result<(), nb_core::Error> {
    let mut shown: Option<BTreeSet<String>> = None;
    for name in formula_names(&formulas, "deps")? {
        let deps = installer.dependencies(&name, direct).await?;
        shown = Some(match shown {
            None => deps,
            Some(seen) if union => &seen | &deps,
            Some(seen) => &seen & &deps,
        });
    }
    shown
        .into_iter()
        .flatten()
        .for_each(|dep| println!("{dep}"));
    Ok(())
}

/// `nb uses`: formulae and casks that depend on all of `formulas`, as
/// `brew uses` lists them. A name the API does not know gets a warning.
pub async fn uses(
    installer: &nb_io::Installer,
    formulas: Vec<String>,
    scope: nb_io::UsesScope,
) -> Result<(), nb_core::Error> {
    let names = formula_names(&formulas, "uses")?;
    let uses = installer.dependents(&names, scope).await?;
    for name in &uses.missing {
        eprintln!(
            "{} No available formula with the name {name:?}.",
            console::style("Warning:").yellow().bold()
        );
    }
    for name in &uses.dependents {
        println!("{name}");
    }
    if !uses.missing.is_empty() && !uses.dependents.is_empty() {
        return Err(nb_core::Error::InvalidArgument {
            message: "Missing formulae should not have dependents!".to_string(),
        });
    }
    Ok(())
}

fn formula_names(formulas: &[String], command: &str) -> Result<Vec<String>, nb_core::Error> {
    formulas
        .iter()
        .map(|formula| {
            let name = normalize_formula_name(formula)?;
            if name.starts_with("cask:") {
                return Err(nb_core::Error::InvalidArgument {
                    message: format!("nb {command} does not handle casks"),
                });
            }
            Ok(name)
        })
        .collect()
}
