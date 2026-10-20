//! `nb tap`, `nb untap` and the taps `nb update` refreshes.

use nb_core::Error;

use super::Installer;
use crate::network::api::TapSource;
use crate::tap::{TapContents, TapName};

impl Installer {
    pub fn tap_source(&self) -> Result<&TapSource, Error> {
        self.api_client
            .taps()
            .ok_or_else(|| Error::InvalidArgument {
                message: "taps are not available".to_string(),
            })
    }

    /// Fetch `tap` (from `remote`, or its GitHub repository).
    pub async fn tap(&self, tap: &TapName, remote: Option<&str>) -> Result<TapContents, Error> {
        let source = self.tap_source()?;
        source
            .taps
            .install(tap, remote, self.api_client.http())
            .await
    }

    /// Remove `tap`. What was trusted stays trusted, as with `brew untap`.
    pub fn untap(&self, tap: &TapName) -> Result<TapContents, Error> {
        self.tap_source()?.taps.uninstall(tap)
    }

    /// The installed formula `name`, which may be given in full as
    /// `user/repo/name` for one from that tap.
    pub fn get_installed_by_full_name(
        &self,
        name: &str,
    ) -> Option<crate::storage::db::InstalledKeg> {
        if let Some(keg) = self.db.get_installed(name) {
            return Some(keg);
        }
        let (tap, short) = TapName::split_full_name(name)?;
        let tap = (!tap.is_core()).then(|| tap.name());
        self.db.get_installed(&short).filter(|keg| keg.tap == tap)
    }

    /// `Formula#full_name` of an installed formula: `user/repo/name` for one
    /// from a third-party tap.
    pub fn installed_full_name(&self, name: &str) -> String {
        if name.starts_with("cask:") {
            return name.to_string();
        }
        match self.db.get_installed(name).and_then(|keg| keg.tap) {
            Some(tap) => format!("{tap}/{name}"),
            None => name.to_string(),
        }
    }

    /// The installed formulae and casks that came from `tap`, by install
    /// name.
    pub fn installed_from_tap(&self, tap: &TapName) -> Result<Vec<String>, Error> {
        let name = tap.name();
        Ok(self
            .db
            .list_installed()?
            .into_iter()
            .filter(|keg| keg.tap.as_deref() == Some(name.as_str()))
            .map(|keg| keg.name)
            .collect())
    }

    /// Fetch every installed tap again whose default branch moved. Returns
    /// the taps that changed.
    pub async fn update_taps(&self) -> Result<Vec<TapName>, Error> {
        let source = self.tap_source()?;
        let mut changed = Vec::new();
        for tap in source.taps.installed() {
            if source.taps.update(&tap, self.api_client.http()).await? {
                changed.push(tap);
            }
        }
        Ok(changed)
    }
}
