use std::path::Path;

use console::style;
use nb_io::LinkOutcome;
use nb_io::installer::install::diagnostic::prepend_path_in_profile;

use crate::utils::normalize_formula_name;

/// `nb link`, with `brew link`'s messages.
pub async fn link(
    installer: &mut nb_io::Installer,
    formulas: Vec<String>,
    force: bool,
    overwrite: bool,
    dry_run: bool,
) -> Result<(), nb_core::Error> {
    for formula in &formulas {
        let name = normalize_formula_name(formula)?;
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
    Ok(())
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
