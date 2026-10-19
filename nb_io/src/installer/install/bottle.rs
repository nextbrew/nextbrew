use std::fs;
use std::path::{Path, PathBuf};

use nb_core::{Error, InstallMethod, formula_token};
use tracing::warn;

use super::cask_deps::check_requirements;
use super::quarantine::{propagate, quarantine_download};
use crate::cellar::link::Linker;
use crate::cellar::materialize::Cellar;
use crate::installer::cask::{check_deprecate_disable, for_current_platform, resolve_cask};
use crate::network::download::{
    BodyConsumer, Commit, DownloadProgressCallback, DownloadRequest, ParallelDownloader,
};
use crate::progress::InstallProgress;
use crate::storage::store::Store;

use super::cask_app::{app_dir, is_dmg, remove_cask_apps, stage_cask};
use super::{Installer, MAX_CORRUPTION_RETRIES, PlannedInstall};

impl Installer {
    /// Clone a prepared store entry into the Cellar, record it, and link it.
    /// Runs on the install task, one bottle at a time, once
    /// [`prepare_bottle`] has produced the entry.
    pub(super) fn finish_bottle_item(
        &mut self,
        item: &PlannedInstall,
        store_entry: &Path,
        link: bool,
        report: &impl Fn(InstallProgress),
    ) -> Result<(), Error> {
        let InstallMethod::Bottle(ref bottle) = item.method else {
            unreachable!()
        };
        let install_name = &item.install_name;
        let formula_name = &item.formula.name;
        let version = item.formula.effective_version();
        let store_key = &bottle.sha256;

        let keg_path = self
            .cellar
            .materialize(formula_name, &version, store_entry)?;

        // Before recording the install, so a failure doesn't leave the
        // package marked installed without its config files.
        crate::cellar::install_etc_var(&keg_path, &self.prefix)
            .map_err(Error::file("failed to install etc/var files"))
            .inspect_err(|_| {
                Self::cleanup_materialized(&self.cellar, formula_name, &version);
            })?;

        report(InstallProgress::UnpackCompleted {
            name: formula_name.clone(),
        });

        let tx = self.db.transaction().inspect_err(|_| {
            Self::cleanup_materialized(&self.cellar, formula_name, &version);
        })?;

        tx.record_install(install_name, &version, store_key)
            .inspect_err(|_| {
                Self::cleanup_materialized(&self.cellar, formula_name, &version);
            })?;

        tx.commit().inspect_err(|_| {
            Self::cleanup_materialized(&self.cellar, formula_name, &version);
        })?;

        // Aliases only: Homebrew refreshes an old name's opt link that a
        // rename left behind, but never makes a new one.
        if let Err(e) = self.linker.link_opt(&keg_path, &item.formula.aliases) {
            warn!(formula = %install_name, error = %e, "failed to create opt link");
        }

        let keg_only = item.formula.is_keg_only() && !item.link_keg_only;
        if link && !keg_only && self.make_room_to_link(item, report)? {
            report(InstallProgress::LinkStarted {
                name: formula_name.clone(),
            });
            match self.linker.link_keg(&keg_path) {
                Ok(linked_files) => {
                    report(InstallProgress::LinkCompleted {
                        name: formula_name.clone(),
                    });
                    self.record_linked_files(install_name, &version, &linked_files);
                }
                Err(e) => {
                    let _ = self.linker.unlink_keg(&keg_path);
                    report(InstallProgress::InstallCompleted {
                        name: formula_name.clone(),
                    });
                    return Err(e);
                }
            }
        } else if link {
            let reason = match &item.formula.keg_only {
                // Kept unlinked for a related formula linked after it.
                _ if !item.formula.is_keg_only() || item.link_keg_only => {
                    "another version is linked".to_string()
                }
                nb_core::KegOnly::Reason(s) => format!("keg-only ({s})"),
                _ if formula_name.contains('@') => "keg-only (versioned formula)".to_string(),
                _ => "keg-only".to_string(),
            };
            report(InstallProgress::LinkSkipped {
                name: formula_name.clone(),
                reason,
            });
        }

        report(InstallProgress::InstallCompleted {
            name: formula_name.clone(),
        });

        Ok(())
    }

