use console::style;

/// `brew list`'s `--formula`, `--cask`, `--versions` and `-1`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Listing {
    pub formula: bool,
    pub cask: bool,
    pub versions: bool,
    pub one: bool,
}

impl Listing {
    fn brew_format(self) -> bool {
        self.formula || self.cask || self.versions || self.one
    }
}

pub fn execute(
    installer: &mut nb_io::Installer,
    pinned: bool,
    listing: Listing,
) -> Result<(), nb_core::Error> {
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

    if listing.brew_format() {
        let packages: Vec<(String, String)> = installed
            .into_iter()
            .map(|keg| (keg.name, keg.version))
            .collect();
        lines(&packages, listing)
            .iter()
            .for_each(|line| println!("{line}"));
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

/// What `brew list` prints for `listing` when not writing to a terminal:
/// formulae, then casks (unless only one kind is asked for), sorted, each
/// as its name, or with `--versions` its name and version.
fn lines(packages: &[(String, String)], listing: Listing) -> Vec<String> {
    let mut formulae = Vec::new();
    let mut casks = Vec::new();
    for (name, version) in packages {
        match name.strip_prefix("cask:") {
            Some(token) => casks.push((token, version)),
            None => formulae.push((name.as_str(), version)),
        }
    }
    formulae.sort();
    casks.sort();
    let mut shown = Vec::new();
    if !listing.cask {
        shown.extend(formulae);
    }
    if !listing.formula {
        shown.extend(casks);
    }
    shown
        .into_iter()
        .map(|(name, version)| {
            if listing.versions {
                format!("{name} {version}")
            } else {
                name.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_like_brew() {
        let packages: Vec<(String, String)> = [
            ("wget", "1.25.0"),
            ("cask:orbstack", "2.2.4,21120"),
            ("gettext", "0.26"),
        ]
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect();
        let listing = |formula, cask, versions| Listing {
            formula,
            cask,
            versions,
            one: false,
        };

        assert_eq!(
            lines(&packages, listing(true, false, false)),
            ["gettext", "wget"]
        );
        assert_eq!(lines(&packages, listing(false, true, false)), ["orbstack"]);
        assert_eq!(
            lines(&packages, listing(false, false, true)),
            ["gettext 0.26", "wget 1.25.0", "orbstack 2.2.4,21120"]
        );
        assert_eq!(
            lines(&packages, listing(true, false, true)),
            ["gettext 0.26", "wget 1.25.0"]
        );
        assert_eq!(
            lines(&packages, listing(false, false, false)),
            ["gettext", "wget", "orbstack"]
        );
    }
}
