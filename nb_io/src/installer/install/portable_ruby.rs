//! Homebrew's portable Ruby, which runs a formula's Ruby when it is built
//! from source, as `brew vendor-install ruby` gets it: the version in
//! `vendor/portable-ruby-version` and, for each platform, the bottle tag
//! and checksum in `vendor/portable-ruby-<arch>-<os>`.

use std::path::PathBuf;

use nb_core::Error;

use super::Installer;
use crate::network::download::DownloadRequest;

const VERSION: &str = "4.0.7_1";

/// This platform's bottle: its tag and checksum.
fn bottle() -> Option<(&'static str, &'static str)> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => Some((
            "arm64_big_sur",
            "25d6becc4464b6e65ac38a76ccd27eee8be5d0d805f6b3292d74e37deb8dd13b",
        )),
        ("x86_64", "macos") => Some((
            "big_sur",
            "da1d2637272470d15f1c9f9fb56a3c7889978b857912675e7e6fa57b165678de",
        )),
        ("aarch64", "linux") => Some((
            "arm64_linux",
            "7a43dce308d310788045ea32b5476ad618d0e92e72b540741b83a73717479dc4",
        )),
        ("x86_64", "linux") => Some((
            "x86_64_linux",
            "1f78c64df16fc4715452d059c3a8482ba12b683cc582a2e07e19bddaea88d4f7",
        )),
        _ => None,
    }
}

impl Installer {
    /// The portable Ruby's `ruby`, fetched and unpacked into the cache the
    /// first time a build needs it.
    pub(super) async fn portable_ruby(&self) -> Result<PathBuf, Error> {
        let root = self.cache_dir.join("portable-ruby");
        let dir = root.join(VERSION);
        let ruby = dir.join("bin/ruby");
        if ruby.is_file() {
            return Ok(ruby);
        }
        let (tag, sha256) = bottle().ok_or_else(|| Error::ExecutionError {
            message: "Homebrew has no portable Ruby for this platform".into(),
        })?;
        let blob = self
            .downloader
            .download_single(
                DownloadRequest::new(
                    format!("https://ghcr.io/v2/homebrew/core/portable-ruby/blobs/sha256:{sha256}"),
                    sha256,
                    format!("portable-ruby-{VERSION}.{tag}"),
                ),
                None,
            )
            .await?;
        // The bottle holds `portable-ruby/<version>/`; it goes in whole or
        // not at all.
        let staging = root.join(format!(".{VERSION}.partial"));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(Error::file(
            "failed to create the portable Ruby's directory",
        ))?;
        crate::extraction::extract_tarball(&blob, &staging)?;
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::rename(staging.join("portable-ruby").join(VERSION), &dir)
            .map_err(Error::file("failed to put the portable Ruby in place"))?;
        let _ = std::fs::remove_dir_all(&staging);
        Ok(ruby)
    }
}
