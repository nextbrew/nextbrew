use std::path::Path;

use console::style;
use nb_io::LinkOutcome;
use nb_io::installer::install::diagnostic::prepend_path_in_profile;
use nb_io::installer::install::{CaskLink, CaskLinkEvent};

use crate::utils::normalize_formula_name;

pub struct LinkOptions {
    pub force: bool,
    pub overwrite: bool,
    pub dry_run: bool,
}

/// `nb link`, with `brew link`'s messages: the formulae named, then the
/// casks.
pub async fn link(
    installer: &mut nb_io::Installer,
    names: Vec<String>,
    options: LinkOptions,
    formula: bool,
    cask: bool,
) -> Result<(), nb_core::Error> {
    let LinkOptions {
        force,
        overwrite,
        dry_run,
    } = options;
    let (formulas, casks) = kegs_and_casks(installer, &names, formula, cask).await?;
    for name in formulas {
        let (keg_path, _) = installer.formula_keg(&name)?;
        let keg = keg_path.display();

        if dry_run {
            match installer.link_dry_run(&name, overwrite).await? {
                Some(plan) => {
                    println!(
                        "{}",
                        if overwrite {
                            "Would remove:"
                        } else {
                            "Would link:"
                        }
                    );
                    for path in &plan.paths {
                        println!("{path}");
                    }
                    if plan.keg_only {
                        keg_only_path_message(installer, &keg_path, &name);
                    }
                }
                None => already_linked(&keg, &name),
            }
            continue;
        }

        let outcome = match installer.link(&name, force, overwrite).await {
            Err(nb_core::Error::LinkConflict { conflicts }) if !conflicts.is_empty() => {
                return Err(nb_core::Error::ExecutionError {
                    message: conflict_message(installer.prefix(), &name, &conflicts[0]),
                });
            }
            outcome => outcome?,
        };
        match outcome {
            LinkOutcome::Linked { count, unlinked } => {
                for (other, removed) in unlinked {
                    println!(
                        "Unlinking {}... {removed} symlinks removed.",
                        other.display()
                    );
                }
                println!("Linking {keg}... {count} symlinks created.");
            }
            LinkOutcome::AlreadyLinked => already_linked(&keg, &name),
            LinkOutcome::KegOnly => {
                warn(format!(
                    "{name} is keg-only and must be linked with `--force`."
                ));
                keg_only_path_message(installer, &keg_path, &name);
            }
        }
    }
    for name in casks {
        if dry_run {
            println!(
                "{}",
                if overwrite {
                    "Would remove:"
                } else {
                    "Would link:"
                }
            );
        }
        installer.link_cask(&name, force, overwrite, dry_run, say)?;
    }
    Ok(())
}

/// `args.named.to_kegs_to_casks`: an installed formula, or else a cask
/// (installed or not, which linking then reports); `--formula` / `--cask`
/// take every name as that.
async fn kegs_and_casks(
    installer: &nb_io::Installer,
    names: &[String],
    formula: bool,
    cask: bool,
) -> Result<(Vec<String>, Vec<String>), nb_core::Error> {
    let mut formulas = Vec::new();
    let mut casks = Vec::new();
    for name in names {
        let installed_cask = installer
            .get_installed_by_full_name(&format!("cask:{name}"))
            .map(|keg| keg.name);
        if cask {
            casks.push(cask_name(installer, name, installed_cask).await?);
            continue;
        }
        let normalized = normalize_formula_name(name)?;
        let as_cask = !formula
            && installer.get_installed_by_full_name(&normalized).is_none()
            && (installed_cask.is_some()
                || (installer.formula(&normalized).await.is_err()
                    && installer.cask(name).await.is_ok()));
        if as_cask {
            casks.push(cask_name(installer, name, installed_cask).await?);
        } else {
            formulas.push(normalized);
        }
    }
    Ok((formulas, casks))
}

/// The install name of the cask `name`, or why there is no such cask.
async fn cask_name(
    installer: &nb_io::Installer,
    name: &str,
    installed: Option<String>,
) -> Result<String, nb_core::Error> {
    if let Some(installed) = installed {
        return Ok(installed);
    }
    let token = name.strip_prefix("cask:").unwrap_or(name);
    if installer.cask(token).await.is_err() {
        return Err(nb_core::Error::InvalidArgument {
            message: format!("Cask '{token}' is unavailable: No Cask with this name exists."),
        });
    }
    Ok(format!("cask:{token}"))
}