    pub(super) fn record_linked_files(
        &mut self,
        name: &str,
        version: &str,
        linked_files: &[crate::cellar::link::LinkedFile],
    ) {
        if let Ok(tx) = self.db.transaction() {
            let mut ok = true;
            for linked in linked_files {
                if tx
                    .record_linked_file(
                        name,
                        version,
                        &linked.link_path.to_string_lossy(),
                        &linked.target_path.to_string_lossy(),
                    )
                    .is_err()
                {
                    ok = false;
                    break;
                }
            }
            if ok {
                let _ = tx.commit();
            }
        }
    }

    pub(super) fn cleanup_failed_install(
        linker: &Linker,
        cellar: &Cellar,
        name: &str,
        version: &str,
        keg_path: &Path,
        unlink: bool,
    ) {
        if unlink && let Err(e) = linker.unlink_keg(keg_path) {
            warn!(
                formula = %name,
                version = %version,
                error = %e,
                "failed to clean up links after install error"
            );
        }

        if let Err(e) = remove_cask_apps(keg_path) {
            warn!(
                formula = %name,
                version = %version,
                error = %e,
                "failed to remove apps after install error"
            );
        }

        if let Err(e) = cellar.remove_keg(name, version) {
            warn!(
                formula = %name,
                version = %version,
                error = %e,
                "failed to remove keg after install error"
            );
        }
    }

    /// Install one cask and, first, what it depends on. The caller holds
    /// the install lock.
    pub(super) async fn install_single_cask(
        &mut self,
        token: &str,
        link: bool,
    ) -> Result<(), Error> {
        self.install_cask_in_chain(token, link, &mut Vec::new())
            .await
    }

