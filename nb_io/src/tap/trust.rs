//! Which third-party taps, formulae and casks may be loaded, as Homebrew's
//! `Homebrew::Trust` keeps it: a tap's files are only read once the tap,
//! or the formula or cask itself, is trusted. Naming a formula or cask in
//! full (`user/repo/name`) on the command line trusts it.
//!
//! The entries live in `$XDG_CONFIG_HOME/nextbrew/trust.json` or
//! `~/.nextbrew/trust.json`, in Homebrew's format.

use std::collections::BTreeMap;
use std::path::PathBuf;

use nb_core::Error;

use super::TapName;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustKind {
    Tap,
    Formula,
    Cask,
}

impl TrustKind {
    fn key(self) -> &'static str {
        match self {
            TrustKind::Tap => "trustedtaps",
            TrustKind::Formula => "trustedformulae",
            TrustKind::Cask => "trustedcasks",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TrustKind::Tap => "tap",
            TrustKind::Formula => "formula",
            TrustKind::Cask => "cask",
        }
    }
}

#[derive(Debug)]
pub struct TrustStore {
    path: PathBuf,
}

impl Default for TrustStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TrustStore {
    pub fn new() -> Self {
        let path = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(config) => PathBuf::from(config).join("nextbrew/trust.json"),
            None => std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".nextbrew/trust.json"),
        };
        Self { path }
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    fn read(&self) -> BTreeMap<String, Vec<String>> {
        std::fs::read(&self.path)
            .ok()
            .and_then(|json| serde_json::from_slice(&json).ok())
            .unwrap_or_default()
    }

    fn write(&self, store: &BTreeMap<String, Vec<String>>) -> Result<(), Error> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(Error::file("failed to create the trust store"))?;
        }
        let json = serde_json::to_vec_pretty(store).unwrap_or_default();
        std::fs::write(&self.path, json).map_err(Error::file("failed to write the trust store"))
    }

    pub fn entries(&self, kind: TrustKind) -> Vec<String> {
        self.read().remove(kind.key()).unwrap_or_default()
    }

    /// Record `name`; false if it was already there.
    pub fn trust(&self, kind: TrustKind, name: &str) -> Result<bool, Error> {
        let mut store = self.read();
        let entries = store.entry(kind.key().to_string()).or_default();
        let name = name.to_lowercase();
        if entries.contains(&name) {
            return Ok(false);
        }
        entries.push(name);
        entries.sort();
        self.write(&store)?;
        Ok(true)
    }

    /// Forget `name`; false if it was not there.
    pub fn untrust(&self, kind: TrustKind, name: &str) -> Result<bool, Error> {
        let mut store = self.read();
        let name = name.to_lowercase();
        let Some(entries) = store.get_mut(kind.key()) else {
            return Ok(false);
        };
        let before = entries.len();
        entries.retain(|entry| *entry != name);
        if entries.len() == before {
            return Ok(false);
        }
        if entries.is_empty() {
            store.remove(kind.key());
        }
        self.write(&store)?;
        Ok(true)
    }

    /// Forget everything about a tap once it is untapped.
    pub fn forget_tap(&self, tap: &TapName) -> Result<(), Error> {
        let mut store = self.read();
        let name = tap.name();
        let prefix = format!("{name}/");
        let mut changed = false;
        for entries in store.values_mut() {
            let before = entries.len();
            entries.retain(|entry| *entry != name && !entry.starts_with(&prefix));
            changed |= entries.len() != before;
        }
        store.retain(|_, entries| !entries.is_empty());
        if changed {
            self.write(&store)?;
        }
        Ok(())
    }

    /// Whether `user/repo/name` (or its tap) may be loaded. Official taps
    /// always may.
    pub fn trusted(&self, kind: TrustKind, tap: &TapName, name: &str) -> bool {
        if tap.is_official() {
            return true;
        }
        let store = self.read();
        let has = |kind: TrustKind, entry: &str| {
            store
                .get(kind.key())
                .is_some_and(|entries| entries.iter().any(|e| e == entry))
        };
        has(TrustKind::Tap, &tap.name())
            || has(kind, &format!("{}/{}", tap.name(), name.to_lowercase()))
    }

    /// `require_trusted_formula!` / `require_trusted_cask!`.
    pub fn require(&self, kind: TrustKind, tap: &TapName, name: &str) -> Result<(), Error> {
        if self.trusted(kind, tap, name) {
            return Ok(());
        }
        let full = format!("{tap}/{name}");
        Err(Error::InvalidArgument {
            message: format!(
                "Refusing to load {} {full} from untrusted tap {tap}.\nRun `nb trust --{} {full}` or `nb trust {tap}` to trust it.",
                kind.label(),
                kind.label()
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusts_taps_and_items() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TrustStore::at(tmp.path().join("trust.json"));
        let tap = TapName::parse("me/tools").unwrap();
        assert!(!store.trusted(TrustKind::Formula, &tap, "foo"));
        assert!(store.require(TrustKind::Formula, &tap, "foo").is_err());
        assert!(store.trust(TrustKind::Formula, "Me/Tools/Foo").unwrap());
        assert!(!store.trust(TrustKind::Formula, "me/tools/foo").unwrap());
        assert!(store.trusted(TrustKind::Formula, &tap, "foo"));
        assert!(!store.trusted(TrustKind::Cask, &tap, "foo"));
        store.trust(TrustKind::Tap, "me/tools").unwrap();
        assert!(store.trusted(TrustKind::Cask, &tap, "anything"));
        store.forget_tap(&tap).unwrap();
        assert!(!store.trusted(TrustKind::Formula, &tap, "foo"));
        assert!(store.trusted(
            TrustKind::Formula,
            &TapName::parse("homebrew/core").unwrap(),
            "x"
        ));
    }
}
