use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use nb_core::{Error, InstallMethod};

use super::{Installer, acquire_install_lock};
use crate::installer::cask::{for_current_platform, resolve_cask};
use crate::network::download::DownloadRequest;

/// What [`Installer::cleanup`] removed, or would remove.
#[derive(Debug, Default)]
pub struct Cleanup {
    /// Cached downloads, with their sizes.
    pub files: Vec<(PathBuf, u64)>,
    /// Store entries no keg uses.
    pub store_entries: Vec<String>,
}

impl Installer {
    /// `brew fetch`: the bottles of `names` (and with `deps` of their
    /// dependencies), or the casks' downloads, into the download cache.
    /// A formula without a bottle for this system has no path.
    pub async fn fetch(
        &self,
        names: &[String],
        deps: bool,
    ) -> Result<Vec<(String, Option<PathBuf>)>, Error> {
        let mut fetched = Vec::new();
        let mut formulae = Vec::new();
        for name in names {
            if let Some(token) = name.strip_prefix("cask:") {
                let json = for_current_platform(&self.api_client.get_cask(token).await?);
                let cask = resolve_cask(token, &json)?;
                let request = super::bottle::cask_download(&cask, &json);
                let path = self.downloader.download_single(request, None).await?;
                fetched.push((name.clone(), Some(path)));
            } else {
                formulae.push(name.clone());
            }
        }
        if formulae.is_empty() {
            return Ok(fetched);
        }
        // Aliases and old names, as the plan names them.
        let mut asked = HashSet::new();
        for name in &formulae {
            asked.insert(self.api_client.get_formula(name).await?.name);
        }
        let plan = self.plan_with_options(&formulae, false).await?;
        let mut requests = Vec::new();
        for item in &plan.items {
            if !deps && !asked.contains(&item.formula.name) {
                continue;
            }
            match &item.method {
                InstallMethod::Bottle(bottle) => {
                    requests.push(DownloadRequest::new(
                        bottle.url.clone(),
                        bottle.sha256.clone(),
                        item.formula.name.clone(),
                    ));
                }
                _ => fetched.push((item.formula.name.clone(), None)),
            }
        }
        let names: Vec<String> = requests.iter().map(|r| r.name.clone()).collect();
        let paths = self.downloader.download_all(requests).await?;
        fetched.extend(names.into_iter().zip(paths.into_iter().map(Some)));
        Ok(fetched)
    }

    /// `brew cleanup`: cached downloads older than `older_than` (all of
    /// them with `None`) and the store entries no keg uses.
    pub fn cleanup(
        &mut self,
        older_than: Option<Duration>,
        dry_run: bool,
    ) -> Result<Cleanup, Error> {
        let _lock = acquire_install_lock(&self.locks_dir)?;
        let now = SystemTime::now();
        let mut cleanup = Cleanup::default();
        // Under the lock nothing is downloading, so tmp/ is leftovers.
        for dir in ["blobs", "tmp"] {
            let Ok(entries) = fs::read_dir(self.cache_dir.join(dir)) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                let old = match older_than {
                    None => true,
                    Some(age) => {
                        dir == "tmp"
                            || meta
                                .modified()
                                .ok()
                                .and_then(|m| now.duration_since(m).ok())
                                .is_some_and(|elapsed| elapsed > age)
                    }
                };
                if meta.is_file() && old {
                    cleanup.files.push((entry.path(), meta.len()));
                }
            }
        }
        if dry_run {
            cleanup.store_entries = self.db.get_unreferenced_store_keys()?;
            return Ok(cleanup);
        }
        for (path, _) in &cleanup.files {
            fs::remove_file(path).map_err(Error::store("failed to remove a cached download"))?;
        }
        cleanup.store_entries = self.gc()?;
        Ok(cleanup)
    }
}

#[cfg(test)]
mod tests {
    use crate::cellar::Cellar;
    use crate::network::api::ApiClient;
    use crate::storage::{blob::BlobCache, db::Database, store::Store};
    use crate::{Installer, Linker};
    use std::time::Duration;
    use tempfile::TempDir;

    fn installer(tmp: &TempDir) -> Installer {
        let root = tmp.path().join("root");
        let prefix = tmp.path().join("prefix");
        std::fs::create_dir_all(root.join("db")).unwrap();
        std::fs::create_dir_all(root.join("locks")).unwrap();
        Installer::new(
            ApiClient::new(),
            BlobCache::new(&root.join("cache")).unwrap(),
            Store::new(&root).unwrap(),
            Cellar::new_at(prefix.join("Cellar")).unwrap(),
            Linker::new(&prefix).unwrap(),
            Database::open(&root.join("db/nb.sqlite3")).unwrap(),
            prefix,
            root.join("locks"),
        )
    }

    #[test]
    fn cleanup_removes_old_downloads_only() {
        let tmp = TempDir::new().unwrap();
        let mut installer = installer(&tmp);
        let blobs = tmp.path().join("root/cache/blobs");
        std::fs::write(blobs.join("new.tar.gz"), b"new").unwrap();

        let kept = installer
            .cleanup(Some(Duration::from_secs(3600)), false)
            .unwrap();
        assert!(kept.files.is_empty());
        assert!(blobs.join("new.tar.gz").exists());

        let dry = installer.cleanup(None, true).unwrap();
        assert_eq!(dry.files.len(), 1);
        assert!(blobs.join("new.tar.gz").exists());

        let removed = installer.cleanup(None, false).unwrap();
        assert_eq!(removed.files, vec![(blobs.join("new.tar.gz"), 3)]);
        assert!(!blobs.join("new.tar.gz").exists());
    }
}
