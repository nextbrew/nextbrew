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

/// `nb uses --installed`: installed formulae that depend on all of
/// `formulas`, directly or with `recursive` through other dependencies.
pub async fn uses(
    installer: &nb_io::Installer,
    formulas: Vec<String>,
    installed: bool,
    recursive: bool,
) -> Result<(), nb_core::Error> {
    if !installed {
        return Err(nb_core::Error::InvalidArgument {
            message: "nb uses only searches installed formulae; pass --installed".to_string(),
        });
    }
    let names = formula_names(&formulas, "uses")?;
    for name in installer.installed_dependents(&names, recursive).await? {
        println!("{name}");
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
