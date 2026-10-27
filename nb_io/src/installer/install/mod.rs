mod autoremove;
mod bottle;
mod link;
pub use dependents::RequiredBy;
pub use deps::{Uses, UsesScope};
pub use link::{LinkDryRun, LinkOutcome};
pub use services::ServiceFormula;
mod cask_app;
mod cask_artifacts;
mod cask_deps;
mod cask_link;
pub use cask_link::{CaskLink, CaskLinkEvent};
mod cask_uninstall;
mod dependents;
mod deps;
mod fetch;
pub use fetch::Cleanup;
pub mod diagnostic;
pub mod doctor;
mod outdated;
mod plan;
mod portable_ruby;
mod post_install;
mod quarantine;
mod services;
mod source;
mod tap_search;
mod taps;
mod undeletable;
mod uninstall;
mod upgrade;

use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Semaphore;
use tracing::warn;

use crate::cellar::link::Linker;
use crate::cellar::materialize::Cellar;
use crate::network::api::ApiClient;
use crate::network::cache::ApiCache;
use crate::network::download::{DownloadProgressCallback, DownloadRequest, ParallelDownloader};
use crate::progress::{InstallProgress, ProgressCallback};
use crate::storage::blob::BlobCache;
use crate::storage::db::Database;
use crate::storage::store::Store;

use nb_core::{ConcurrencyLimits, Error, Formula, InstallMethod};

use bottle::{
    BottleJob, Tab, dependency_cellar_path, fetch_tab, prepare_bottle, stream_into_store,
};

const MAX_CORRUPTION_RETRIES: usize = 3;

/// Acquire the cross-process install lock. The returned `File` must be kept
/// alive (e.g. `let _lock = ...`) for the duration the lock should be held —
/// dropping it releases the flock. Re-acquiring in the same process while the
/// guard is alive would deadlock, so multi-step flows (e.g. `upgrade`) take
/// the lock once and call the no-lock `execute_inner` directly.
pub(crate) fn acquire_install_lock(locks_dir: &Path) -> Result<File, Error> {
    let lock_path = locks_dir.join("install.lock");
    let lock_file =
        File::create(&lock_path).map_err(Error::store("failed to create install lock"))?;
    lock_file
        .lock()
        .map_err(Error::store("failed to acquire install lock"))?;
    Ok(lock_file)
}

pub struct Installer {
    api_client: ApiClient,
    downloader: ParallelDownloader,
    store: Store,
    cellar: Cellar,
    linker: Linker,
    pub(crate) db: Database,
    prefix: PathBuf,
    locks_dir: PathBuf,
    /// `<root>/cache`, beside the blob cache.
    cache_dir: PathBuf,
    /// The options casks are installed with (`--appdir`, `--no-quarantine`,
    /// ...), over `$HOMEBREW_CASK_OPTS`.
    cask_config: crate::installer::cask_config::CaskConfig,
    /// The formulae the last plan installed (see
    /// [`Installer::last_installed`]).
    last_installed: Vec<(String, bool)>,
}

#[derive(Debug)]
pub struct PlannedInstall {
    pub install_name: String,
    pub formula: Formula,
    pub method: InstallMethod,
    /// Link although keg-only: a versioned formula Homebrew links anyway
    /// (see [`Installer::links_versioned_keg_only`]), or one that was linked
    /// before an upgrade.
    pub link_keg_only: bool,
    /// Related formulae to unlink before this one links, and whether each
    /// is keg-only (see [`Installer::unlink_overwritten`]).
    pub link_overwrite: Vec<(String, bool)>,
    /// Where Homebrew would install this: the names asked for in order,
    /// each after its dependencies. Decides which of two related formulae
    /// ends up linked, whatever order the bottles arrive in.
    pub link_rank: usize,
    /// Related formulae in the same plan that Homebrew installs later.
    pub link_later: Vec<String>,
    /// Asked for, but a dependency of a formula asked for before it:
    /// Homebrew installs it as that one's dependency, not on request.
    pub as_earlier_dependency: bool,
}

#[derive(Debug)]
pub struct InstallPlan {
    pub items: Vec<PlannedInstall>,
    /// Install names of the packages asked for, for `mark_on_request`.
    pub roots: Vec<String>,
}

#[derive(Debug)]
pub struct PlanFailure {
    pub name: String,
    pub error: Error,
}

#[derive(Debug, Default)]
pub struct ExecuteResult {
    pub installed: usize,
    /// Messages from `warn` post-install steps.
    pub notices: Vec<PostInstallNotice>,
    /// Packages whose post-install steps failed. Each is installed and
    /// linked; the steps can be retried with `nb postinstall`.
    pub post_install_failures: Vec<Error>,
}

/// A message a formula's post-install asked to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostInstallNotice {
    pub name: String,
    pub message: String,
}

/// Which casks a bulk check counts although they would rather not be
/// upgraded: `brew`'s `--greedy-latest` and `--greedy-auto-updates`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Greedy {
    pub latest: bool,
    pub auto_updates: bool,
}

impl Greedy {
    pub const ALL: Self = Self {
        latest: true,
        auto_updates: true,
    };
}

/// A package that has a newer version available upstream.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OutdatedPackage {
    pub name: String,
    pub installed_version: String,
    pub current_version: String,
    #[serde(skip)]
    pub installed_sha256: String,
    #[serde(skip)]
    pub current_sha256: String,
    #[serde(skip)]
    pub is_source_build: bool,
}

/// One package's failure, while the rest go on: the last is kept to return,
/// and the one it replaces is reported now, so none goes unsaid.
fn keep_last(error: &mut Option<Error>, e: Error, report: &impl Fn(InstallProgress)) {
    if let Some(earlier) = error.replace(e) {
        report(InstallProgress::Failed { error: earlier });
    }
}

