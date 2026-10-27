use std::path::Path;

use crate::init::{InitError, nextbrew_dirs, shell_block};

/// `nb shellenv`: the shell code `nb init` writes, for `eval "$(nb shellenv)"`.
pub fn shellenv(shell: Option<String>, root: &Path, prefix: &Path) -> Result<(), nb_core::Error> {
    let shell = shell
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_default();
    let (dir, bin) = nextbrew_dirs()
        .map_err(|InitError::Message(message)| nb_core::Error::InvalidArgument { message })?;
    let block = shell_block(
        shell.ends_with("fish"),
        &dir,
        &bin,
        &root.display().to_string(),
        &prefix.display().to_string(),
    );
    println!("{}", block.trim());
    Ok(())
}

/// `nb config`: where nextbrew keeps things, and what it runs on.
pub fn config(installer: &nb_io::Installer, root: &Path) -> Result<(), nb_core::Error> {
    let prefix = installer.prefix();
    let installed = installer.list_installed()?.len();
    let taps = nb_io::tap::Taps::new(prefix).installed().len();
    println!("nextbrew: {}", env!("CARGO_PKG_VERSION"));
    println!("NEXTBREW_ROOT: {}", root.display());
    println!("NEXTBREW_PREFIX: {}", prefix.display());
    println!("Cellar: {}", prefix.join("Cellar").display());
    println!("Cache: {}", installer.cache_dir().display());
    println!("OS: {} {}", std::env::consts::OS, std::env::consts::ARCH);
    println!("Taps: {taps}");
    println!("Installed: {installed}");
    Ok(())
}

/// `nb --prefix`, `--cellar` and `--cache`, with or without formulae.
pub async fn path(
    installer: &nb_io::Installer,
    kind: &str,
    names: Vec<String>,
    installed: bool,
) -> Result<(), nb_core::Error> {
    let prefix = installer.prefix();
    if names.is_empty() {
        let path = match kind {
            "prefix" => prefix.to_path_buf(),
            "cellar" => prefix.join("Cellar"),
            _ => installer.cache_dir().to_path_buf(),
        };
        println!("{}", path.display());
        return Ok(());
    }
    for name in names {
        let formula = installer.formula(&name).await?;
        if installed && !installer.is_installed(&formula.name) {
            continue;
        }
        let path = match kind {
            "prefix" => prefix.join("opt").join(&formula.name),
            "cellar" => prefix.join("Cellar").join(&formula.name),
            _ => {
                let bottle = nb_core::formula::bottle::select_bottle(&formula)?;
                installer
                    .cache_dir()
                    .join("blobs")
                    .join(format!("{}.tar.gz", bottle.sha256))
            }
        };
        println!("{}", path.display());
    }
    Ok(())
}

/// `nb leaves`: installed formulae nothing installed depends on.
pub async fn leaves(
    installer: &nb_io::Installer,
    on_request: bool,
    as_dependency: bool,
) -> Result<(), nb_core::Error> {
    for name in installer.leaves().await? {
        let requested = installer.get_installed(&name).is_some_and(|k| k.on_request);
        if (on_request && !requested) || (as_dependency && requested) {
            continue;
        }
        println!("{name}");
    }
    Ok(())
}