    /// Check that nextbrew can install `token` and download it, without
    /// installing: what a swap from Homebrew needs before it removes the
    /// cask there. A download that is not verified (`sha256 :no_check`) is
    /// not kept, so it is not fetched ahead.
    pub async fn prepare_cask(&self, token: &str) -> Result<(), Error> {
        if let Some((tap, _)) = token.rsplit_once('/') {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{token}' is from the '{tap}' tap; nextbrew installs homebrew/cask casks only"
                ),
            });
        }
        let json = for_current_platform(&self.api_client.get_cask(token).await?);
        check_deprecate_disable(&json)?;
        check_requirements(&json)?;
        self.check_conflicts(&json)?;
        let cask = resolve_cask(token, &json)?;
        if !cask.no_check {
            self.downloader
                .download_single(cask_download(&cask, &json), None)
                .await?;
        }
        Ok(())
    }

    /// [`Self::install_single_cask`] for a cask that `chain` depends on.
    pub(super) async fn install_cask_in_chain(
        &mut self,
        token: &str,
        link: bool,
        chain: &mut Vec<String>,
    ) -> Result<(), Error> {
        // Only homebrew/cask comes from the API; other taps are Phase 3.
        let token = token.strip_prefix("homebrew/cask/").unwrap_or(token);
        if let Some((tap, _)) = token.rsplit_once('/') {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{token}' is from the '{tap}' tap; nextbrew installs homebrew/cask casks only"
                ),
            });
        }
        let cask_json = for_current_platform(&self.api_client.get_cask(token).await?);
        check_deprecate_disable(&cask_json)?;
        check_requirements(&cask_json)?;
        self.check_conflicts(&cask_json)?;
        let mut cask = resolve_cask(token, &cask_json)?;
        let config = self.cask_config.clone();
        if cask.no_check && config.require_sha {
            return Err(Error::InvalidArgument {
                message: format!(
                    "Cask '{token}' does not have a sha256 checksum defined and was not installed.\nThis means you have the `--require-sha` option set, perhaps in your `$HOMEBREW_CASK_OPTS`."
                ),
            });
        }
        for placed in &mut cask.placed {
            placed.target = config.retarget(&placed.kind, &placed.target);
        }
        if config.no_binaries {
            cask.binaries.clear();
        }

        let blob_path = self
            .downloader
            .download_single(cask_download(&cask, &cask_json), None)
            .await?;
        if !cask.no_check {
            return self
                .install_downloaded_cask(cask, &cask_json, &blob_path, link, chain)
                .await;
        }
        // An unverified download is kept by what it is, and fetched afresh
        // next time, whether this install works or not.
        let key = std::mem::take(&mut cask.sha256);
        let result = match crate::checksum::sha256_file(&blob_path) {
            Ok(actual) => {
                cask.sha256 = actual;
                self.install_downloaded_cask(cask, &cask_json, &blob_path, link, chain)
                    .await
            }
            Err(e) => Err(e),
        };
        self.downloader.remove_blob(&key);
        result
    }

    async fn install_downloaded_cask(
        &mut self,
        cask: crate::installer::cask::ResolvedCask,
        cask_json: &serde_json::Value,
        blob_path: &Path,
        link: bool,
        chain: &mut Vec<String>,
    ) -> Result<(), Error> {
        let homepage = cask_json["homepage"].as_str().unwrap_or_default();
        let config = self.cask_config.clone();
        quarantine_download(blob_path, &cask.url, homepage)?;
        self.install_cask_dependencies(cask_json, link, chain)
            .await?;

        let keg_path = self.cellar.keg_path(&cask.install_name, &cask.version);
        let mut cleanup = FailedInstallGuard::new(
            &self.linker,
            &self.cellar,
            &cask.install_name,
            &cask.version,
            &keg_path,
            link,
        );

        if !cask.apps.is_empty()
            || !cask.hooks.is_empty()
            || !cask.installers.is_empty()
            || !cask.placed.is_empty()
            || !cask.wrappers.is_empty()
            || !cask.completions.is_empty()
            || !cask.generated_scripts.is_empty()
            || is_dmg(blob_path)?
        {
            let appdir = match config.appdir() {
                Some(dir) => dir.to_path_buf(),
                None => app_dir()?,
            };
            let ctx = self.cask_context(&cask, &keg_path, appdir);
            stage_cask(blob_path, &cask, cask_json, &ctx)?;
        } else if crate::extraction::is_archive(blob_path)? {
            // Cask archives are staged file by file; nothing to prepare.
            // v2: entries carry the download's quarantine.
            let extracted =
                self.store
                    .ensure_entry(&cask.sha256, blob_path, "cask v2\n", |dir| {
                        propagate(blob_path, dir)
                    })?;
            stage_cask_binaries(&extracted, None, &keg_path, &cask)?;
        } else {
            stage_raw_cask_binary(blob_path, &keg_path, &cask)?;
            propagate(blob_path, &keg_path)?;
        }
        if !keg_path.join("cask.json").exists() {
            super::cask_app::save_cask_json(&keg_path, cask_json)?;
        }
        config.save(&keg_path)?;

        let linked_files = if link {
            self.linker.link_keg(&keg_path)?
        } else {
            Vec::new()
        };

        let tx = self.db.transaction()?;
        tx.record_install(&cask.install_name, &cask.version, &cask.sha256)?;
        for linked in &linked_files {
            tx.record_linked_file(
                &cask.install_name,
                &cask.version,
                &linked.link_path.to_string_lossy(),
                &linked.target_path.to_string_lossy(),
            )?;
        }
        tx.commit()?;

        cleanup.disarm();
        Ok(())
    }
}

/// The download of a cask. One with `sha256 :no_check` is not verified,
/// as Homebrew does for its own casks; that is said only for a cask that
/// does not update itself, since one that does is expected to change.
pub(super) fn cask_download(
    cask: &crate::installer::cask::ResolvedCask,
    json: &serde_json::Value,
) -> DownloadRequest {
    let request = DownloadRequest::new(
        cask.url.clone(),
        cask.sha256.clone(),
        cask.install_name.clone(),
    );
    if !cask.no_check {
        return request;
    }
    if !json["auto_updates"].as_bool().unwrap_or(false) {
        warn!(
            "No checksum defined for cask '{}', skipping verification.",
            cask.token
        );
    }
    request.unverified()
}

/// What it takes to turn a downloaded bottle into a store entry.
#[derive(Debug, Clone)]
pub(super) struct BottleJob {
    /// Position in the plan's bottle list.
    pub(super) index: usize,
    pub(super) formula_name: String,
    pub(super) version: String,
    pub(super) sha256: String,
    pub(super) url: String,
    pub(super) build_prefix: String,
}