/// `Symlinked#link` / `#unlink`'s messages.
fn say(event: CaskLinkEvent) {
    let there = |link: &CaskLink| {
        let article = if link.kind.starts_with(['A', 'E', 'I', 'O', 'U']) {
            "an"
        } else {
            "a"
        };
        format!(
            "It seems there is already {article} {} at '{}'",
            link.kind,
            link.target.display()
        )
    };
    match event {
        CaskLinkEvent::Would(path) => println!("{}", path.display()),
        CaskLinkEvent::Overwriting(link) => warn(format!("{}; overwriting.", there(&link))),
        CaskLinkEvent::Linked(link) => ohai(&format!(
            "Linking {} '{}' to '{}'",
            link.kind,
            link.source_name(),
            link.target.display()
        )),
        CaskLinkEvent::AlreadyLinked(link) => ohai(&format!(
            "{} '{}' is already linked to '{}'",
            link.kind,
            link.source_name(),
            link.target.display()
        )),
        CaskLinkEvent::SkippedFormula(link, formula) => warn(format!(
            "{} from formula {formula}; skipping link.",
            there(&link)
        )),
        CaskLinkEvent::Unlinked(link) => ohai(&format!(
            "Unlinking {} '{}'",
            link.kind,
            link.target.display()
        )),
    }
}

fn ohai(message: &str) {
    println!("{} {}", style("==>").blue().bold(), style(message).bold());
}

fn already_linked(keg: &impl std::fmt::Display, name: &str) {
    warn(format!("Already linked: {keg}"));
    println!("To relink, run:\n  nb unlink {name} && nb link {name}");
}

/// `Keg::ConflictError#to_s`, for the first file in the way.
fn conflict_message(prefix: &Path, name: &str, conflict: &nb_core::ConflictedLink) -> String {
    let dst = &conflict.path;
    let relative = dst.strip_prefix(prefix).unwrap_or(dst);
    let suggestion = match &conflict.owned_by {
        Some(owner) => {
            format!("is a symlink belonging to {owner}. You can unlink it:\n  nb unlink {owner}\n")
        }
        None => format!(
            "already exists. You may want to remove it:\n  rm '{}'\n",
            dst.display()
        ),
    };
    format!(
        "Could not symlink {}\nTarget {}\n{suggestion}\n\
         To force the link and overwrite all conflicting files:\n  \
         nb link --overwrite {name}\n\n\
         To list all files that would be deleted:\n  \
         nb link --overwrite {name} --dry-run",
        relative.display(),
        dst.display()
    )
}

/// `puts_keg_only_path_message`.
fn keg_only_path_message(installer: &nb_io::Installer, keg: &Path, name: &str) {
    let (bin, sbin) = (keg.join("bin").is_dir(), keg.join("sbin").is_dir());
    if !bin && !sbin {
        return;
    }
    let opt = installer.prefix().join("opt").join(name);
    let shell = std::env::var("SHELL").ok();
    println!("\nIf you need to have this software first in your PATH instead consider running:");
    for (has, dir) in [(bin, "bin"), (sbin, "sbin")] {
        if has {
            println!(
                "  {}",
                prepend_path_in_profile(&opt.join(dir), shell.as_deref())
            );
        }
    }
}

/// `nb unlink`, with `brew unlink`'s messages: the formulae named, then
/// the casks.
pub async fn unlink(
    installer: &mut nb_io::Installer,
    names: Vec<String>,
    formula: bool,
    cask: bool,
) -> Result<(), nb_core::Error> {
    let (formulas, casks) = kegs_and_casks(installer, &names, formula, cask).await?;
    for name in formulas {
        let (keg, _) = installer.formula_keg(&name)?;
        let n = installer.unlink(&name)?;
        println!("Unlinking {}... {n} symlinks removed.", keg.display());
    }
    for name in casks {
        installer.unlink_cask(&name, false, say)?;
    }
    Ok(())
}

fn warn(message: String) {
    eprintln!("{} {message}", style("Warning:").yellow().bold());
}
