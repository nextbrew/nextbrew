//! `nb reinstall`: as `brew reinstall`, each installed package is put in
//! again at its latest version, keeping whether it was asked for and, for
//! a keg-only formula, whether it was linked. A package not installed is
//! installed; a pinned formula is left alone with an error; a name that is
//! neither a formula nor a cask fails the command once the rest are done.

use console::style;

use super::upgrade::{Replace, replace_packages};
use crate::ui::StdUi;
use crate::utils::{normalize_formula_name, resolve_install_name, resolve_installed_name};

pub async fn execute(
    installer: &mut nb_io::Installer,
    names: Vec<String>,
    build_from_source: bool,
    zap: bool,
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    let mut installed = Vec::new();
    let mut fresh = Vec::new();
    let mut unavailable = None;
    for name in names {
        let name = if name.starts_with("formula:") {
            name
        } else {
            normalize_formula_name(&name)?
        };
        // `--formula` keeps a bare name from meaning an installed cask.
        let current = match name.strip_prefix("formula:") {
            Some(formula) => formula.to_string(),
            None => resolve_installed_name(installer, name.clone()),
        };
        if installer.is_installed(&current) {
            if installer
                .get_installed(&current)
                .is_some_and(|keg| keg.pinned)
            {
                eprintln!(
                    "{} {current} is pinned. You must unpin it to reinstall.",
                    style("Error:").red().bold()
                );
                continue;
            }
            installed.push(current);
            continue;
        }
        let wanted = resolve_install_name(installer, name).await;
        let exists = match wanted.strip_prefix("cask:") {
            Some(token) => installer.cask(token).await.map(|_| ()),
            None => installer.formula(&wanted).await.map(|_| ()),
        };
        match exists {
            Ok(()) => fresh.push(wanted),
            Err(e) => {
                unavailable.get_or_insert(e);
            }
        }
    }

    if !fresh.is_empty() {
        super::install::execute(installer, fresh, false, build_from_source, ui).await?;
    }
    if !installed.is_empty() {
        replace_packages(
            installer,
            &installed,
            Replace::Reinstall { zap },
            build_from_source,
            false,
            unavailable,
            ui,
        )
        .await
    } else {
        unavailable.map_or(Ok(()), Err)
    }
}