impl BottleJob {
    pub(super) fn new(index: usize, item: &PlannedInstall) -> Self {
        let InstallMethod::Bottle(ref bottle) = item.method else {
            unreachable!()
        };
        Self {
            index,
            formula_name: item.formula.name.clone(),
            version: item.formula.effective_version(),
            sha256: bottle.sha256.clone(),
            url: bottle.url.clone(),
            build_prefix: bottle.build_prefix().to_string(),
        }
    }
}

/// Unpack and relocate a bottle while it downloads. The entry is published
/// by the downloader once the checksum matched, so [`prepare_bottle`] then
/// finds it ready; if anything goes wrong here the blob is unpacked the
/// usual way instead.
pub(super) fn stream_into_store(store: &Store, cellar: &Cellar, job: BottleJob) -> BodyConsumer {
    let store = store.clone();
    let cellar = cellar.clone();
    BodyConsumer {
        consume: Box::new(move |body| {
            let fingerprint = cellar.relocation_fingerprint(&job.build_prefix);
            let staged = store.stage_entry(&job.sha256, body, &fingerprint, |extracted| {
                cellar.relocate_extracted(
                    extracted,
                    &job.formula_name,
                    &job.version,
                    &job.build_prefix,
                )
            })?;
            Ok(Box::new(move || staged.commit().map(|_| ())) as Commit)
        }),
    }
}

/// Unpack and relocate a downloaded bottle into the store. The blocking
/// work runs off the async runtime, so several bottles can be prepared at
/// once while others are still downloading. A blob that turns out corrupt
/// is downloaded again, up to [`MAX_CORRUPTION_RETRIES`] times.
pub(super) async fn prepare_bottle(
    store: Store,
    cellar: Cellar,
    downloader: ParallelDownloader,
    job: BottleJob,
    mut blob_path: PathBuf,
    progress: Option<DownloadProgressCallback>,
) -> Result<PathBuf, Error> {
    let fingerprint = cellar.relocation_fingerprint(&job.build_prefix);
    let mut last_error = None;

    for attempt in 0..MAX_CORRUPTION_RETRIES {
        let (store, cellar, job_for_task, fingerprint, blob) = (
            store.clone(),
            cellar.clone(),
            job.clone(),
            fingerprint.clone(),
            blob_path.clone(),
        );
        let outcome = tokio::task::spawn_blocking(move || {
            let job = job_for_task;
            store.ensure_entry(&job.sha256, &blob, &fingerprint, |extracted| {
                cellar
                    .relocate_extracted(
                        extracted,
                        &job.formula_name,
                        &job.version,
                        &job.build_prefix,
                    )
                    .map_err(|e| relocation_failed(&job.formula_name, e))
            })
        })
        .await
        .map_err(|e| Error::ExecutionError {
            message: format!("bottle preparation task failed: {e}"),
        })?;

        match outcome {
            Ok(entry) => return Ok(entry),
            Err(Error::StoreCorruption { message }) => {
                downloader.remove_blob(&job.sha256);

                if attempt + 1 < MAX_CORRUPTION_RETRIES {
                    warn!(
                        formula = %job.formula_name,
                        attempt = attempt + 2,
                        max_retries = MAX_CORRUPTION_RETRIES,
                        "corrupted download detected; retrying"
                    );

                    let request = DownloadRequest::new(
                        job.url.clone(),
                        job.sha256.clone(),
                        job.formula_name.clone(),
                    );

                    match downloader.download_single(request, progress.clone()).await {
                        Ok(new_path) => blob_path = new_path,
                        Err(e) => {
                            last_error = Some(e);
                            break;
                        }
                    }
                } else {
                    last_error = Some(Error::StoreCorruption {
                        message: format!(
                            "{message}\n\nFailed after {MAX_CORRUPTION_RETRIES} attempts. The download may be corrupted at the source."
                        ),
                    });
                }
            }
            Err(e) => {
                last_error = Some(e);
                break;
            }
        }
    }

    Err(last_error.unwrap_or_else(|| Error::StoreCorruption {
        message: "extraction failed with unknown error".to_string(),
    }))
}

/// A bottle that unpacked but could not be relocated is not a corrupted
/// download: the same bytes fail the same way, so it is not downloaded again.
fn relocation_failed(formula: &str, error: Error) -> Error {
    Error::ExecutionError {
        message: format!("failed to relocate {formula}: {error}"),
    }
}