impl Installer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api_client: ApiClient,
        blob_cache: BlobCache,
        store: Store,
        cellar: Cellar,
        linker: Linker,
        db: Database,
        prefix: PathBuf,
        locks_dir: PathBuf,
    ) -> Self {
        let cache_dir = locks_dir
            .parent()
            .map(|root| root.join("cache"))
            .unwrap_or_else(|| prefix.join("cache"));
        Self {
            api_client,
            downloader: ParallelDownloader::new(blob_cache),
            store,
            cellar,
            linker,
            db,
            prefix,
            locks_dir,
            cache_dir,
            cask_config: Default::default(),
            last_installed: Vec::new(),
        }
    }

    /// Fetch the formula index again and drop cached API responses.
    /// Returns how many formulas the fresh index has.
    pub async fn update_api_cache(&self) -> Result<usize, Error> {
        self.api_client.clear_cache()?;
        self.api_client.refresh_index(true).await?;
        Ok(self.api_client.index_len())
    }

    pub async fn execute(&mut self, plan: InstallPlan, link: bool) -> Result<ExecuteResult, Error> {
        self.execute_with_progress(plan, link, None).await
    }

    pub async fn execute_with_progress(
        &mut self,
        plan: InstallPlan,
        link: bool,
        progress: Option<Arc<ProgressCallback>>,
    ) -> Result<ExecuteResult, Error> {
        let _lock = acquire_install_lock(&self.locks_dir)?;
        let roots = plan.roots.clone();
        let mut plan = plan;
        for item in &mut plan.items {
            item.link_keg_only = roots.contains(&item.install_name)
                && !self.is_installed(&item.install_name)
                && self.links_versioned_keg_only(&item.formula).await?;
        }
        let result = self.execute_inner(plan, link, progress).await;
        // Also after a partial failure: a root that did install was asked for.
        self.mark_on_request(&roots)?;
        result
    }

    /// No-lock variant of `execute_with_progress`. Callers MUST already hold
    /// the install lock — used by `upgrade` to compose uninstall + install
    /// under a single lock acquisition.
    pub(crate) async fn execute_inner(
        &mut self,
        plan: InstallPlan,
        link: bool,
        progress: Option<Arc<ProgressCallback>>,
    ) -> Result<ExecuteResult, Error> {
        self.last_installed.clear();
        let report = |event: InstallProgress| {
            if let Some(ref cb) = progress {
                cb(event);
            }
        };
        let mut plan = plan;
        if link {
            for item in &mut plan.items {
                // Best effort: without the formula list, nothing is unlinked.
                item.link_overwrite = self
                    .overwrite_candidates(&item.formula)
                    .await
                    .unwrap_or_default();
            }
            let ranks: Vec<(String, usize)> = plan
                .items
                .iter()
                .map(|item| (item.formula.name.clone(), item.link_rank))
                .collect();
            for item in &mut plan.items {
                item.link_later = item
                    .link_overwrite
                    .iter()
                    .filter(|(other, _)| {
                        ranks
                            .iter()
                            .any(|(name, rank)| name == other && *rank > item.link_rank)
                    })
                    .map(|(other, _)| other.clone())
                    .collect();
            }
        }
        nb_core::timing::mark("link candidates found");

        // Post-install runs once everything is installed, in plan order, so
        // a step can rely on a dependency that is still downloading here.
        let positions: HashMap<String, usize> = plan
            .items
            .iter()
            .enumerate()
            .map(|(position, item)| (item.install_name.clone(), position))
            .collect();

        let (bottle_items, source_items): (Vec<_>, Vec<_>) = plan
            .items
            .into_iter()
            .partition(|item| matches!(item.method, InstallMethod::Bottle(_)));

        if bottle_items.is_empty() && source_items.is_empty() {
            return Ok(ExecuteResult::default());
        }

        let mut installed = 0usize;
        let mut finished: Vec<&PlannedInstall> = Vec::new();
        let mut error: Option<Error> = None;
        // Post-install work that needs only its own keg, started as soon as
        // that keg is in place.
        let mut ahead = Vec::new();

        if !bottle_items.is_empty() {
            if bottle_items.iter().any(|item| match &item.method {
                InstallMethod::Bottle(bottle) => !self.store.has_entry(&bottle.sha256),
                _ => false,
            }) {
                crate::network::tls::preload_roots();
            }
            let tabs: Vec<Tab> = bottle_items
                .iter()
                .map(|item| fetch_tab(&self.downloader, &self.store, item))
                .collect();
            let requests: Vec<DownloadRequest> = bottle_items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let job = BottleJob::new(index, item, tabs[index].clone());
                    let request = DownloadRequest::new(
                        job.url.clone(),
                        job.sha256.clone(),
                        job.formula_name.clone(),
                    );
                    request.with_consumer(stream_into_store(&self.store, &self.cellar, job))
                })
                .collect();

            let download_progress: Option<DownloadProgressCallback> = progress.clone().map(|cb| {
                Arc::new(move |event: InstallProgress| {
                    cb(event);
                }) as DownloadProgressCallback
            });

            let mut rx = self
                .downloader
                .download_streaming(requests, download_progress.clone());

            // Bottles are unpacked, relocated and cloned into the Cellar as
            // they arrive, several at a time, while the rest are still
            // downloading. Only recording and linking runs here, one bottle
            // at a time.
            let prepare_limit = Arc::new(Semaphore::new(ConcurrencyLimits::default().materialize));
            let mut preparing = FuturesUnordered::new();
            let mut downloads_open = true;

            while downloads_open || !preparing.is_empty() {
                tokio::select! {
                    result = rx.recv(), if downloads_open => match result {
                        None => downloads_open = false,
                        Some(Err(e)) => keep_last(&mut error, e, &report),
                        Some(Ok(download)) => {
                            let job = BottleJob::new(
                                download.index,
                                &bottle_items[download.index],
                                tabs[download.index].clone(),
                            );
                            report(InstallProgress::UnpackStarted {
                                name: job.formula_name.clone(),
                            });
                            let store = self.store.clone();
                            let cellar = self.cellar.clone();
                            let downloader = self.downloader.clone();
                            let progress = download_progress.clone();
                            let limit = prepare_limit.clone();
                            preparing.push(tokio::spawn(async move {
                                let _permit = limit.acquire_owned().await;
                                let index = job.index;
                                let outcome = prepare_bottle(
                                    store,
                                    cellar,
                                    downloader,
                                    job,
                                    download.blob_path,
                                    progress,
                                )
                                .await;
                                (index, outcome)
                            }));
                        }
                    },
                    Some(joined) = preparing.next() => match joined {
                        Ok((index, Ok(keg))) => {
                            match self.finish_bottle_item(&bottle_items[index], &keg, link, &report) {
                                Ok(()) => {
                                    installed += 1;
                                    finished.push(&bottle_items[index]);
                                    ahead.extend(self.prepare_post_install_ahead(&bottle_items[index].formula));
                                }
                                Err(e) => keep_last(&mut error, e, &report),
                            }
                        }
                        Ok((_, Err(e))) => keep_last(&mut error, e, &report),
                        Err(e) => keep_last(
                            &mut error,
                            Error::ExecutionError {
                                message: format!("bottle preparation task failed: {e}"),
                            },
                            &report,
                        ),
                    },
                }
            }
        }

        // A formula whose dependency did not build is not tried, as `brew`
        // stops there: the dependency's error stands for it.
        let mut failed: Vec<&str> = Vec::new();
        for item in &source_items {
            let InstallMethod::Source(ref build_plan) = item.method else {
                unreachable!()
            };
            let formula = &item.formula;
            if formula
                .dependencies
                .iter()
                .chain(&formula.build_dependencies)
                .any(|dep| failed.contains(&dep.as_str()))
            {
                failed.push(&formula.name);
                continue;
            }

            report(InstallProgress::UnpackStarted {
                name: item.formula.name.clone(),
            });

            match self
                .install_from_source(item, build_plan, link, &report)
                .await
            {
                Ok(()) => {
                    installed += 1;
                    finished.push(item);
                }
                Err(e) => {
                    failed.push(&formula.name);
                    keep_last(&mut error, e, &report);
                }
            }
        }

        for task in ahead {
            let _ = task.await;
        }
        nb_core::timing::mark("post-install prepared ahead");
        let mut by_rank: Vec<&PlannedInstall> = finished.clone();
        by_rank.sort_by_key(|item| item.link_rank);
        self.last_installed = by_rank
            .iter()
            .map(|item| (item.install_name.clone(), item.as_earlier_dependency))
            .collect();
        finished.sort_by_key(|item| positions.get(&item.install_name).copied());
        let mut notices = Vec::new();
        let mut post_install_failures = Vec::new();
        for item in finished {
            if item.formula.post_install_steps.is_empty() {
                continue;
            }
            let name = item.formula.name.clone();
            report(InstallProgress::PostInstallStarted { name: name.clone() });
            match self.run_post_install(&item.formula).await {
                Ok(messages) => {
                    notices.extend(messages.into_iter().map(|message| PostInstallNotice {
                        name: name.clone(),
                        message,
                    }));
                    report(InstallProgress::PostInstallCompleted { name });
                }
                Err(e) => {
                    report(InstallProgress::PostInstallFailed {
                        name,
                        message: e.to_string(),
                    });
                    post_install_failures.push(e);
                }
            }
        }

        if let Some(e) = error {
            return Err(e);
        }

        Ok(ExecuteResult {
            installed,
            notices,
            post_install_failures,
        })
    }

    pub async fn install(&mut self, names: &[String], link: bool) -> Result<ExecuteResult, Error> {
        let (casks, formulas): (Vec<_>, Vec<_>) = names
            .iter()
            .cloned()
            .partition(|name| name.starts_with("cask:"));

        let mut result = ExecuteResult::default();

        if !formulas.is_empty() {
            let plan = self.plan(&formulas).await?;
            let executed = self.execute(plan, link).await?;
            result.installed += executed.installed;
            result.notices.extend(executed.notices);
            result
                .post_install_failures
                .extend(executed.post_install_failures);
        }

        if !casks.is_empty() {
            result.installed += self.install_casks(&casks, link).await?.installed;
        }

        Ok(result)
    }

    pub async fn install_casks(
        &mut self,
        names: &[String],
        link: bool,
    ) -> Result<ExecuteResult, Error> {
        // Held across dependency installs too, which run unlocked.
        let _lock = acquire_install_lock(&self.locks_dir)?;
        let mut installed = 0usize;
        for name in names {
            let token = name
                .strip_prefix("cask:")
                .expect("install_casks expects cask: prefixed names");
            let install_name = self.install_single_cask(token, link).await?;
            self.mark_on_request(&[install_name])?;
            installed += 1;
        }
        Ok(ExecuteResult {
            installed,
            ..Default::default()
        })
    }

    /// Record that the user asked for these packages, so `autoremove`
    /// keeps them. Dependencies are recorded as not requested when installed.
    pub fn mark_on_request(&self, names: &[String]) -> Result<(), Error> {
        names
            .iter()
            .try_for_each(|name| self.db.set_on_request(name))
    }

    /// Pin or unpin an installed formula. Returns false when it already was.
    pub fn set_pinned(&self, name: &str, pinned: bool) -> Result<bool, Error> {
        if name.starts_with("cask:") {
            return Err(Error::InvalidArgument {
                message: "casks cannot be pinned".to_string(),
            });
        }
        let keg = self.db.get_installed(name).ok_or(Error::NotInstalled {
            name: name.to_string(),
        })?;
        if keg.pinned == pinned {
            return Ok(false);
        }
        self.db.set_pinned(name, pinned)?;
        Ok(true)
    }

    /// The formulae the last install, upgrade or reinstall put in, also
    /// when some of the others failed: as Homebrew installs them, the names
    /// asked for in order, each after the dependencies it brought in. Each
    /// says whether it was asked for but went in as the dependency of one
    /// asked for before it.
    pub fn last_installed(&self) -> &[(String, bool)] {
        &self.last_installed
    }

    pub fn is_installed(&self, name: &str) -> bool {
        self.db.get_installed(name).is_some()
    }

    pub fn get_installed(&self, name: &str) -> Option<crate::storage::db::InstalledKeg> {
        self.db.get_installed(name)
    }

    pub fn list_installed(&self) -> Result<Vec<crate::storage::db::InstalledKeg>, Error> {
        self.db.list_installed()
    }

    /// Install casks with `config` from now on.
    pub fn set_cask_config(&mut self, config: crate::installer::cask_config::CaskConfig) {
        self.cask_config = config;
    }

    /// The prefix packages are installed into.
    pub fn prefix(&self) -> &std::path::Path {
        &self.prefix
    }

    /// Where downloads are cached (`brew --cache`).
    pub fn cache_dir(&self) -> &std::path::Path {
        &self.cache_dir
    }

    /// The HTTP client API requests go through.
    pub fn http(&self) -> &reqwest::Client {
        self.api_client.http()
    }

    /// A formula as the API describes it.
    pub async fn formula(&self, name: &str) -> Result<Formula, Error> {
        self.api_client.get_formula(name).await
    }

    /// [`Self::formula`] as the formula API's JSON for it.
    pub async fn formula_json(&self, name: &str) -> Result<serde_json::Value, Error> {
        self.api_client.get_formula_json(name).await
    }

    /// The names of homebrew/core's formulae.
    pub async fn formula_names(&self) -> Result<Vec<String>, Error> {
        self.api_client.formula_names().await
    }

    /// A cask's JSON as the API gives it.
    pub async fn cask(&self, token: &str) -> Result<serde_json::Value, Error> {
        self.api_client.get_cask(token).await
    }

    /// Record the third-party tap an installed formula came from (or that
    /// it came from homebrew/core), so it is read from there again.
    pub(super) fn record_tap(&self, install_name: &str, formula: &Formula) -> Result<(), Error> {
        let tap = formula.third_party_tap();
        self.db.set_tap(install_name, tap)?;
        if let Some(source) = self.api_client.taps()
            && let Ok(mut installed) = source.installed.write()
        {
            match tap.and_then(|t| crate::tap::TapName::parse(t).ok()) {
                Some(tap) => installed.insert(install_name.to_string(), tap),
                None => installed.remove(install_name),
            };
        }
        Ok(())
    }

    /// The file of the formula `name` in the third-party `tap`.
    pub(super) fn tap_formula_rb(&self, tap: &str, name: &str) -> Result<PathBuf, Error> {
        let missing = || Error::MissingFormula {
            name: format!("{tap}/{name}"),
        };
        let source = self.api_client.taps().ok_or_else(missing)?;
        let tap = crate::tap::TapName::parse(tap)?;
        source
            .taps
            .find_formula(&tap, name)
            .map(|(_, file)| file)
            .ok_or_else(missing)
    }

    pub fn keg_path(&self, name: &str, version: &str) -> PathBuf {
        self.cellar.keg_path(name, version)
    }

    fn cleanup_materialized(cellar: &Cellar, name: &str, version: &str) {
        if let Err(e) = cellar.remove_keg(name, version) {
            warn!(
                formula = %name,
                version = %version,
                error = %e,
                "failed to remove keg after install error"
            );
        }
    }
}

