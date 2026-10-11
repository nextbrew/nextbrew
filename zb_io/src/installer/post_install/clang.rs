//! `configure_clang_system`: tell Homebrew's clang where the macOS SDK is.
//!
//! llvm's keg ships a clang that knows nothing about the system it runs on.
//! Homebrew writes one config file per target triple under `etc/clang`,
//! each holding an `-isysroot` pointing at the Command Line Tools SDK, and
//! clang picks the one matching its default target. Nothing to do on Linux.

#[cfg(target_os = "macos")]
use std::path::Path;

use super::{PostInstallContext, StepError};

#[cfg(target_os = "macos")]
const CLT_SDKS: &str = "/Library/Developer/CommandLineTools/SDKs";

#[cfg(not(target_os = "macos"))]
pub(super) fn configure_clang_system(_ctx: &PostInstallContext) -> Result<(), StepError> {
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) fn configure_clang_system(ctx: &PostInstallContext) -> Result<(), StepError> {
    let macos = macos_version()?;
    let kernel = kernel_major()?;
    write_config_files(
        &ctx.prefix.join("etc/clang"),
        &macos,
        &kernel,
        super::template::arch(),
    )
}

/// The macOS version the way Homebrew names it: the major alone from Big
/// Sur on (`26`), `10.x` before.
#[cfg(target_os = "macos")]
fn macos_version() -> Result<String, StepError> {
    zb_core::macos_major_version()
        .map(|major| major.to_string())
        .ok_or_else(|| StepError::Failed("cannot read the macOS version".to_string()))
}

/// The Darwin kernel major, `25` for macOS 26.
#[cfg(target_os = "macos")]
fn kernel_major() -> Result<String, StepError> {
    let output = std::process::Command::new("/usr/bin/uname")
        .arg("-r")
        .output()
        .map_err(StepError::io("uname -r"))?;
    let release = String::from_utf8_lossy(&output.stdout);
    release
        .trim()
        .split('.')
        .next()
        .filter(|major| !major.is_empty())
        .map(str::to_string)
        .ok_or_else(|| StepError::Failed("cannot read the kernel version".to_string()))
}

#[cfg(target_os = "macos")]
fn write_config_files(
    config_dir: &Path,
    macos: &str,
    kernel: &str,
    arch: &str,
) -> Result<(), StepError> {
    let mut arches = vec!["arm64", "x86_64", "aarch64"];
    if !arches.contains(&arch) {
        arches.push(arch);
    }
    let systems = [("darwin", kernel), ("macosx", macos)];
    let files: Vec<_> = systems
        .iter()
        .flat_map(|(system, version)| {
            arches
                .iter()
                .map(move |arch| config_dir.join(format!("{arch}-apple-{system}{version}.cfg")))
        })
        .collect();
    if files.iter().all(|file| file.exists()) {
        return Ok(());
    }

    std::fs::create_dir_all(config_dir).map_err(StepError::io("creating etc/clang"))?;
    let content = format!("-isysroot {CLT_SDKS}/MacOSX{macos}.sdk\n");
    for file in &files {
        super::steps::write_atomically(file, content.as_bytes())?;
    }
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn writes_one_file_per_triple() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("etc/clang");
        write_config_files(&dir, "26", "25", "arm64").unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "aarch64-apple-darwin25.cfg",
                "aarch64-apple-macosx26.cfg",
                "arm64-apple-darwin25.cfg",
                "arm64-apple-macosx26.cfg",
                "x86_64-apple-darwin25.cfg",
                "x86_64-apple-macosx26.cfg",
            ]
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("arm64-apple-macosx26.cfg")).unwrap(),
            "-isysroot /Library/Developer/CommandLineTools/SDKs/MacOSX26.sdk\n"
        );
    }

    #[test]
    fn existing_files_are_left_alone() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("etc/clang");
        write_config_files(&dir, "26", "25", "arm64").unwrap();
        let file = dir.join("arm64-apple-macosx26.cfg");
        std::fs::write(&file, "custom\n").unwrap();
        write_config_files(&dir, "26", "25", "arm64").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "custom\n");
    }

    #[test]
    fn kernel_and_macos_versions_are_numbers() {
        assert!(kernel_major().unwrap().parse::<u32>().is_ok());
        assert!(macos_version().unwrap().parse::<u32>().is_ok());
    }
}