pub(super) fn dependency_cellar_path(
    cellar: &Cellar,
    installed_name: &str,
    version: &str,
) -> String {
    cellar
        .keg_path(formula_token(installed_name), version)
        .display()
        .to_string()
}

struct FailedInstallGuard<'a> {
    linker: &'a Linker,
    cellar: &'a Cellar,
    name: &'a str,
    version: &'a str,
    keg_path: &'a Path,
    unlink: bool,
    armed: bool,
}

impl<'a> FailedInstallGuard<'a> {
    fn new(
        linker: &'a Linker,
        cellar: &'a Cellar,
        name: &'a str,
        version: &'a str,
        keg_path: &'a Path,
        unlink: bool,
    ) -> Self {
        Self {
            linker,
            cellar,
            name,
            version,
            keg_path,
            unlink,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for FailedInstallGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            Installer::cleanup_failed_install(
                self.linker,
                self.cellar,
                self.name,
                self.version,
                self.keg_path,
                self.unlink,
            );
        }
    }
}

/// Copy the cask's binaries into the keg. Binaries inside a placed app
/// (`$APPDIR/...`) are symlinked instead, so they keep finding the bundle.
pub(super) fn stage_cask_binaries(
    extracted_root: &Path,
    appdir: Option<&Path>,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    let bin_dir = keg_path.join("bin");
    fs::create_dir_all(&bin_dir).map_err(Error::store("failed to create cask bin dir"))?;

    for binary in &cask.binaries {
        let source =
            resolve_cask_source_path(extracted_root, appdir, Some(keg_path), cask, &binary.source)?;
        if !source.exists() {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{}' binary source '{}' not found in archive",
                    cask.token, binary.source
                ),
            });
        }

        let target = bin_dir.join(&binary.target);
        if target.symlink_metadata().is_ok() {
            fs::remove_file(&target)
                .map_err(Error::store("failed to replace existing cask binary"))?;
        }

        // Homebrew links a binary to where it lies, so a script that finds
        // its files through its own path still works. That is the app or
        // this keg's staged directory; out of the shared store it is copied.
        if !source.starts_with(extracted_root) || extracted_root.starts_with(keg_path) {
            make_executable(&source)?;
            std::os::unix::fs::symlink(&source, &target)
                .map_err(Error::store("failed to link cask binary"))?;
            continue;
        }

        fs::copy(&source, &target).map_err(|e| Error::StoreCorruption {
            message: format!("failed to stage cask binary '{}': {e}", binary.target),
        })?;
        make_executable(&target)?;
    }

    Ok(())
}

/// Add execute permission to a file that has none, as Homebrew's `binary`
/// artifact does for its source.
fn make_executable(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)
            .map_err(Error::store("failed to read cask binary metadata"))?
            .permissions();
        if perms.mode() & 0o111 == 0 {
            perms.set_mode(0o755);
            fs::set_permissions(path, perms)
                .map_err(Error::store("failed to make cask binary executable"))?;
        }
    }
    Ok(())
}