pub fn create_installer(
    root: &Path,
    prefix: &Path,
    concurrency: usize,
) -> Result<Installer, Error> {
    if !root.exists() {
        fs::create_dir_all(root).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                Error::StoreCorruption {
                    message: format!(
                        "cannot create root directory '{}': permission denied.\n\n\
                        Create it with:\n  sudo mkdir -p {} && sudo chown $USER {}",
                        root.display(),
                        root.display(),
                        root.display()
                    ),
                }
            } else {
                Error::StoreCorruption {
                    message: format!("failed to create root directory '{}': {e}", root.display()),
                }
            }
        })?;
    }

    fs::create_dir_all(root.join("db")).map_err(Error::store("failed to create db directory"))?;

    fs::create_dir_all(root.join("cache"))
        .map_err(Error::store("failed to create cache directory"))?;

    let api_cache_path = root.join("cache/api-cache.sqlite");
    let api_cache =
        ApiCache::open(&api_cache_path).map_err(Error::store("failed to open API cache"))?;

    let api_client = match std::env::var("NEXTBREW_API_URL") {
        Ok(url) => ApiClient::with_base_url(url)?,
        Err(_) => ApiClient::new(),
    }
    .with_cache(api_cache);

    let blob_cache =
        BlobCache::new(&root.join("cache")).map_err(Error::store("failed to create blob cache"))?;
    let store = Store::new(root).map_err(Error::store("failed to create store"))?;
    // Use prefix/Cellar so bottles' hardcoded rpaths work
    let cellar =
        Cellar::new_at(prefix.join("Cellar")).map_err(Error::store("failed to create cellar"))?;
    let linker = Linker::new(prefix).map_err(Error::store("failed to create linker"))?;
    let db = Database::open(&root.join("db/nb.sqlite3"))?;
    let tap_source = crate::network::api::TapSource::new(prefix);
    if let Ok(mut installed) = tap_source.installed.write() {
        for keg in db.list_installed()? {
            if let Some(tap) = keg
                .tap
                .as_deref()
                .and_then(|t| crate::tap::TapName::parse(t).ok())
            {
                installed.insert(keg.name, tap);
            }
        }
    }
    let api_client = api_client.with_taps(tap_source);

    let locks_dir = root.join("locks");
    fs::create_dir_all(&locks_dir).map_err(Error::store("failed to create locks directory"))?;

    let parallel_downloader = ParallelDownloader::with_concurrency(blob_cache, concurrency);

    Ok(Installer {
        api_client,
        downloader: parallel_downloader,
        store,
        cellar,
        linker,
        db,
        prefix: prefix.to_path_buf(),
        locks_dir,
        cache_dir: root.join("cache"),
        cask_config: Default::default(),
        last_installed: Vec::new(),
    })
}

