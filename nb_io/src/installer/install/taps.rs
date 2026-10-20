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

    /// Remove `tap` and forget which of its files were trusted.
    pub fn untap(&self, tap: &TapName) -> Result<TapContents, Error> {
        let source = self.tap_source()?;
        let contents = source.taps.uninstall(tap)?;
        source.trust.forget_tap(tap)?;
        Ok(contents)
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