fn stage_raw_cask_binary(
    blob_path: &Path,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    if cask.binaries.len() != 1 {
        return Err(Error::InvalidArgument {
            message: format!(
                "cask '{}' has {} binary artifacts but the download is a raw binary; expected exactly 1",
                cask.token,
                cask.binaries.len()
            ),
        });
    }

    let binary = &cask.binaries[0];
    let bin_dir = keg_path.join("bin");
    fs::create_dir_all(&bin_dir).map_err(Error::store("failed to create cask bin dir"))?;

    let target = bin_dir.join(&binary.target);
    if target.exists() {
        fs::remove_file(&target).map_err(Error::store("failed to replace existing cask binary"))?;
    }

    fs::copy(blob_path, &target).map_err(|e| Error::StoreCorruption {
        message: format!("failed to stage cask binary '{}': {e}", binary.target),
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
            .map_err(Error::store("failed to make staged cask binary executable"))?;
    }

    Ok(())
}

/// Where an artifact's `source` is: in the app for `$APPDIR/…`, in the keg
/// for `$HOMEBREW_PREFIX/…` (what the cask placed in the prefix is made
/// there), as given when absolute (a pkg's `/usr/local/…`), else in the
/// unpacked download.
pub(super) fn resolve_cask_source_path(
    extracted_root: &Path,
    appdir: Option<&Path>,
    keg: Option<&Path>,
    cask: &crate::installer::cask::ResolvedCask,
    source: &str,
) -> Result<std::path::PathBuf, Error> {
    let (root, source) = match source.strip_prefix("$APPDIR/") {
        Some(in_app) => {
            let appdir = appdir.ok_or_else(|| Error::InvalidArgument {
                message: format!(
                    "cask '{}' binary '{source}' points into an app the cask does not install",
                    cask.token
                ),
            })?;
            (appdir, in_app)
        }
        None => (extracted_root, source),
    };

    let mut normalized = source.to_string();
    let caskroom_prefix = format!("$HOMEBREW_PREFIX/Caskroom/{}/{}/", cask.token, cask.version);
    if let Some(stripped) = normalized.strip_prefix(&caskroom_prefix) {
        normalized = stripped.to_string();
    }

    if let (Some(rel), Some(keg)) = (normalized.strip_prefix("$HOMEBREW_PREFIX/"), keg) {
        normalized = keg.join(rel).to_string_lossy().into_owned();
    }

    let source_path = Path::new(&normalized);
    for component in source_path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{}' binary source '{}' cannot contain '..'",
                    cask.token, source
                ),
            });
        }
    }

    if source_path.is_absolute() {
        return Ok(source_path.to_path_buf());
    }
    Ok(root.join(source_path))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::cellar::Cellar;
    use crate::storage::db::Database;

    use super::*;

    #[test]
    fn dependency_cellar_path_uses_formula_token_for_tap_name() {
        let tmp = TempDir::new().unwrap();
        let cellar = Cellar::new(tmp.path()).unwrap();
        let path = dependency_cellar_path(&cellar, "hashicorp/tap/terraform", "1.10.0");

        assert!(path.ends_with("cellar/terraform/1.10.0"));
    }

    #[test]
    fn dependency_cellar_path_keeps_core_formula_name() {
        let tmp = TempDir::new().unwrap();
        let cellar = Cellar::new(tmp.path()).unwrap();
        let path = dependency_cellar_path(&cellar, "openssl@3", "3.3.2");

        assert!(path.ends_with("cellar/openssl@3/3.3.2"));
    }

    #[test]
    fn dependency_cellar_path_uses_name_from_db_record() {
        let tmp = TempDir::new().unwrap();
        let cellar = Cellar::new(tmp.path()).unwrap();

        let db_path = tmp.path().join("nb.sqlite3");
        let mut db = Database::open(&db_path).unwrap();
        let tx = db.transaction().unwrap();
        tx.record_install("hashicorp/tap/terraform", "1.10.0", "store-key")
            .unwrap();
        tx.commit().unwrap();

        let keg = db.get_installed("hashicorp/tap/terraform").unwrap();
        let path = dependency_cellar_path(&cellar, &keg.name, &keg.version);

        assert!(path.ends_with("cellar/terraform/1.10.0"));
    }

    #[test]
    fn a_relocation_failure_is_not_retried_as_a_corrupted_download() {
        // prepare_bottle downloads again only on StoreCorruption.
        let error = relocation_failed(
            "icu4c@78",
            Error::StoreCorruption {
                message: "install_name_tool failed".into(),
            },
        );
        assert!(matches!(error, Error::ExecutionError { .. }), "{error:?}");
        assert!(error.to_string().contains("failed to relocate icu4c@78"));
    }

    #[test]
    fn binaries_from_the_prefix_or_an_absolute_path_are_linked() {
        // android-commandlinetools: a binary in what its `artifact` moved
        // into the prefix; dotnet-sdk: one its pkg put in /usr/local.
        let tmp = TempDir::new().unwrap();
        let keg_path = tmp.path().join("keg");
        let staged = keg_path.join("staged");
        fs::create_dir_all(&staged).unwrap();
        let moved = keg_path.join("share/t/latest/bin/apkanalyzer");
        fs::create_dir_all(moved.parent().unwrap()).unwrap();
        fs::write(&moved, b"#!/bin/sh\n").unwrap();
        let absolute = tmp.path().join("usr-local/dotnet");
        fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        fs::write(&absolute, b"#!/bin/sh\n").unwrap();
        let cask = crate::installer::cask::ResolvedCask {
            token: "t".to_string(),
            version: "1".to_string(),
            binaries: vec![
                crate::installer::cask::CaskArtifact {
                    source: "$HOMEBREW_PREFIX/share/t/latest/bin/apkanalyzer".to_string(),
                    target: "apkanalyzer".to_string(),
                },
                crate::installer::cask::CaskArtifact {
                    source: absolute.display().to_string(),
                    target: "dotnet".to_string(),
                },
            ],
            ..Default::default()
        };

        stage_cask_binaries(&staged, None, &keg_path, &cask).unwrap();

        assert_eq!(
            fs::read_link(keg_path.join("bin/apkanalyzer")).unwrap(),
            moved
        );
        assert_eq!(
            fs::read_link(keg_path.join("bin/dotnet")).unwrap(),
            absolute
        );
    }

    #[test]
    fn staged_cask_binaries_are_links_into_the_staged_dir() {
        // gcloud finds its SDK by following its own symlink, as Homebrew
        // links a binary to where it lies in the staged path.
        let tmp = TempDir::new().unwrap();
        let keg_path = tmp.path().join("keg");
        let staged = keg_path.join("staged");
        let tool = staged.join("sdk/bin/tool");
        fs::create_dir_all(tool.parent().unwrap()).unwrap();
        fs::write(&tool, b"#!/bin/sh\n").unwrap();
        let cask = crate::installer::cask::ResolvedCask {
            install_name: "cask:sdk".to_string(),
            token: "sdk".to_string(),
            version: "1.0.0".to_string(),
            url: "https://example.com/sdk.tar.gz".to_string(),
            sha256: "aaa".to_string(),
            binaries: vec![crate::installer::cask::CaskArtifact {
                source: "sdk/bin/tool".to_string(),
                target: "tool".to_string(),
            }],
            apps: vec![],
            hooks: Default::default(),
            installers: Vec::new(),
            ..Default::default()
        };

        stage_cask_binaries(&staged, None, &keg_path, &cask).unwrap();

        assert_eq!(fs::read_link(keg_path.join("bin/tool")).unwrap(), tool);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&tool).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "the source is made executable");
        }
    }

    #[test]
    fn stage_raw_cask_binary_copies_and_marks_executable() {
        let tmp = TempDir::new().unwrap();
        let blob_path = tmp.path().join("claude");
        fs::write(&blob_path, b"#!/bin/sh\necho hello").unwrap();

        let keg_path = tmp.path().join("keg");
        let cask = crate::installer::cask::ResolvedCask {
            install_name: "cask:claude-code".to_string(),
            token: "claude-code".to_string(),
            version: "1.0.0".to_string(),
            url: "https://example.com/claude".to_string(),
            sha256: "aaa".to_string(),
            binaries: vec![crate::installer::cask::CaskArtifact {
                source: "claude".to_string(),
                target: "claude".to_string(),
            }],
            apps: vec![],
            hooks: Default::default(),
            installers: Vec::new(),
            ..Default::default()
        };

        stage_raw_cask_binary(&blob_path, &keg_path, &cask).unwrap();

        let target = keg_path.join("bin/claude");
        assert!(target.exists());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "#!/bin/sh\necho hello"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o755, 0o755);
        }
    }

    #[test]
    fn stage_raw_cask_binary_rejects_multiple_binaries() {
        let tmp = TempDir::new().unwrap();
        let blob_path = tmp.path().join("blob");
        fs::write(&blob_path, b"data").unwrap();

        let keg_path = tmp.path().join("keg");
        let cask = crate::installer::cask::ResolvedCask {
            install_name: "cask:multi".to_string(),
            token: "multi".to_string(),
            version: "1.0.0".to_string(),
            url: "https://example.com/multi".to_string(),
            sha256: "bbb".to_string(),
            binaries: vec![
                crate::installer::cask::CaskArtifact {
                    source: "a".to_string(),
                    target: "a".to_string(),
                },
                crate::installer::cask::CaskArtifact {
                    source: "b".to_string(),
                    target: "b".to_string(),
                },
            ],
            apps: vec![],
            hooks: Default::default(),
            installers: Vec::new(),
            ..Default::default()
        };

        let err = stage_raw_cask_binary(&blob_path, &keg_path, &cask).unwrap_err();
        assert!(err.to_string().contains("raw binary"));
    }
}
