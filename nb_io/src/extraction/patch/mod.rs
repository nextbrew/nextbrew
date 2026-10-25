#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(any(target_os = "macos", test))]
pub(crate) mod macho;
mod phar;
pub(crate) mod text;

#[cfg(target_os = "linux")]
pub use linux::patch_placeholders;

#[cfg(target_os = "macos")]
pub use macos::relocate_keg;

/// The files of a keg that need relocating, as the bottle's tab lists them;
/// Homebrew pours with these lists too. Paths are relative to the keg.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelocationFiles {
    /// `changed_files`: text with placeholders.
    pub text: Vec<std::path::PathBuf>,
    /// `linkage_files` and `binary_relocation_files`: binaries with
    /// placeholders in their load commands or the build prefix in strings.
    pub binary: Vec<std::path::PathBuf>,
}

impl RelocationFiles {
    /// Read the lists from a tab; `None` for a tab from a Homebrew too old
    /// to write all three, or with a path leaving the keg.
    pub fn from_tab(tab: &serde_json::Value) -> Option<Self> {
        let list = |key: &str| -> Option<Vec<std::path::PathBuf>> {
            tab[key]
                .as_array()?
                .iter()
                .map(|path| {
                    let path = std::path::Path::new(path.as_str()?);
                    path.components()
                        .all(|c| matches!(c, std::path::Component::Normal(_)))
                        .then(|| path.to_path_buf())
                })
                .collect()
        };
        let text = list("changed_files")?;
        let mut binary = list("linkage_files")?;
        for path in list("binary_relocation_files")? {
            if !binary.contains(&path) {
                binary.push(path);
            }
        }
        Some(Self { text, binary })
    }
}

/// The regular files among `paths` in the keg.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn in_keg(keg_path: &std::path::Path, paths: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    paths
        .iter()
        .map(|path| keg_path.join(path))
        .filter(|path| std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relocation_files_come_from_all_three_tab_lists() {
        let tab = serde_json::json!({
            "changed_files": ["bin/gem"],
            "linkage_files": ["bin/ruby", "lib/libruby.dylib"],
            "binary_relocation_files": ["lib/libruby.dylib"],
        });
        assert_eq!(
            RelocationFiles::from_tab(&tab),
            Some(RelocationFiles {
                text: vec!["bin/gem".into()],
                binary: vec!["bin/ruby".into(), "lib/libruby.dylib".into()],
            })
        );
        let old = serde_json::json!({ "changed_files": ["bin/gem"] });
        assert_eq!(RelocationFiles::from_tab(&old), None);
        let escaping = serde_json::json!({
            "changed_files": ["../x"],
            "linkage_files": [],
            "binary_relocation_files": [],
        });
        assert_eq!(RelocationFiles::from_tab(&escaping), None);
    }
}
