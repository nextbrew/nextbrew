//! Homebrew's `Cask::Config`: the directories cask artifacts go into, and
//! the switches that change how a cask is installed. Set explicitly
//! (`--appdir=…`, a Brewfile's `args:`), by `$HOMEBREW_CASK_OPTS`, or left
//! at Homebrew's defaults; kept in the keg so an upgrade uses them again.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use nb_core::Error;
use serde::{Deserialize, Serialize};

/// `Cask::Config::DEFAULT_DIRS`.
pub const DEFAULT_DIRS: &[(&str, &str)] = &[
    ("appdir", "/Applications"),
    ("appimagedir", "~/Applications"),
    ("keyboard_layoutdir", "/Library/Keyboard Layouts"),
    ("colorpickerdir", "~/Library/ColorPickers"),
    ("prefpanedir", "~/Library/PreferencePanes"),
    ("qlplugindir", "~/Library/QuickLook"),
    ("mdimporterdir", "~/Library/Spotlight"),
    ("dictionarydir", "~/Library/Dictionaries"),
    ("fontdir", "~/Library/Fonts"),
    ("servicedir", "~/Library/Services"),
    ("input_methoddir", "~/Library/Input Methods"),
    ("internet_plugindir", "~/Library/Internet Plug-Ins"),
    (
        "audio_unit_plugindir",
        "~/Library/Audio/Plug-Ins/Components",
    ),
    ("vst_plugindir", "~/Library/Audio/Plug-Ins/VST"),
    ("vst3_plugindir", "~/Library/Audio/Plug-Ins/VST3"),
    ("screen_saverdir", "~/Library/Screen Savers"),
];

/// The directory each moved artifact kind goes into by default.
const KIND_DIRS: &[(&str, &str)] = &[
    ("app_image", "appimagedir"),
    ("colorpicker", "colorpickerdir"),
    ("prefpane", "prefpanedir"),
    ("qlplugin", "qlplugindir"),
    ("mdimporter", "mdimporterdir"),
    ("dictionary", "dictionarydir"),
    ("font", "fontdir"),
    ("service", "servicedir"),
    ("input_method", "input_methoddir"),
    ("internet_plugin", "internet_plugindir"),
    ("keyboard_layout", "keyboard_layoutdir"),
    ("audio_unit_plugin", "audio_unit_plugindir"),
    ("vst_plugin", "vst_plugindir"),
    ("vst3_plugin", "vst3_plugindir"),
    ("screen_saver", "screen_saverdir"),
];

const CONFIG_JSON: &str = "cask-config.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaskConfig {
    /// Directories that are not at their default, by `DEFAULT_DIRS` key.
    #[serde(default)]
    pub dirs: BTreeMap<String, PathBuf>,
    /// `--require-sha`: refuse a cask without a checksum.
    #[serde(default)]
    pub require_sha: bool,
    /// `--no-binaries`: leave out the cask's `binary` artifacts.
    #[serde(default)]
    pub no_binaries: bool,
}

impl CaskConfig {
    /// Set `key` (`appdir`, `fontdir`, … with `-` or `_`) to `value`,
    /// expanded as Ruby's `Pathname#expand_path` does.
    pub fn set_dir(&mut self, key: &str, value: &str) -> Result<(), Error> {
        let key = key.trim_start_matches("--").replace('-', "_");
        if !DEFAULT_DIRS.iter().any(|(name, _)| *name == key) {
            return Err(Error::InvalidArgument {
                message: format!("unknown cask directory option `{key}`"),
            });
        }
        self.dirs.insert(key, expand(value));
        Ok(())
    }

    /// Read one switch or `--key=value` option as `brew install --cask`
    /// takes it. `--language` is refused: nextbrew installs the cask's
    /// default language only. An unknown switch is an error when `strict`
    /// (a Brewfile's `args:`), and ignored otherwise (`$HOMEBREW_CASK_OPTS`
    /// may hold `brew install`'s own switches).
    pub fn apply_option(&mut self, option: &str, strict: bool) -> Result<(), Error> {
        match option.split_once('=') {
            Some((key, _)) if key.trim_start_matches("--") == "language" => {
                Err(Error::InvalidArgument {
                    message: "`--language` is not supported by nextbrew".to_string(),
                })
            }
            Some((key, value)) => self.set_dir(key, value),
            None => {
                match option.trim_start_matches("--").replace('_', "-").as_str() {
                    "require-sha" => self.require_sha = true,
                    "no-binaries" => self.no_binaries = true,
                    "binaries" => self.no_binaries = false,
                    other if strict => {
                        return Err(Error::InvalidArgument {
                            message: format!("invalid option: --{other}"),
                        });
                    }
                    _ => {}
                }
                Ok(())
            }
        }
    }