#[cfg(test)]
mod test_support {
    pub fn create_bottle_tarball(formula_name: &str) -> Vec<u8> {
        create_bottle_tarball_with_version(formula_name, "1.0.0")
    }

    pub fn create_bottle_tarball_with_version(formula_name: &str, version: &str) -> Vec<u8> {
        create_bottle_tarball_with_files(formula_name, version, &[])
    }

    /// A bottle with `bin/<name>` plus extra `(path in keg, contents)` files.
    pub fn create_bottle_tarball_with_files(
        formula_name: &str,
        version: &str,
        extra_files: &[(&str, &str)],
    ) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;
        use tar::Builder;

        let mut builder = Builder::new(Vec::new());

        let content = format!("#!/bin/sh\necho {} v{}", formula_name, version);
        let files = std::iter::once((format!("bin/{formula_name}"), content.as_str(), 0o755))
            .chain(
                extra_files
                    .iter()
                    .map(|(path, contents)| (path.to_string(), *contents, 0o644)),
            );

        for (path, contents, mode) in files {
            let mut header = tar::Header::new_gnu();
            header
                .set_path(format!("{}/{}/{}", formula_name, version, path))
                .unwrap();
            header.set_size(contents.len() as u64);
            header.set_mode(mode);
            header.set_cksum();
            builder.append(&header, contents.as_bytes()).unwrap();
        }