    /// The options in `$HOMEBREW_CASK_OPTS`.
    pub fn from_env() -> Result<Self, Error> {
        let mut config = Self::default();
        for option in std::env::var("HOMEBREW_CASK_OPTS")
            .unwrap_or_default()
            .split_whitespace()
        {
            config.apply_option(option, false)?;
        }
        Ok(config)
    }

    /// This config with `over`'s settings on top, as explicit options win
    /// over the environment and the environment over what was saved.
    pub fn overlay(&self, over: &CaskConfig) -> CaskConfig {
        let mut merged = self.clone();
        merged.dirs.extend(over.dirs.clone());
        merged.require_sha |= over.require_sha;
        merged.no_binaries |= over.no_binaries;
        merged
    }

    /// The Applications folder, if set.
    pub fn appdir(&self) -> Option<&Path> {
        self.dirs.get("appdir").map(PathBuf::as_path)
    }

    /// Move a placed artifact's target out of its kind's default directory
    /// into the configured one. The API resolves targets with the defaults
    /// (`/$HOME/Library/Fonts/X.otf`); one the cask names itself elsewhere
    /// is left as it is.
    pub fn retarget(&self, kind: &str, target: &str) -> String {
        let Some((_, key)) = KIND_DIRS.iter().find(|(k, _)| *k == kind) else {
            return target.to_string();
        };
        let Some(dir) = self.dirs.get(*key) else {
            return target.to_string();
        };
        let default = DEFAULT_DIRS
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, path)| *path)
            .unwrap_or_default();
        let api_default = match default.strip_prefix("~/") {
            Some(rest) => format!("/$HOME/{rest}"),
            None => default.to_string(),
        };
        match target.strip_prefix(&format!("{api_default}/")) {
            Some(rest) => dir.join(rest).to_string_lossy().into_owned(),
            None => target.to_string(),
        }
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The config a keg was installed with.
    pub fn load(keg: &Path) -> Self {
        std::fs::read(keg.join(CONFIG_JSON))
            .ok()
            .and_then(|json| serde_json::from_slice(&json).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, keg: &Path) -> Result<(), Error> {
        if self.is_default() {
            return Ok(());
        }
        let json = serde_json::to_string(self).map_err(|e| {
            Error::store("failed to encode the cask config")(std::io::Error::other(e))
        })?;
        std::fs::write(keg.join(CONFIG_JSON), json)
            .map_err(Error::store("failed to save the cask config"))
    }
}

/// `Pathname#expand_path`: `~` is the home folder, a relative path is taken
/// from the working directory, `.` and `..` are resolved.
fn expand(path: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let path = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(path),
    };
    let path = if path.is_relative() {
        std::env::current_dir().unwrap_or_default().join(path)
    } else {
        path
    };
    let mut out = PathBuf::from("/");
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retargets_default_directories_only() {
        let mut config = CaskConfig::default();
        config.set_dir("--fontdir", "/tmp/fonts").unwrap();
        assert_eq!(
            config.retarget("font", "/$HOME/Library/Fonts/X.otf"),
            "/tmp/fonts/X.otf"
        );
        // A target the cask chose itself, or another kind, stays.
        assert_eq!(config.retarget("font", "/opt/x/X.otf"), "/opt/x/X.otf");
        assert_eq!(
            config.retarget("qlplugin", "/$HOME/Library/QuickLook/Q.qlgenerator"),
            "/$HOME/Library/QuickLook/Q.qlgenerator"
        );
    }

    #[test]
    fn reads_options_like_brew() {
        let mut config = CaskConfig::default();
        for option in ["--appdir=/tmp/Apps", "--require-sha", "--force"] {
            config.apply_option(option, false).unwrap();
        }
        assert_eq!(config.appdir(), Some(Path::new("/tmp/Apps")));
        assert!(config.require_sha && !config.no_binaries);
        assert!(config.apply_option("--language=ja", false).is_err());
        assert!(config.apply_option("--nonsense-dir=/x", false).is_err());
        // Homebrew dropped --no-quarantine; a Brewfile asking for it fails.
        assert!(config.apply_option("--no_quarantine", true).is_err());

        let explicit = CaskConfig {
            dirs: [("appdir".to_string(), PathBuf::from("/other"))].into(),
            ..Default::default()
        };
        assert_eq!(
            config.overlay(&explicit).appdir(),
            Some(Path::new("/other"))
        );
    }

    #[test]
    fn round_trips_through_the_keg() {
        let keg = tempfile::tempdir().unwrap();
        assert!(CaskConfig::load(keg.path()).is_default());
        let mut config = CaskConfig::default();
        config.set_dir("appdir", "/tmp/Apps").unwrap();
        config.no_binaries = true;
        config.save(keg.path()).unwrap();
        assert_eq!(CaskConfig::load(keg.path()), config);
    }
}