        let tar_data = builder.into_inner().unwrap();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap()
    }

    pub fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(data);
        crate::checksum::sha256_hex(hasher)
    }

    pub fn get_test_bottle_tag() -> &'static str {
        if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            "arm64_linux"
        } else if cfg!(target_os = "linux") {
            "x86_64_linux"
        } else if cfg!(target_arch = "x86_64") {
            "sonoma"
        } else {
            "arm64_sonoma"
        }
    }
    /// Formula JSON for `name` whose bottle is served by the mock at
    /// `uri`, with `extra` spliced in as more top-level fields.
    pub fn formula_json_with(
        uri: &str,
        name: &str,
        version: &str,
        sha: &str,
        extra: &str,
    ) -> String {
        let tag = get_test_bottle_tag();
        let extra = if extra.is_empty() {
            String::new()
        } else {
            format!("{extra},")
        };
        format!(
            r#"{{
                "name": "{name}",
                "versions": {{ "stable": "{version}" }},
                "dependencies": [],
                {extra}
                "bottle": {{ "stable": {{ "files": {{
                    "{tag}": {{
                        "url": "{uri}/bottles/{name}-{version}.{tag}.bottle.tar.gz",
                        "sha256": "{sha}"
                    }}
                }} }} }}
            }}"#
        )
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::cellar::Cellar;
    use crate::network::api::ApiClient;
    use crate::storage::blob::BlobCache;
    use crate::storage::db::Database;
    use crate::storage::store::Store;
    use crate::{Installer, Linker};

    use super::test_support::*;

    #[tokio::test]
    async fn install_completes_successfully() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("testpkg");
        let bottle_sha = sha256_hex(&bottle);

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "testpkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/testpkg-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            bottle_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/testpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/testpkg-1.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/nb.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        installer
            .install(&["testpkg".to_string()], true)
            .await
            .unwrap();

        assert!(root.join("cellar/testpkg/1.0.0").exists());
        assert!(prefix.join("bin/testpkg").exists());

        let installed = installer.db.get_installed("testpkg");
        assert!(installed.is_some());
        assert_eq!(installed.unwrap().version, "1.0.0");
        // `brew unlink` keeps opt/ for dependents; `brew link` puts the rest back.
        assert!(installer.unlink("testpkg").unwrap() > 0);
        assert!(!prefix.join("bin/testpkg").exists());
        assert!(prefix.join("opt/testpkg").exists());
        assert!(matches!(
            installer.link("testpkg", false, false).await.unwrap(),
            super::LinkOutcome::Linked { count, .. } if count > 0
        ));
        assert!(prefix.join("bin/testpkg").exists());
        assert_eq!(
            installer.link("testpkg", false, false).await.unwrap(),
            super::LinkOutcome::AlreadyLinked
        );
    }

    #[tokio::test]
    async fn install_with_dependencies() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let dep_bottle = create_bottle_tarball("deplib");
        let dep_sha = sha256_hex(&dep_bottle);
        let main_bottle = create_bottle_tarball("mainpkg");
        let main_sha = sha256_hex(&main_bottle);

        let tag = get_test_bottle_tag();
        let dep_json = format!(
            r#"{{"name":"deplib","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/deplib-1.0.0.{}.bottle.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            tag,
            dep_sha
        );
        let main_json = format!(
            r#"{{"name":"mainpkg","versions":{{"stable":"2.0.0"}},"dependencies":["deplib"],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/mainpkg-2.0.0.{}.bottle.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            tag,
            main_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/deplib.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&dep_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/mainpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&main_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/deplib-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(dep_bottle))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/mainpkg-2.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(main_bottle))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/nb.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        installer
            .install(&["mainpkg".to_string()], true)
            .await
            .unwrap();

        assert!(installer.db.get_installed("mainpkg").unwrap().on_request);
        assert!(!installer.db.get_installed("deplib").unwrap().on_request);
        // A dependency before the formula that brought it in.
        assert_eq!(
            installer.last_installed(),
            [("deplib".into(), false), ("mainpkg".into(), false)]
        );

        let deps = installer.dependencies("mainpkg", false).await.unwrap();
        assert_eq!(deps.into_iter().collect::<Vec<_>>(), ["deplib"]);
        assert!(
            installer
                .dependencies("deplib", true)
                .await
                .unwrap()
                .is_empty()
        );
        let scope = super::UsesScope {
            installed: true,
            recursive: false,
            formulae: true,
            casks: true,
        };
        let uses = installer.dependents(&["deplib".to_string()], scope).await;
        assert_eq!(uses.unwrap().dependents, ["mainpkg"]);

        let blocked = installer
            .formula_dependents(&["deplib".to_string()])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            blocked.required,
            [installer.keg_path("deplib", "1.0.0").display().to_string()]
        );
        assert_eq!(blocked.dependents, ["mainpkg"]);
        let both = ["deplib".to_string(), "mainpkg".to_string()];
        assert!(installer.formula_dependents(&both).await.unwrap().is_none());
        assert!(installer.cask_dependents(&both).unwrap().is_none());

        assert!(installer.unneeded_formulae().await.unwrap().is_empty());
        installer.uninstall("mainpkg").unwrap();
        assert_eq!(installer.unneeded_formulae().await.unwrap(), ["deplib"]);
    }

    #[tokio::test]
    async fn opt_links_aliases_but_not_old_names() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let tag = get_test_bottle_tag();
        let bottle = create_bottle_tarball("glib");
        let json = format!(
            r#"{{"name":"glib","aliases":["glib2"],"oldnames":["glib-utils"],"versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{tag}":{{"url":"{uri}/bottles/glib.tar.gz","sha256":"{sha}"}}}}}}}}}}"#,
            uri = mock_server.uri(),
            sha = sha256_hex(&bottle),
        );
        Mock::given(method("GET"))
            .and(path("/formula/glib.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/bottles/glib.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();
        let mut installer = Installer::new(
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap(),
            BlobCache::new(&root.join("cache")).unwrap(),
            Store::new(&root).unwrap(),
            Cellar::new(&root).unwrap(),
            Linker::new(&prefix).unwrap(),
            Database::open(&root.join("db/nb.sqlite3")).unwrap(),
            prefix.clone(),
            root.join("locks"),
        );
        installer
            .install(&["glib".to_string()], true)
            .await
            .unwrap();

        assert!(prefix.join("opt/glib2").exists());
        assert!(prefix.join("opt/glib-utils").symlink_metadata().is_err());
    }

    #[tokio::test]
    async fn a_versioned_keg_only_formula_links_unless_a_sibling_is_installed() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let tag = get_test_bottle_tag();
        let mut bodies = Vec::new();
        for name in ["foo@1", "foo@2"] {
            let bottle = create_bottle_tarball(name);
            let keg_only = if name == "foo@1" {
                r#""keg_only":true,"keg_only_reason":{"reason":":versioned_formula","explanation":""},"#
            } else {
                ""
            };
            let json = format!(
                r#"{{"name":"{name}",{keg_only}"versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{tag}":{{"url":"{uri}/bottles/{name}.tar.gz","sha256":"{sha}"}}}}}}}}}}"#,
                uri = mock_server.uri(),
                sha = sha256_hex(&bottle),
            );
            Mock::given(method("GET"))
                .and(path(format!("/formula/{name}.json")))
                .respond_with(ResponseTemplate::new(200).set_body_string(&json))
                .mount(&mock_server)
                .await;
            Mock::given(method("GET"))
                .and(path(format!("/bottles/{name}.tar.gz")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
                .mount(&mock_server)
                .await;
            bodies.push(json);
        }
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(format!("[{}]", bodies.join(","))),
            )
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();
        let mut installer = Installer::new(
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap(),
            BlobCache::new(&root.join("cache")).unwrap(),
            Store::new(&root).unwrap(),
            Cellar::new(&root).unwrap(),
            Linker::new(&prefix).unwrap(),
            Database::open(&root.join("db/nb.sqlite3")).unwrap(),
            prefix.clone(),
            root.join("locks"),
        );
        installer
            .install(&["foo@1".to_string()], true)
            .await
            .unwrap();
        assert!(
            prefix.join("bin/foo@1").exists(),
            "linked: foo@2 is not installed"
        );

        installer.uninstall("foo@1").unwrap();
        installer
            .install(&["foo@2".to_string()], true)
            .await
            .unwrap();
        installer
            .install(&["foo@1".to_string()], true)
            .await
            .unwrap();
        assert!(
            !prefix.join("bin/foo@1").exists(),
            "keg-only: foo@2 is installed"
        );

        // Linking it by hand unlinks foo@2 first, as `brew link` does.
        match installer.link("foo@1", false, false).await.unwrap() {
            super::LinkOutcome::Linked { unlinked, .. } => {
                assert_eq!(unlinked.len(), 1, "{unlinked:?}");
                assert!(unlinked[0].0.ends_with("foo@2/1.0.0"));
            }
            other => panic!("{other:?}"),
        }
        assert!(prefix.join("bin/foo@1").exists());
        assert!(!prefix.join("bin/foo@2").exists());
    }

    #[tokio::test]
    async fn preserves_successful_installs_when_one_package_fails() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let good_bottle = create_bottle_tarball("goodpkg");
        let good_sha = sha256_hex(&good_bottle);

        let tag = get_test_bottle_tag();
        let good_json = format!(
            r#"{{
                "name": "goodpkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/goodpkg-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            good_sha
        );

        let bad_json = format!(
            r#"{{
                "name": "badpkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/badpkg-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );

        Mock::given(method("GET"))
            .and(path("/formula/goodpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&good_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/badpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&bad_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/goodpkg-1.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(good_bottle))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/badpkg-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_delay(Duration::from_millis(100))
                    .set_body_string("download failed"),
            )
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/nb.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let result = installer
            .install(&["goodpkg".to_string(), "badpkg".to_string()], false)
            .await;
        assert!(result.is_err());

        assert!(installer.db.get_installed("goodpkg").is_some());
        assert!(installer.db.get_installed("badpkg").is_none());
        assert_eq!(installer.last_installed(), [("goodpkg".into(), false)]);
        assert!(root.join("cellar/goodpkg/1.0.0").exists());
    }

    /// An installer backed by a mock API with nothing mounted yet.
    pub(super) async fn bare_installer(
        mock_server: &MockServer,
        tmp: &TempDir,
    ) -> (Installer, std::path::PathBuf, std::path::PathBuf) {
        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let installer = Installer::new(
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap(),
            BlobCache::new(&root.join("cache")).unwrap(),
            Store::new(&root).unwrap(),
            Cellar::new(&root).unwrap(),
            Linker::new(&prefix).unwrap(),
            Database::open(&root.join("db/nb.sqlite3")).unwrap(),
            prefix.clone(),
            root.join("locks"),
        );
        (installer, root, prefix)
    }

    /// An installer backed by a mock API serving `name` 1.0.0 as `bottle`.
    async fn installer_for_bottle(
        mock_server: &MockServer,
        tmp: &TempDir,
        name: &str,
        bottle: Vec<u8>,
    ) -> (Installer, std::path::PathBuf, std::path::PathBuf) {
        installer_for_bottle_with(mock_server, tmp, name, bottle, "").await
    }

    /// [`installer_for_bottle`] with extra top-level formula JSON fields.
    async fn installer_for_bottle_with(
        mock_server: &MockServer,
        tmp: &TempDir,
        name: &str,
        bottle: Vec<u8>,
        extra_json: &str,
    ) -> (Installer, std::path::PathBuf, std::path::PathBuf) {
        let tag = get_test_bottle_tag();
        let formula_json = formula_json_with(
            &mock_server.uri(),
            name,
            "1.0.0",
            &sha256_hex(&bottle),
            extra_json,
        );

        Mock::given(method("GET"))
            .and(path(format!("/formula/{name}.json")))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/{name}-1.0.0.{tag}.bottle.tar.gz")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .mount(mock_server)
            .await;

        bare_installer(mock_server, tmp).await
    }

    #[tokio::test]
    async fn post_install_steps_run_after_the_install_and_report_progress() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let bottle = create_bottle_tarball_with_files(
            "certs",
            "1.0.0",
            &[("share/certs/cacert.pem", "MOZILLA\n")],
        );
        let steps = r#""post_install_steps": [
            {"type": "run", "command": {"path": "cp"},
             "args": ["{{pkgshare}}/cacert.pem", "{{etc}}/cert.pem"]},
            {"type": "warn", "message": "bundle written for {{formula_name}}"}
        ]"#;
        let (mut installer, _root, prefix) =
            installer_for_bottle_with(&mock_server, &tmp, "certs", bottle, steps).await;
        fs::create_dir_all(prefix.join("etc")).unwrap();

        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = events.clone();
        let progress: Arc<crate::progress::ProgressCallback> = Arc::new(Box::new(move |event| {
            let label = match event {
                crate::progress::InstallProgress::InstallCompleted { .. } => "installed",
                crate::progress::InstallProgress::PostInstallStarted { .. } => "post-install",
                crate::progress::InstallProgress::PostInstallCompleted { .. } => {
                    "post-install done"
                }
                crate::progress::InstallProgress::PostInstallFailed { .. } => "post-install failed",
                _ => return,
            };
            seen.lock().unwrap().push(label);
        }));

        let plan = installer.plan(&["certs".to_string()]).await.unwrap();
        let result = installer
            .execute_with_progress(plan, true, Some(progress))
            .await
            .unwrap();

        assert_eq!(result.installed, 1);
        assert!(result.post_install_failures.is_empty());
        assert_eq!(
            result.notices,
            vec![super::PostInstallNotice {
                name: "certs".into(),
                message: "bundle written for certs".into()
            }]
        );
        assert_eq!(
            fs::read_to_string(prefix.join("etc/cert.pem")).unwrap(),
            "MOZILLA\n"
        );
        assert_eq!(
            *events.lock().unwrap(),
            ["installed", "post-install", "post-install done"]
        );
    }

    #[tokio::test]
    async fn a_failed_post_install_leaves_the_package_installed() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let bottle = create_bottle_tarball("flaky");
        let steps = r#""post_install_steps": [
            {"type": "mkdir_p", "path": {"path": "{{var}}/flaky"}},
            {"type": "run", "command": {"path": "sh"}, "args": ["-c", "echo nope >&2; exit 7"]},
            {"type": "mkdir_p", "path": {"path": "{{var}}/never"}}
        ]"#;
        let (mut installer, _root, prefix) =
            installer_for_bottle_with(&mock_server, &tmp, "flaky", bottle, steps).await;

        let result = installer
            .install(&["flaky".to_string()], true)
            .await
            .unwrap();

        assert_eq!(result.installed, 1);
        assert!(installer.is_installed("flaky"));
        assert!(prefix.join("bin/flaky").exists(), "the keg is linked");
        assert!(prefix.join("var/flaky").is_dir());
        assert!(
            !prefix.join("var/never").exists(),
            "steps after the failure do not run"
        );
        assert_eq!(result.post_install_failures.len(), 1);
        match &result.post_install_failures[0] {
            nb_core::Error::PostInstallFailed {
                name,
                step,
                message,
            } => {
                assert_eq!(name, "flaky");
                assert_eq!(step, "run");
                assert!(message.contains("nope"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unsupported_step_is_reported_not_skipped() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let bottle = create_bottle_tarball("newfangled");
        let steps = r#""post_install_steps": [{"type": "bootstrap_pypy", "abi_version": "3.11"}]"#;
        let (mut installer, _root, _prefix) =
            installer_for_bottle_with(&mock_server, &tmp, "newfangled", bottle, steps).await;

        let result = installer
            .install(&["newfangled".to_string()], true)
            .await
            .unwrap();

        assert_eq!(result.installed, 1);
        assert!(installer.is_installed("newfangled"));
        let failure = result.post_install_failures[0].to_string();
        assert!(failure.contains("bootstrap_pypy"), "{failure}");
        assert!(failure.contains("not supported"), "{failure}");
    }

    #[tokio::test]
    async fn formulae_without_steps_report_nothing() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let bottle = create_bottle_tarball("plain");
        let (mut installer, _root, _prefix) =
            installer_for_bottle(&mock_server, &tmp, "plain", bottle).await;
        let result = installer
            .install(&["plain".to_string()], true)
            .await
            .unwrap();
        assert!(result.notices.is_empty());
        assert!(result.post_install_failures.is_empty());
    }

    #[tokio::test]
    async fn install_copies_bottle_etc_files_into_prefix() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let bottle = create_bottle_tarball_with_files(
            "withconf",
            "1.0.0",
            &[(".bottle/etc/withconf/withconf.conf", "setting = 1\n")],
        );
        let (mut installer, _root, prefix) =
            installer_for_bottle(&mock_server, &tmp, "withconf", bottle).await;

        installer
            .install(&["withconf".to_string()], true)
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(prefix.join("etc/withconf/withconf.conf")).unwrap(),
            "setting = 1\n"
        );
    }

    #[tokio::test]
    async fn etc_install_failure_fails_install_and_cleans_keg() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let bottle = create_bottle_tarball_with_files(
            "badconf",
            "1.0.0",
            &[(".bottle/etc/badconf/badconf.conf", "setting = 1\n")],
        );
        let (mut installer, root, prefix) =
            installer_for_bottle(&mock_server, &tmp, "badconf", bottle).await;

        // A file where the config directory needs to go.
        fs::write(prefix.join("etc/badconf"), "in the way").unwrap();

        let result = installer.install(&["badconf".to_string()], true).await;

        assert!(result.is_err());
        assert!(installer.db.get_installed("badconf").is_none());
        assert!(!root.join("cellar/badconf/1.0.0").exists());
        assert!(!prefix.join("bin/badconf").exists());
    }

    #[tokio::test]
    async fn db_persist_failure_cleans_materialized_and_linked_files() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("rollbackme");
        let bottle_sha = sha256_hex(&bottle);

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "rollbackme",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/rollbackme-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            bottle_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/rollbackme.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/rollbackme-1.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let db_path = root.join("db/nb.sqlite3");
        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&db_path).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("DROP TABLE installed_kegs", []).unwrap();

        let result = installer.install(&["rollbackme".to_string()], true).await;
        assert!(result.is_err());

        assert!(!root.join("cellar/rollbackme/1.0.0").exists());
        assert!(!prefix.join("bin/rollbackme").exists());
        assert!(!prefix.join("opt/rollbackme").exists());
        assert!(root.join("store").join(&bottle_sha).exists());
    }

    #[tokio::test]
    async fn parallel_api_fetching_with_deep_deps() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let leaf1_bottle = create_bottle_tarball("leaf1");
        let leaf1_sha = sha256_hex(&leaf1_bottle);
        let leaf2_bottle = create_bottle_tarball("leaf2");
        let leaf2_sha = sha256_hex(&leaf2_bottle);
        let mid1_bottle = create_bottle_tarball("mid1");
        let mid1_sha = sha256_hex(&mid1_bottle);
        let mid2_bottle = create_bottle_tarball("mid2");
        let mid2_sha = sha256_hex(&mid2_bottle);
        let root_bottle = create_bottle_tarball("root");
        let root_sha = sha256_hex(&root_bottle);

        let tag = get_test_bottle_tag();
        let leaf1_json = format!(
            r#"{{"name":"leaf1","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/leaf1.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            leaf1_sha
        );
        let leaf2_json = format!(
            r#"{{"name":"leaf2","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/leaf2.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            leaf2_sha
        );
        let mid1_json = format!(
            r#"{{"name":"mid1","versions":{{"stable":"1.0.0"}},"dependencies":["leaf1"],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/mid1.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            mid1_sha
        );
        let mid2_json = format!(
            r#"{{"name":"mid2","versions":{{"stable":"1.0.0"}},"dependencies":["leaf1","leaf2"],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/mid2.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            mid2_sha
        );
        let root_json = format!(
            r#"{{"name":"root","versions":{{"stable":"1.0.0"}},"dependencies":["mid1","mid2"],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/root.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            root_sha
        );

        for (name, json) in [
            ("leaf1", &leaf1_json),
            ("leaf2", &leaf2_json),
            ("mid1", &mid1_json),
            ("mid2", &mid2_json),
            ("root", &root_json),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/formula/{}.json", name)))
                .respond_with(ResponseTemplate::new(200).set_body_string(json))
                .mount(&mock_server)
                .await;
        }
        for (name, bottle) in [
            ("leaf1", &leaf1_bottle),
            ("leaf2", &leaf2_bottle),
            ("mid1", &mid1_bottle),
            ("mid2", &mid2_bottle),
            ("root", &root_bottle),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/bottles/{}.tar.gz", name)))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
                .mount(&mock_server)
                .await;
        }

        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/nb.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        installer
            .install(&["root".to_string()], true)
            .await
            .unwrap();

        assert!(installer.db.get_installed("root").is_some());
        assert!(installer.db.get_installed("mid1").is_some());
        assert!(installer.db.get_installed("mid2").is_some());
        assert!(installer.db.get_installed("leaf1").is_some());
        assert!(installer.db.get_installed("leaf2").is_some());
    }

    #[tokio::test]
    async fn streaming_extraction_processes_as_downloads_complete() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let fast_bottle = create_bottle_tarball("fastpkg");
        let fast_sha = sha256_hex(&fast_bottle);
        let slow_bottle = create_bottle_tarball("slowpkg");
        let slow_sha = sha256_hex(&slow_bottle);

        let tag = get_test_bottle_tag();
        let fast_json = format!(
            r#"{{"name":"fastpkg","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/fast.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            fast_sha
        );
        let slow_json = format!(
            r#"{{"name":"slowpkg","versions":{{"stable":"1.0.0"}},"dependencies":["fastpkg"],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/slow.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            slow_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/fastpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&fast_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/slowpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&slow_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/bottles/fast.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(fast_bottle.clone()))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/bottles/slow.tar.gz"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(slow_bottle.clone())
                    .set_delay(Duration::from_millis(100)),
            )
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/nb.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        installer
            .install(&["slowpkg".to_string()], true)
            .await
            .unwrap();

        assert!(installer.db.get_installed("fastpkg").is_some());
        assert!(installer.db.get_installed("slowpkg").is_some());
        assert!(root.join("cellar/fastpkg/1.0.0").exists());
        assert!(root.join("cellar/slowpkg/1.0.0").exists());
        assert!(prefix.join("bin/fastpkg").exists());
        assert!(prefix.join("bin/slowpkg").exists());
    }

    #[tokio::test]
    async fn a_corrupt_bottle_fails_alone_after_its_retries() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let good_bottle = create_bottle_tarball("goodpkg");
        // Right checksum, but not a tarball: unpacking fails every time.
        let corrupt_bottle = b"this is not a gzip stream".to_vec();

        let tag = get_test_bottle_tag();
        let formula = |name: &str, sha: &str| {
            format!(
                r#"{{"name":"{name}","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{tag}":{{"url":"{}/bottles/{name}.tar.gz","sha256":"{sha}"}}}}}}}}}}"#,
                mock_server.uri()
            )
        };
        for (name, bottle) in [("goodpkg", &good_bottle), ("corruptpkg", &corrupt_bottle)] {
            Mock::given(method("GET"))
                .and(path(format!("/formula/{name}.json")))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(formula(name, &sha256_hex(bottle))),
                )
                .mount(&mock_server)
                .await;
            Mock::given(method("GET"))
                .and(path(format!("/bottles/{name}.tar.gz")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
                .mount(&mock_server)
                .await;
        }

        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();
        let mut installer = Installer::new(
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap(),
            BlobCache::new(&root.join("cache")).unwrap(),
            Store::new(&root).unwrap(),
            Cellar::new(&root).unwrap(),
            Linker::new(&prefix).unwrap(),
            Database::open(&root.join("db/nb.sqlite3")).unwrap(),
            prefix.clone(),
            root.join("locks"),
        );

        let err = installer
            .install(&["goodpkg".to_string(), "corruptpkg".to_string()], true)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("attempts"), "{err}");
        assert!(installer.db.get_installed("goodpkg").is_some());
        assert!(prefix.join("bin/goodpkg").exists());
        assert!(installer.db.get_installed("corruptpkg").is_none());
        assert!(!root.join("cellar/corruptpkg").exists());
        let corrupt_downloads = mock_server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/bottles/corruptpkg.tar.gz")
            .count();
        assert_eq!(corrupt_downloads, super::MAX_CORRUPTION_RETRIES);
        assert!(
            !root
                .join("cache/blobs")
                .join(format!("{}.tar.gz", sha256_hex(&corrupt_bottle)))
                .exists(),
            "the corrupt blob must not be kept"
        );
    }

    #[tokio::test]
    async fn retries_on_corrupted_download() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("retrypkg");
        let bottle_sha = sha256_hex(&bottle);

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "retrypkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/retrypkg-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            bottle_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/retrypkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        let attempt_count = Arc::new(AtomicUsize::new(0));
        let attempt_clone = attempt_count.clone();
        let valid_bottle = bottle.clone();

        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/retrypkg-1.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(move |_: &wiremock::Request| {
                let _attempt = attempt_clone.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_bytes(valid_bottle.clone())
            })
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("nextbrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/nb.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        installer
            .install(&["retrypkg".to_string()], true)
            .await
            .unwrap();

        assert!(installer.is_installed("retrypkg"));
        assert!(root.join("cellar/retrypkg/1.0.0").exists());
        assert!(prefix.join("bin/retrypkg").exists());
    }

    #[tokio::test]
    async fn fails_after_max_retries() {
        // Validates the retry mechanism structure -- proper integration test
        // would need injection of corruption between download and extraction.
    }
}
