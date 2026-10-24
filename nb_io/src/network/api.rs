use std::borrow::Cow;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::checksum::verify_sha256_bytes;
use crate::network::cache::{ApiCache, CacheEntry, IndexEntry, IndexMeta};
use crate::network::suggest::{did_you_mean, rank_formula_suggestions};
use nb_core::{Error, Formula};
use serde_json::value::RawValue;
use tokio::sync::OnceCell;
use tracing::{debug, warn};

/// How long the formula index is trusted before it is revalidated, unless
/// `NEXTBREW_API_AUTO_UPDATE_SECS` says otherwise. Matches the API's own
/// `max-age`.
const INDEX_MAX_AGE: Duration = Duration::from_secs(600);

/// The names in one entry of the bulk API file.
#[derive(serde::Deserialize)]
struct IndexNames<'a> {
    #[serde(borrow, default)]
    name: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    aliases: Vec<Cow<'a, str>>,
    #[serde(borrow, default)]
    oldnames: Vec<Cow<'a, str>>,
}

/// Split the bulk API file into index entries without building a tree for
/// its 30 MB: each formula's JSON is kept verbatim. Old names in
/// `not_renames` are left out of the aliases.
fn index_entries<'a>(body: &'a str, not_renames: &[String]) -> Result<Vec<IndexEntry<'a>>, Error> {
    let raws: Vec<&RawValue> =
        serde_json::from_str(body).map_err(Error::network("failed to parse bulk formula JSON"))?;
    Ok(raws
        .into_iter()
        .filter_map(|raw| {
            let names: IndexNames = serde_json::from_str(raw.get()).ok()?;
            let name = names.name?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            let renames = names
                .oldnames
                .iter()
                .filter(|old| !not_renames.iter().any(|not| not == old.trim()));
            let aliases = names
                .aliases
                .iter()
                .chain(renames)
                .map(|alias| alias.trim().to_string())
                .filter(|alias| !alias.is_empty() && *alias != name)
                .collect();
            Some(IndexEntry {
                name,
                body: raw.get(),
                aliases,
            })
        })
        .collect())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn index_max_age() -> Duration {
    std::env::var("NEXTBREW_API_AUTO_UPDATE_SECS")
        .ok()
        .and_then(|secs| secs.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(INDEX_MAX_AGE)
}

pub(crate) const HOMEBREW_CORE_RAW_BASE: &str =
    "https://raw.githubusercontent.com/Homebrew/homebrew-core/main";

/// homebrew-core's files at `git_head` (the API's `tap_git_head`, the
/// commit the API was made from), as Homebrew fetches a formula's source;
/// at `main` without one.
pub(crate) fn homebrew_core_raw_base(git_head: Option<&str>) -> String {
    match git_head.filter(|head| !head.is_empty()) {
        Some(head) => format!("https://raw.githubusercontent.com/Homebrew/homebrew-core/{head}"),
        None => HOMEBREW_CORE_RAW_BASE.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RubySourceLocator<'a> {
    CoreRelativePath(&'a str),
    AbsoluteUrl(&'a str),
}

impl<'a> RubySourceLocator<'a> {
    fn parse(input: &'a str) -> Self {
        if input.starts_with("https://") || input.starts_with("http://") {
            return Self::AbsoluteUrl(input);
        }

        Self::CoreRelativePath(input)
    }

    fn source_id(self, original: &'a str) -> &'a str {
        match self {
            Self::CoreRelativePath(_) => original,
            Self::AbsoluteUrl(url) => url,
        }
    }

    fn to_url(self, git_head: Option<&str>) -> String {
        match self {
            Self::CoreRelativePath(path) => {
                format!("{}/{path}", homebrew_core_raw_base(git_head))
            }
            Self::AbsoluteUrl(url) => url.to_string(),
        }
    }
}

/// `brew search`'s matching: `/regex/`, or a substring of the name once
/// both are lowercased and stripped to letters, digits, `@` and `+`.
type Matcher = Box<dyn Fn(&str) -> bool>;

fn is_regex_query(query: &str) -> bool {
    query.len() > 1 && query.starts_with('/') && query.ends_with('/')
}

fn search_matcher(query: &str) -> Result<Matcher, Error> {
    if let Some(pattern) = query.strip_prefix('/').and_then(|q| q.strip_suffix('/')) {
        let regex = regex::Regex::new(pattern).map_err(|_| Error::InvalidArgument {
            message: format!("{query} is not a valid regex."),
        })?;
        return Ok(Box::new(move |name| regex.is_match(name)));
    }
    fn simplify(s: &str) -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '@' || *c == '+')
            .collect::<String>()
            .to_ascii_lowercase()
    }
    let query = simplify(query);
    Ok(Box::new(move |name| simplify(name).contains(&query)))
}

enum CachedGetResult {
    Cached(String),
    Fresh(reqwest::Response),
}

#[derive(Debug)]
pub struct ApiClient {
    base_url: String,
    cask_base_url: String,
    client: reqwest::Client,
    cache: Option<ApiCache>,
    /// Whether the index in `cache` is usable this run, decided by the first
    /// caller that needs it; see [`ApiClient::ensure_index`].
    index_usable: OnceCell<bool>,
    formula_candidates: RwLock<Option<Arc<[String]>>>,
    /// [`ApiClient::formula_names`] from the index, read once per run.
    formula_names: RwLock<Option<Arc<[String]>>>,
    /// Third-party taps, read for formulae the API does not have.
    taps: Option<Arc<TapSource>>,
}

/// Where tap formulae come from: the installed taps, which of them may be
/// loaded, and which installed formulae came from a tap.
#[derive(Debug)]
pub struct TapSource {
    pub taps: crate::tap::Taps,
    pub trust: crate::tap::trust::TrustStore,
    pub platform: crate::tap::ruby::Platform,
    /// Installed formulae from taps, by name.
    pub installed: std::sync::RwLock<std::collections::HashMap<String, crate::tap::TapName>>,
}

impl TapSource {
    pub fn new(prefix: &std::path::Path) -> Self {
        Self {
            taps: crate::tap::Taps::new(prefix),
            trust: crate::tap::trust::TrustStore::new(),
            platform: crate::tap::ruby::Platform::current(),
            installed: Default::default(),
        }
    }

    /// The formula `name` of `tap`, read from its file.
    pub fn formula(&self, tap: &crate::tap::TapName, name: &str) -> Result<Formula, Error> {
        let json = self.formula_json(tap, name)?;
        let resolved = json["name"].as_str().unwrap_or(name).to_string();
        serde_json::from_value(json).map_err(|e| Error::UnsupportedFormula {
            name: format!("{tap}/{resolved}"),
            reason: e.to_string(),
        })
    }

    /// [`Self::formula`] as the formula API's JSON for it.
    pub fn formula_json(
        &self,
        tap: &crate::tap::TapName,
        name: &str,
    ) -> Result<serde_json::Value, Error> {
        let full = format!("{tap}/{name}");
        if !self.taps.is_installed(tap) {
            return Err(Error::InvalidArgument {
                message: format!(
                    "No available formula with the name \"{full}\".\nThis command requires the tap {tap}.\nIf you trust this tap, tap it explicitly and then try again:\n  nb tap {tap}"
                ),
            });
        }
        let Some((resolved, file)) = self.taps.find_formula(tap, name) else {
            return Err(Error::MissingFormula { name: full });
        };
        let remote = self.taps.remote(tap);
        self.trust.require(
            crate::tap::trust::TrustKind::Formula,
            tap,
            &resolved,
            remote.as_deref(),
        )?;
        let source = std::fs::read(&file).map_err(Error::file("failed to read the formula"))?;
        let read = crate::tap::ruby::read_formula(&source, &resolved, tap, self.platform).map_err(
            |e| Error::UnsupportedFormula {
                name: format!("{tap}/{resolved}"),
                reason: format!("{} (line {})", e.message, e.line),
            },
        )?;
        Ok(read.json)
    }

    /// The cask `token` of `tap`, read from its file, as the cask API's
    /// JSON for it.
    pub fn cask(&self, tap: &crate::tap::TapName, token: &str) -> Result<serde_json::Value, Error> {
        self.cask_staged(tap, token, None)
    }

    /// [`Self::cask`] read again once its download is staged in `staged`,
    /// as Homebrew reads a cask again before it installs its artifacts.
    pub fn cask_staged(
        &self,
        tap: &crate::tap::TapName,
        token: &str,
        staged: Option<&std::path::Path>,
    ) -> Result<serde_json::Value, Error> {
        let full = format!("{tap}/{token}");
        if !self.taps.is_installed(tap) {
            return Err(Error::InvalidArgument {
                message: format!(
                    "No available cask with the name \"{full}\".\nThis command requires the tap {tap}.\nIf you trust this tap, tap it explicitly and then try again:\n  nb tap {tap}"
                ),
            });
        }
        let Some((resolved, file)) = self.taps.find_cask(tap, token) else {
            return Err(Error::MissingFormula {
                name: format!("cask:{full}"),
            });
        };
        let remote = self.taps.remote(tap);
        self.trust.require(
            crate::tap::trust::TrustKind::Cask,
            tap,
            &resolved,
            remote.as_deref(),
        )?;
        let source = std::fs::read(&file).map_err(Error::file("failed to read the cask"))?;
        let read = crate::tap::ruby::read_cask(&source, &resolved, tap, self.platform, staged)
            .map_err(|e| Error::InvalidArgument {
                message: format!(
                    "cask '{tap}/{resolved}' is not supported: {} (line {})",
                    e.message, e.line
                ),
            })?;
        read.warn_deprecations(tap, &file);
        Ok(read.json)
    }

    /// The installed taps with a cask `token`.
    pub fn taps_with_cask(&self, token: &str) -> Vec<crate::tap::TapName> {
        self.taps
            .installed()
            .into_iter()
            .filter(|tap| !tap.is_core() && self.taps.find_cask(tap, token).is_some())
            .collect()
    }

    /// The installed taps with a formula `name`, as `FromNameLoader`
    /// searches them for a name homebrew/core does not have.
    pub fn taps_with(&self, name: &str) -> Vec<crate::tap::TapName> {
        self.taps
            .installed()
            .into_iter()
            .filter(|tap| !tap.is_core() && self.taps.find_formula(tap, name).is_some())
            .collect()
    }
}

impl ApiClient {
    const DEFAULT_BASE_URL: &'static str = "https://formulae.brew.sh/api/formula";

    pub fn new() -> Self {
        Self::build_client(Self::DEFAULT_BASE_URL.to_string())
    }

    /// Rejects non-http(s) schemes and URLs containing credentials.
    pub fn with_base_url(base_url: String) -> Result<Self, Error> {
        let parsed = reqwest::Url::parse(&base_url).map_err(|e| Error::InvalidArgument {
            message: format!("invalid API base URL: {e}"),
        })?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(Error::InvalidArgument {
                message: format!(
                    "API base URL must use http or https scheme, got: {}",
                    parsed.scheme()
                ),
            });
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(Error::InvalidArgument {
                message: "Bad NEXTBREW_API_URL configuration".to_string(),
            });
        }

        Ok(Self::build_client(base_url))
    }

    fn build_client(base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("nextbrew/0.1")
            .pool_max_idle_per_host(20)
            .use_preconfigured_tls((*crate::network::tls::shared_tls_config()).clone())
            .build()
            .expect("failed to build HTTP client");

        Self {
            base_url,
            cask_base_url: "https://formulae.brew.sh/api/cask".to_string(),
            client,
            cache: None,
            index_usable: OnceCell::new(),
            formula_candidates: RwLock::new(None),
            formula_names: RwLock::new(None),
            taps: None,
        }
    }

    pub fn with_taps(mut self, taps: TapSource) -> Self {
        self.taps = Some(Arc::new(taps));
        self
    }

    pub fn taps(&self) -> Option<&TapSource> {
        self.taps.as_deref()
    }

    /// The HTTP client, for fetching taps.
    pub fn http(&self) -> &reqwest::Client {
        &self.client
    }

    #[cfg(test)]
    #[cfg(test)]
    pub fn with_cask_base_url(mut self, cask_base_url: String) -> Self {
        self.cask_base_url = cask_base_url;
        self
    }

    pub fn with_cache(mut self, cache: ApiCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Clear all cached API responses. Returns the number removed.
    pub fn clear_cache(&self) -> Result<usize, Error> {
        match &self.cache {
            Some(cache) => cache
                .clear()
                .map_err(Error::store("failed to clear API cache")),
            None => Ok(0),
        }
    }

    pub async fn fetch_formula_rb(
        &self,
        ruby_source_path: &str,
        git_head: Option<&str>,
        cache_dir: &std::path::Path,
        expected_sha256: Option<&str>,
    ) -> Result<std::path::PathBuf, Error> {
        let locator = RubySourceLocator::parse(ruby_source_path);
        let source_id = locator.source_id(ruby_source_path);
        let url = locator.to_url(git_head);

        self.fetch_formula_rb_from_url(source_id, &url, cache_dir, expected_sha256)
            .await
    }

    async fn fetch_formula_rb_from_url(
        &self,
        ruby_source_path: &str,
        url: &str,
        cache_dir: &std::path::Path,
        expected_sha256: Option<&str>,
    ) -> Result<std::path::PathBuf, Error> {
        let cache_key = format!("rb:{url}");
        // A copy that no longer matches (the file moved on since) is
        // fetched again.
        if let Some(entry) = self
            .cache
            .as_ref()
            .and_then(|c| c.get(&cache_key))
            .filter(|entry| verify_sha256_bytes(entry.body.as_bytes(), expected_sha256).is_ok())
        {
            let dest = cache_dir.join(ruby_source_path.replace('/', "_"));
            std::fs::create_dir_all(cache_dir)
                .map_err(Error::file("failed to create rb cache dir"))?;
            std::fs::write(&dest, entry.body.as_bytes())
                .map_err(Error::file("failed to write cached rb file"))?;
            return Ok(dest);
        }

        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(Error::network("failed to fetch formula rb"))?;

        if !response.status().is_success() {
            return Err(Error::NetworkFailure {
                message: format!("formula rb fetch returned HTTP {}", response.status()),
            });
        }

        let body = response
            .text()
            .await
            .map_err(Error::network("failed to read formula rb response"))?;

        verify_sha256_bytes(body.as_bytes(), expected_sha256)
            .map_err(|e| Self::map_formula_rb_checksum_error(e, ruby_source_path, "network"))?;

        if let Some(ref cache) = self.cache {
            let entry = CacheEntry {
                etag: None,
                last_modified: None,
                body: body.clone(),
            };
            let _ = cache.put(&cache_key, &entry);
        }

        let dest = cache_dir.join(ruby_source_path.replace('/', "_"));
        std::fs::create_dir_all(cache_dir).map_err(Error::file("failed to create rb cache dir"))?;
        std::fs::write(&dest, body.as_bytes()).map_err(Error::file("failed to write rb file"))?;

        Ok(dest)
    }

    fn map_formula_rb_checksum_error(err: Error, ruby_source_path: &str, source: &str) -> Error {
        match err {
            Error::ChecksumMismatch { .. } => err,
            Error::InvalidArgument { message } => Error::InvalidArgument {
                message: format!(
                    "invalid ruby_source_checksum for '{ruby_source_path}' (source: {source}): {message}"
                ),
            },
            other => other,
        }
    }

    async fn cached_get(&self, url: &str) -> Result<CachedGetResult, Error> {
        let cached_entry = self.cache.as_ref().and_then(|c| c.get(url));

        let mut request = self.client.get(url);

        if let Some(ref entry) = cached_entry {
            if let Some(ref etag) = entry.etag {
                request = request.header("If-None-Match", etag.as_str());
            }
            if let Some(ref last_modified) = entry.last_modified {
                request = request.header("If-Modified-Since", last_modified.as_str());
            }
        }

        let response = request.send().await.map_err(|e| Error::NetworkFailure {
            message: e.to_string(),
        })?;

        if response.status() == reqwest::StatusCode::NOT_MODIFIED
            && let Some(entry) = cached_entry
        {
            return Ok(CachedGetResult::Cached(entry.body));
        }

        Ok(CachedGetResult::Fresh(response))
    }

    fn store_response_in_cache(
        &self,
        url: &str,
        etag: Option<String>,
        last_modified: Option<String>,
        body: &str,
    ) {
        if let Some(ref cache) = self.cache {
            let entry = CacheEntry {
                etag,
                last_modified,
                body: body.to_string(),
            };
            let _ = cache.put(url, &entry);
        }
    }

    /// Look a formula up in the index (an alias or old name first, as
    /// `Formulary.tap_formula_name_type` does), then on the API. A name the
    /// index does not have may be newer than the index, so it is still
    /// asked for; a 404 there is final.
    pub async fn get_formula(&self, name: &str) -> Result<Formula, Error> {
        // Formulae of other taps are read from their Ruby files.
        let name = name.strip_prefix("homebrew/core/").unwrap_or(name);
        if let Some((tap, short)) = crate::tap::TapName::split_full_name(name) {
            let Some(taps) = &self.taps else {
                return Err(Error::UnsupportedTap {
                    name: name.to_string(),
                });
            };
            // Migrated to another tap: read from there, as brew does.
            return match taps.taps.formula_migration(&tap, &short) {
                Some((to, moved))
                    if to.is_official() && matches!(to.repo.as_str(), "core" | "homebrew") =>
                {
                    self.get_core_formula(&moved).await
                }
                Some((to, moved)) if to.is_core() => Err(Error::MissingFormula {
                    name: format!("{to}/{moved}"),
                }),
                // Homebrew follows it only into a tap that is there.
                Some((to, _)) if !taps.taps.is_installed(&to) => Err(Error::MissingFormula {
                    name: format!("{tap}/{short}"),
                }),
                Some((to, moved)) => taps.formula(&to, &moved),
                None => taps.formula(&tap, &short),
            };
        }
        // An installed formula from a tap is that one, not core's.
        if let Some(taps) = &self.taps {
            let installed = taps
                .installed
                .read()
                .ok()
                .and_then(|map| map.get(name).cloned());
            if let Some(tap) = installed {
                return taps.formula(&tap, name);
            }
        }
        match self.get_core_formula(name).await {
            Err(Error::MissingFormula { .. }) if self.taps.is_some() => {
                let taps = self.taps.as_ref().unwrap();
                match taps.taps_with(name).as_slice() {
                    [] => Err(Error::MissingFormula {
                        name: name.to_string(),
                    }),
                    [tap] => taps.formula(tap, name),
                    many => Err(Error::InvalidArgument {
                        message: format!(
                            "Formulae found in multiple taps:\n{}\n\nPlease use the fully-qualified name (e.g. {}/{name}) to refer to a specific formula.",
                            many.iter()
                                .map(|tap| format!("       * {tap}/{name}"))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            many[0]
                        ),
                    }),
                }
            }
            other => other,
        }
    }

    async fn get_core_formula(&self, name: &str) -> Result<Formula, Error> {
        let body = self.get_core_formula_body(name).await?;
        serde_json::from_str(&body).map_err(Error::network("failed to parse formula JSON"))
    }

    /// The API's JSON of homebrew/core's formula `name` (or of the one it
    /// is an alias or old name of), as text.
    async fn get_core_formula_body(&self, name: &str) -> Result<String, Error> {
        let mut lookup = name.to_string();
        if self.ensure_index().await
            && let Some(cache) = &self.cache
        {
            if let Some(canonical) = cache.index_alias(name) {
                lookup = canonical;
            }
            if let Some(body) = cache.index_formula(&lookup) {
                return Ok(body);
            }
        }

        match self.fetch_formula_json(&lookup).await {
            Ok(body) => Ok(body),
            Err(Error::MissingFormula { .. }) => Err(Error::MissingFormula {
                name: name.to_string(),
            }),
            Err(e) => Err(e),
        }
    }

    /// [`Self::get_formula`]'s formula as its JSON: the API's for
    /// homebrew/core's, read from its file like it for a tap's.
    pub async fn get_formula_json(&self, name: &str) -> Result<serde_json::Value, Error> {
        let formula = self.get_formula(name).await?;
        if let Some(tap) = formula.third_party_tap() {
            let tap = crate::tap::TapName::parse(tap)?;
            let taps = self.taps.as_ref().ok_or_else(|| Error::UnsupportedTap {
                name: format!("{tap}/{}", formula.name),
            })?;
            return taps.formula_json(&tap, &formula.name);
        }
        let body = self.get_core_formula_body(&formula.name).await?;
        serde_json::from_str(&body).map_err(Error::network("failed to parse formula JSON"))
    }

    async fn fetch_formula_json(&self, name: &str) -> Result<String, Error> {
        let url = format!("{}/{}.json", self.base_url, name);
        self.cached_body(&url)
            .await?
            .ok_or_else(|| Error::MissingFormula {
                name: name.to_string(),
            })
    }

    /// The body at `url` through the response cache, or `None` on a 404.
    async fn cached_body(&self, url: &str) -> Result<Option<String>, Error> {
        match self.cached_get(url).await? {
            CachedGetResult::Cached(body) => Ok(Some(body)),
            CachedGetResult::Fresh(response) => {
                if response.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(None);
                }
                if !response.status().is_success() {
                    return Err(Error::NetworkFailure {
                        message: format!("HTTP {}", response.status()),
                    });
                }

                let header = |name: &str| {
                    response
                        .headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string())
                };
                let (etag, last_modified) = (header("etag"), header("last-modified"));

                let body = response
                    .text()
                    .await
                    .map_err(Error::network("failed to read response body"))?;

                self.store_response_in_cache(url, etag, last_modified, &body);
                Ok(Some(body))
            }
        }
    }

    /// Formula names and aliases matching `query`, as `brew search` lists
    /// them: an alias only when its formula's own name did not match,
    /// unversioned formulae before versioned ones, and for a plain query
    /// then the names [`did_you_mean`] finds.
    pub async fn search_formulas(&self, query: &str) -> Result<Vec<String>, Error> {
        let matches = search_matcher(query)?;
        let candidates = self.formula_candidates().await?;
        let found: Vec<&String> = candidates.iter().filter(|name| matches(name)).collect();
        // The index keeps old names with the aliases; brew searches only the latter.
        let alias_of = |name: &str| {
            let cache = self.cache.as_ref()?;
            let formula = cache.index_alias(name)?;
            let body: serde_json::Value =
                serde_json::from_str(&cache.index_formula(&formula)?).ok()?;
            let old = body["oldnames"]
                .as_array()
                .is_some_and(|old| old.iter().any(|old| old == name));
            Some((formula, old))
        };
        let mut results: Vec<String> = found
            .iter()
            .filter(|name| !matches!(alias_of(name), Some((_, true))))
            .map(|name| name.to_string())
            .collect();
        results.sort_by_cached_key(|name| {
            let (base, version) = name.split_once('@').unwrap_or((name, ""));
            let parts: Vec<u64> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
            (base.to_string(), parts)
        });
        results.dedup();
        if !is_regex_query(query) {
            for name in did_you_mean(query, &self.formula_names().await?) {
                if !results.contains(&name) {
                    results.push(name);
                }
            }
        }
        // Only now, so a formula found by its spelling also hides its aliases.
        let shown = results.clone();
        results.retain(|name| match alias_of(name) {
            Some((formula, _)) => !shown.contains(&formula),
            None => true,
        });
        Ok(results)
    }

    /// Cask tokens matching `query`, with [`did_you_mean`]'s for a plain
    /// query, sorted, from the bulk cask API file.
    pub async fn search_casks(&self, query: &str) -> Result<Vec<String>, Error> {
        let matches = search_matcher(query)?;
        #[derive(serde::Deserialize)]
        struct Cask {
            token: String,
        }
        let casks: Vec<Cask> = serde_json::from_str(&self.cask_list_body().await?)
            .map_err(Error::network("failed to parse cask list"))?;
        let all: Vec<String> = casks.into_iter().map(|cask| cask.token).collect();
        let mut tokens: Vec<String> = all.iter().filter(|token| matches(token)).cloned().collect();
        if !is_regex_query(query) {
            tokens.extend(did_you_mean(query, &all));
        }
        tokens.sort();
        tokens.dedup();
        Ok(tokens)
    }

    /// Every cask in the bulk cask API file, as the API gives it.
    pub async fn all_casks(&self) -> Result<Vec<serde_json::Value>, Error> {
        serde_json::from_str(&self.cask_list_body().await?)
            .map_err(Error::network("failed to parse cask list"))
    }

    async fn cask_list_body(&self) -> Result<String, Error> {
        let url = format!("{}.json", self.cask_base_url);
        self.cached_body(&url).await?.ok_or(Error::NetworkFailure {
            message: format!("{url} not found"),
        })
    }

    /// Every formula in the API. One that does not parse (the `portable-*`
    /// formulae, which have no bottles) is left out.
    pub async fn all_formulas(&self) -> Result<Vec<Formula>, Error> {
        let parse = |body: &str| match serde_json::from_str(body) {
            Ok(formula) => Some(formula),
            Err(e) => {
                debug!(error = %e, "skipping a formula that does not parse");
                None
            }
        };
        if let Some(cache) = &self.cache
            && self.ensure_index().await
        {
            let bodies = cache
                .index_formulas()
                .map_err(Error::store("failed to read the formula index"))?;
            return Ok(bodies.iter().filter_map(|body| parse(body)).collect());
        }
        let body = self.bulk_formula_body().await?;
        Ok(index_entries(&body, &[])?
            .iter()
            .filter_map(|entry| parse(entry.body))
            .collect())
    }

    /// Every formula's name, without its aliases and old names.
    pub async fn formula_names(&self) -> Result<Vec<String>, Error> {
        if let Some(names) = self.formula_names.read().ok().and_then(|n| n.clone()) {
            return Ok(names.to_vec());
        }
        if let Some(cache) = &self.cache
            && self.ensure_index().await
        {
            let names: Arc<[String]> = cache
                .index_formula_names()
                .map_err(Error::store("failed to read the formula index"))?
                .into();
            if let Ok(mut cached) = self.formula_names.write() {
                *cached = Some(Arc::clone(&names));
            }
            return Ok(names.to_vec());
        }
        let body = self.bulk_formula_body().await?;
        Ok(index_entries(&body, &[])?
            .into_iter()
            .map(|entry| entry.name)
            .collect())
    }

    fn index_url(&self) -> String {
        format!("{}.json", self.base_url)
    }

    /// Whether the index can serve lookups this run. The first caller
    /// revalidates it when it is older than the max age; a failure there
    /// keeps whatever index exists, and without any index every lookup goes
    /// to the API as before.
    async fn ensure_index(&self) -> bool {
        *self
            .index_usable
            .get_or_init(|| async {
                let Some(cache) = &self.cache else {
                    return false;
                };
                match self.refresh_index(false).await {
                    Ok(()) => cache.index_meta().is_some(),
                    Err(e) => {
                        if cache.index_meta().is_some() {
                            warn!(error = %e, "could not refresh the formula index; using the cached one");
                            true
                        } else {
                            debug!(error = %e, "no formula index available; fetching formulas one by one");
                            false
                        }
                    }
                }
            })
            .await
    }

    /// Bring the index up to date: a conditional GET of the bulk API file
    /// when it is older than the max age, or always when `force` is set. A
    /// 304 only bumps the timestamp; a 200 replaces the whole index.
    pub async fn refresh_index(&self, force: bool) -> Result<(), Error> {
        let Some(cache) = &self.cache else {
            return Ok(());
        };
        let meta = cache.index_meta();
        if !force
            && let Some(meta) = &meta
            && unix_now().saturating_sub(meta.fetched_at) < index_max_age().as_secs() as i64
        {
            return Ok(());
        }

        let url = self.index_url();
        let mut request = self.client.get(&url);
        if let Some(meta) = &meta {
            if let Some(etag) = &meta.etag {
                request = request.header("If-None-Match", etag.as_str());
            }
            if let Some(last_modified) = &meta.last_modified {
                request = request.header("If-Modified-Since", last_modified.as_str());
            }
        }
        let response = request.send().await.map_err(|e| Error::NetworkFailure {
            message: e.to_string(),
        })?;

        if response.status() == reqwest::StatusCode::NOT_MODIFIED && meta.is_some() {
            cache
                .touch_index(unix_now())
                .map_err(Error::store("failed to update the formula index"))?;
            debug!("formula index is current");
            return Ok(());
        }
        if !response.status().is_success() {
            return Err(Error::NetworkFailure {
                message: format!("bulk formula fetch returned HTTP {}", response.status()),
            });
        }

        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        };
        let new_meta = IndexMeta {
            etag: header("etag"),
            last_modified: header("last-modified"),
            fetched_at: unix_now(),
        };
        let body = response
            .text()
            .await
            .map_err(Error::network("failed to read bulk formula response body"))?;
        let not_renames = self.casks_migrated_to_core().await;
        let count = cache
            .replace_index(index_entries(&body, &not_renames)?, &new_meta)
            .map_err(Error::store("failed to store the formula index"))?;
        debug!(count, "formula index refreshed");
        Ok(())
    }

    /// A file of the API next to the formula one (`…/api/<file>`).
    fn api_url(&self, file: &str) -> String {
        match self.base_url.rsplit_once('/') {
            Some((parent, _)) => format!("{parent}/{file}"),
            None => file.to_string(),
        }
    }

    /// The old cask tokens homebrew/cask migrated to homebrew/core. The
    /// formulae they went to list them among their `oldnames`, but they are
    /// not in homebrew/core's `formula_renames`, so brew does not resolve
    /// them to the formula (it says where they went instead). Empty when
    /// the list cannot be had.
    async fn casks_migrated_to_core(&self) -> Vec<String> {
        let Ok(Some(body)) = self
            .cached_body(&self.api_url("cask_tap_migrations.json"))
            .await
        else {
            return Vec::new();
        };
        serde_json::from_str::<std::collections::BTreeMap<String, String>>(&body)
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, to)| to == "homebrew/core" || to.starts_with("homebrew/core/"))
            .map(|(old, _)| old)
            .collect()
    }

    /// How many formulas the index holds.
    pub fn index_len(&self) -> usize {
        self.cache
            .as_ref()
            .and_then(|cache| cache.index_formula_count().ok())
            .unwrap_or(0)
    }

    pub async fn suggest_formulas(&self, query: &str, limit: usize) -> Result<Vec<String>, Error> {
        if limit == 0 || query.trim().is_empty() {
            return Ok(Vec::new());
        }

        if query.split('/').count() == 3 || query.starts_with("cask:") {
            return Ok(Vec::new());
        }

        let candidates = self.formula_candidates().await?;
        Ok(rank_formula_suggestions(query, &candidates, limit))
    }

    /// The bulk formula file, fetched afresh for want of an index.
    async fn bulk_formula_body(&self) -> Result<String, Error> {
        let response = self
            .client
            .get(self.index_url())
            .send()
            .await
            .map_err(|e| Error::NetworkFailure {
                message: e.to_string(),
            })?;
        if !response.status().is_success() {
            return Err(Error::NetworkFailure {
                message: format!("bulk formula fetch returned HTTP {}", response.status()),
            });
        }
        response
            .text()
            .await
            .map_err(Error::network("failed to read bulk formula response body"))
    }

    /// Every formula name, alias and old name: from the index, or straight
    /// from the bulk API file when there is no cache to index into.
    async fn formula_candidates(&self) -> Result<Arc<[String]>, Error> {
        if let Some(candidates) = self.formula_candidates.read().ok().and_then(|c| c.clone()) {
            return Ok(candidates);
        }

        let names: Vec<String> = match &self.cache {
            Some(cache) if self.ensure_index().await => cache
                .index_names()
                .map_err(Error::store("failed to read the formula index"))?,
            _ => index_entries(&self.bulk_formula_body().await?, &[])?
                .into_iter()
                .flat_map(|entry| std::iter::once(entry.name).chain(entry.aliases))
                .collect(),
        };
        let candidates: Arc<[String]> = names.into();
        if let Ok(mut cached) = self.formula_candidates.write() {
            *cached = Some(Arc::clone(&candidates));
        }
        Ok(candidates)
    }

    /// A cask's JSON: from the API, or read from a tap's file for a cask
    /// named in full (`user/repo/token`), installed from a tap, or that only
    /// a tap has.
    pub async fn get_cask(&self, token: &str) -> Result<serde_json::Value, Error> {
        let token = token.strip_prefix("homebrew/cask/").unwrap_or(token);
        if let Some((tap, short)) = crate::tap::TapName::split_full_name(token) {
            let Some(taps) = &self.taps else {
                return Err(Error::UnsupportedTap {
                    name: token.to_string(),
                });
            };
            return match taps.taps.cask_migration(&tap, &short) {
                Some((to, moved)) if to.is_official() && to.repo == "cask" => {
                    self.get_renamed_core_cask(&moved).await
                }
                Some((to, moved)) if to.is_core() => Err(Error::MissingFormula {
                    name: format!("cask:{to}/{moved}"),
                }),
                Some((to, _)) if !taps.taps.is_installed(&to) => Err(Error::MissingFormula {
                    name: format!("cask:{tap}/{short}"),
                }),
                Some((to, moved)) => taps.cask(&to, &moved),
                None => taps.cask(&tap, &short),
            };
        }
        if let Some(taps) = &self.taps {
            let installed = taps
                .installed
                .read()
                .ok()
                .and_then(|map| map.get(&format!("cask:{token}")).cloned());
            if let Some(tap) = installed {
                return taps.cask(&tap, token);
            }
        }
        match self.get_renamed_core_cask(token).await {
            Err(Error::MissingFormula { .. }) if self.taps.is_some() => {
                let taps = self.taps.as_ref().unwrap();
                match taps.taps_with_cask(token).as_slice() {
                    [] => Err(Error::MissingFormula {
                        name: format!("cask:{token}"),
                    }),
                    [tap] => taps.cask(tap, token),
                    many => Err(Error::InvalidArgument {
                        message: format!(
                            "Casks found in multiple taps:\n{}\n\nPlease use the fully-qualified name (e.g. {}/{token}) to refer to a specific cask.",
                            many.iter()
                                .map(|tap| format!("       * {tap}/{token}"))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            many[0]
                        ),
                    }),
                }
            }
            other => other,
        }
    }

    /// A homebrew/cask cask by its token, or else by an old token: one it
    /// was renamed from, or a formula's that moved to it (its `old_tokens`,
    /// from the bulk cask file), as `CaskLoader` follows `cask_renames` and
    /// tap migrations. No current token is also an old one, so trying the
    /// token first gives what brew's order (renames first) gives.
    async fn get_renamed_core_cask(&self, token: &str) -> Result<serde_json::Value, Error> {
        match self.get_core_cask(token).await {
            Err(Error::MissingFormula { name }) => match self.renamed_cask(token).await {
                Some(renamed) => self.get_core_cask(&renamed).await,
                None => Err(Error::MissingFormula { name }),
            },
            other => other,
        }
    }

    /// The cask whose `old_tokens` has `token`.
    async fn renamed_cask(&self, token: &str) -> Option<String> {
        #[derive(serde::Deserialize)]
        struct Tokens {
            token: String,
            #[serde(default)]
            old_tokens: Vec<String>,
        }
        let casks: Vec<Tokens> = serde_json::from_str(&self.cask_list_body().await.ok()?).ok()?;
        casks
            .into_iter()
            .find(|cask| cask.old_tokens.iter().any(|old| old == token))
            .map(|cask| cask.token)
    }

    async fn get_core_cask(&self, token: &str) -> Result<serde_json::Value, Error> {
        let url = format!("{}/{}.json", self.cask_base_url, token);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::NetworkFailure {
                message: e.to_string(),
            })?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::MissingFormula {
                name: format!("cask:{token}"),
            });
        }

        if !response.status().is_success() {
            return Err(Error::NetworkFailure {
                message: format!("HTTP {}", response.status()),
            });
        }

        response
            .json::<serde_json::Value>()
            .await
            .map_err(Error::network("failed to parse cask JSON"))
    }
}

impl Default for ApiClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn with_base_url_rejects_non_http_schemes() {
        let err = ApiClient::with_base_url("ftp://example.com/api".into()).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
        assert!(err.to_string().contains("http or https"));
    }

    #[test]
    fn with_base_url_rejects_embedded_credentials() {
        let err = ApiClient::with_base_url("https://user:pass@example.com/api".into()).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
    }

    #[test]
    fn with_base_url_rejects_garbage() {
        assert!(ApiClient::with_base_url("not a url".into()).is_err());
    }

    #[test]
    fn with_base_url_accepts_valid_https() {
        assert!(ApiClient::with_base_url("https://mirror.example.com/api/formula".into()).is_ok());
    }

    #[test]
    fn with_base_url_accepts_valid_http() {
        assert!(ApiClient::with_base_url("http://localhost:8080/api".into()).is_ok());
    }

    #[test]
    fn ruby_source_locator_parses_all_supported_kinds() {
        assert_eq!(
            RubySourceLocator::parse("Formula/f/foo.rb"),
            RubySourceLocator::CoreRelativePath("Formula/f/foo.rb")
        );
        assert_eq!(
            RubySourceLocator::parse("https://example.com/foo.rb"),
            RubySourceLocator::AbsoluteUrl("https://example.com/foo.rb")
        );
    }

    #[test]
    fn ruby_source_locator_resolves_urls_exhaustively() {
        assert_eq!(
            RubySourceLocator::CoreRelativePath("Formula/f/foo.rb").to_url(None),
            "https://raw.githubusercontent.com/Homebrew/homebrew-core/main/Formula/f/foo.rb"
        );
        assert_eq!(
            RubySourceLocator::AbsoluteUrl("https://example.com/foo.rb").to_url(None),
            "https://example.com/foo.rb"
        );
    }

    #[tokio::test]
    async fn fetches_formula_from_mock_server() {
        let mock_server = MockServer::start().await;

        let fixture = include_str!("../../../nb_core/fixtures/formula_foo.json");

        Mock::given(method("GET"))
            .and(path("/foo.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri()).unwrap();
        let formula = client.get_formula("foo").await.unwrap();

        assert_eq!(formula.name, "foo");
        assert_eq!(formula.versions.stable, "1.2.3");
    }

    #[tokio::test]
    async fn returns_missing_formula_on_404() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/nonexistent.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri()).unwrap();
        let err = client.get_formula("nonexistent").await.unwrap_err();

        assert!(matches!(
            err,
            Error::MissingFormula { name } if name == "nonexistent"
        ));
    }

    #[tokio::test]
    async fn formulae_of_other_taps_are_refused_by_name() {
        let mock_server = MockServer::start().await;
        let fixture = include_str!("../../../nb_core/fixtures/formula_foo.json");
        Mock::given(method("GET"))
            .and(path("/foo.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture))
            .mount(&mock_server)
            .await;
        let client = ApiClient::with_base_url(mock_server.uri()).unwrap();

        let err = client
            .get_formula("hashicorp/tap/terraform")
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::UnsupportedTap { ref name } if name == "hashicorp/tap/terraform")
        );
        assert!(err.to_string().contains("'hashicorp/tap' tap"), "{err}");
        let core = client.get_formula("homebrew/core/foo").await.unwrap();
        assert_eq!(core.name, "foo");
    }

    #[tokio::test]
    async fn fetch_formula_rb_supports_absolute_url_paths() {
        let mock_server = MockServer::start().await;
        let ruby_body = "class Foo < Formula\nend\n";

        Mock::given(method("GET"))
            .and(path("/custom/foo.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ruby_body))
            .mount(&mock_server)
            .await;

        let cache_dir = tempdir().unwrap();
        let client = ApiClient::new();

        let fetched = client
            .fetch_formula_rb(
                &format!("{}/custom/foo.rb", mock_server.uri()),
                None,
                cache_dir.path(),
                None,
            )
            .await
            .unwrap();

        assert!(fetched.exists());
    }

    #[tokio::test]
    async fn fetch_formula_rb_from_network_rejects_checksum_mismatch() {
        let mock_server = MockServer::start().await;
        let ruby_body = "class Foo < Formula\nend\n";

        Mock::given(method("GET"))
            .and(path("/Formula/f/foo.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ruby_body))
            .mount(&mock_server)
            .await;

        let cache_dir = tempdir().unwrap();
        let client = ApiClient::new();

        let err = client
            .fetch_formula_rb_from_url(
                "Formula/f/foo.rb",
                &format!("{}/Formula/f/foo.rb", mock_server.uri()),
                cache_dir.path(),
                Some(&"0".repeat(64)),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, Error::ChecksumMismatch { .. }));
    }

    #[tokio::test]
    async fn a_cached_formula_rb_that_no_longer_matches_is_fetched_again() {
        let mock_server = MockServer::start().await;
        let current = "class Foo < Formula\n  url \"new\"\nend\n";
        Mock::given(method("GET"))
            .and(path("/Formula/f/foo.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(current))
            .mount(&mock_server)
            .await;
        let url = format!("{}/Formula/f/foo.rb", mock_server.uri());
        let cache = ApiCache::in_memory().unwrap();
        cache
            .put(
                &format!("rb:{url}"),
                &CacheEntry {
                    etag: None,
                    last_modified: None,
                    body: "class Foo < Formula\nend\n".to_string(),
                },
            )
            .unwrap();

        let cache_dir = tempdir().unwrap();
        let client = ApiClient::new().with_cache(cache);
        let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
        sha2::Digest::update(&mut hasher, current.as_bytes());
        let sha = crate::checksum::sha256_hex(hasher);
        let fetched = client
            .fetch_formula_rb_from_url("Formula/f/foo.rb", &url, cache_dir.path(), Some(&sha))
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(fetched).unwrap(), current);
    }

    #[test]
    fn formula_sources_are_fetched_at_the_apis_commit() {
        let core = RubySourceLocator::parse("Formula/f/foo.rb");
        assert_eq!(
            core.to_url(Some("96641d7")),
            "https://raw.githubusercontent.com/Homebrew/homebrew-core/96641d7/Formula/f/foo.rb"
        );
        assert_eq!(
            core.to_url(None),
            format!("{HOMEBREW_CORE_RAW_BASE}/Formula/f/foo.rb")
        );
    }

    #[test]
    fn search_matches_like_brew() {
        let plain = search_matcher("Node-JS").unwrap();
        assert!(plain("nodejs") && plain("node.js@22") && !plain("node"));
        let regex = search_matcher("/^jq$/").unwrap();
        assert!(regex("jq") && !regex("gojq"));
        assert!(search_matcher("/(/").is_err());
    }

    #[tokio::test]
    async fn fetches_cask_json() {
        let mock_server = MockServer::start().await;
        let cask_json = r#"{
  "token": "iterm2",
  "version": "3.5.0",
  "url": "https://example.com/iterm2.zip",
  "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "artifacts": [{"app":["iTerm.app"]}]
}"#;

        Mock::given(method("GET"))
            .and(path("/iterm2.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(cask_json))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri())
            .unwrap()
            .with_cask_base_url(mock_server.uri());
        let cask = client.get_cask("iterm2").await.unwrap();
        assert_eq!(cask["token"], "iterm2");
        assert_eq!(cask["version"], "3.5.0");
    }

    #[tokio::test]
    async fn suggest_formulas_returns_ranked_matches_from_bulk_index() {
        let mock_server = MockServer::start().await;
        let bulk = r#"[
            {"name":"python","aliases":["python@3.13"],"oldnames":["python3"]},
            {"name":"pytest"},
            {"name":"pypy"}
        ]"#;

        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let suggestions = client.suggest_formulas("pythn", 3).await.unwrap();

        assert_eq!(suggestions.first().map(String::as_str), Some("python"));
    }

    #[tokio::test]
    async fn suggest_formulas_reuses_cached_candidates_across_calls() {
        let mock_server = MockServer::start().await;
        let bulk = r#"[
            {"name":"python"},
            {"name":"pytest"},
            {"name":"pypy"}
        ]"#;

        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();

        let first = client.suggest_formulas("pythn", 3).await.unwrap();
        let second = client.suggest_formulas("pythn", 3).await.unwrap();

        assert_eq!(first.first().map(String::as_str), Some("python"));
        assert_eq!(second.first().map(String::as_str), Some("python"));
    }

    #[tokio::test]
    async fn casks_of_other_taps_are_read_from_their_files() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;
        let tmp = tempdir().unwrap();
        let tap = crate::tap::TapName::parse("someone/tools").unwrap();
        let taps = crate::tap::Taps::new(tmp.path());
        let casks = taps.path(&tap).join("Casks");
        std::fs::create_dir_all(&casks).unwrap();
        std::fs::write(
            casks.join("thing.rb"),
            "cask \"thing\" do\n  version \"1.0\"\n  sha256 :no_check\n  url \"https://example.com/thing.zip\"\n  binary \"thing\"\nend\n",
        )
        .unwrap();
        let trust = crate::tap::trust::TrustStore::at(tmp.path().join("trust.json"));
        trust
            .trust(crate::tap::trust::TrustKind::Tap, "someone/tools")
            .unwrap();
        let source = TapSource {
            taps,
            trust,
            platform: crate::tap::ruby::Platform {
                macos: Some((26, 0)),
                arm: true,
            },
            installed: Default::default(),
        };
        let client = ApiClient::with_base_url(format!("{}/formula", mock_server.uri()))
            .unwrap()
            .with_cask_base_url(format!("{}/cask", mock_server.uri()))
            .with_taps(source);

        let cask = client.get_cask("someone/tools/thing").await.unwrap();
        assert_eq!(cask["token"], "thing");
        assert_eq!(cask["tap"], "someone/tools");
        assert_eq!(cask["sha256"], "no_check");
        // A name only a tap has is found there.
        let cask = client.get_cask("thing").await.unwrap();
        assert_eq!(cask["full_token"], "someone/tools/thing");
        assert!(matches!(
            client.get_cask("missing").await,
            Err(Error::MissingFormula { .. })
        ));
    }

    #[tokio::test]
    async fn formulae_migrated_to_another_tap_are_read_there() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula/foo.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FOO))
            .mount(&mock_server)
            .await;
        let tmp = tempdir().unwrap();
        let taps = crate::tap::Taps::new(tmp.path());
        let old = crate::tap::TapName::parse("old/tools").unwrap();
        let new = crate::tap::TapName::parse("new/tools").unwrap();
        std::fs::create_dir_all(taps.path(&old).join("Formula")).unwrap();
        std::fs::create_dir_all(taps.path(&new).join("Formula")).unwrap();
        // A stale copy left in the old tap is not the one read.
        for (tap, body) in [(&old, "stale"), (&new, "moved")] {
            std::fs::write(
                taps.path(tap).join("Formula/thing.rb"),
                format!("class Thing < Formula\n  desc \"{body}\"\n  url \"https://example.com/thing-1.0.tar.gz\"\nend\n"),
            )
            .unwrap();
        }
        std::fs::write(
            taps.path(&old).join("tap_migrations.json"),
            r#"{"thing": "new/tools", "foo": "homebrew/core", "gone": "elsewhere/tap/gone", "app": "homebrew/cask"}"#,
        )
        .unwrap();
        let trust = crate::tap::trust::TrustStore::at(tmp.path().join("trust.json"));
        for tap in ["old/tools", "new/tools"] {
            trust.trust(crate::tap::trust::TrustKind::Tap, tap).unwrap();
        }
        let source = TapSource {
            taps,
            trust,
            platform: crate::tap::ruby::Platform {
                macos: Some((26, 0)),
                arm: true,
            },
            installed: Default::default(),
        };
        let client = ApiClient::with_base_url(format!("{}/formula", mock_server.uri()))
            .unwrap()
            .with_taps(source);

        let thing = client.get_formula("old/tools/thing").await.unwrap();
        assert_eq!(thing.desc.as_deref(), Some("moved"));
        assert_eq!(thing.tap.as_deref(), Some("new/tools"));
        assert_eq!(
            client.get_formula("old/tools/foo").await.unwrap().name,
            "foo"
        );
        assert!(matches!(
            client.get_formula("old/tools/gone").await,
            Err(Error::MissingFormula { name }) if name == "old/tools/gone"
        ));
        assert!(matches!(
            client.get_formula("old/tools/app").await,
            Err(Error::MissingFormula { .. })
        ));
    }

    #[tokio::test]
    async fn renamed_core_casks_resolve_by_their_old_tokens() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cask.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[{"token":"doppler-app","old_tokens":["doppler"]},{"token":"other"}]"#,
            ))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/cask/doppler-app.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"doppler-app"}"#))
            .mount(&mock_server)
            .await;
        let client = ApiClient::with_base_url(format!("{}/formula", mock_server.uri()))
            .unwrap()
            .with_cask_base_url(format!("{}/cask", mock_server.uri()));

        let cask = client.get_cask("doppler").await.unwrap();
        assert_eq!(cask["token"], "doppler-app");
        assert!(matches!(
            client.get_cask("nothing").await,
            Err(Error::MissingFormula { name }) if name == "cask:nothing"
        ));
    }

    #[tokio::test]
    async fn casks_migrated_to_core_are_not_old_names_of_formulae() {
        let mock_server = MockServer::start().await;
        let cli = FOO.replace("\"foo\"", "\"openshift-cli\"").replacen(
            "{",
            r#"{"oldnames":["openshift-client","oc-old"],"#,
            1,
        );
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk(&[&cli])))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/cask_tap_migrations.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"openshift-client":"homebrew/core/openshift-cli","basex":"homebrew/core"}"#,
            ))
            .mount(&mock_server)
            .await;

        let client = indexed_client(&mock_server, ApiCache::in_memory().unwrap());
        assert_eq!(
            client.get_formula("oc-old").await.unwrap().name,
            "openshift-cli"
        );
        assert!(matches!(
            client.get_formula("openshift-client").await,
            Err(Error::MissingFormula { .. })
        ));
    }

    #[tokio::test]
    async fn suggest_formulas_returns_empty_for_tap_references() {
        let client = ApiClient::new();
        let suggestions = client
            .suggest_formulas("hashicorp/tap/terraform", 3)
            .await
            .unwrap();

        assert!(suggestions.is_empty());
    }

    fn bulk(entries: &[&str]) -> String {
        format!("[{}]", entries.join(","))
    }

    const FOO: &str = include_str!("../../../nb_core/fixtures/formula_foo.json");

    /// A client whose cache is `cache`, pointed at `server`'s `/formula`.
    fn indexed_client(server: &MockServer, cache: ApiCache) -> ApiClient {
        ApiClient::with_base_url(format!("{}/formula", server.uri()))
            .unwrap()
            .with_cache(cache)
    }

    #[tokio::test]
    async fn search_lists_formulae_like_brew() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[{"name":"node","aliases":["nodejs"]},{"name":"node@22"},
                    {"name":"node@9"},{"name":"node-build"},
                    {"name":"certifi","oldnames":["python-certifi"]},
                    {"name":"lua","aliases":["luajit-node"]}]"#,
            ))
            .mount(&mock_server)
            .await;
        let client = indexed_client(&mock_server, ApiCache::in_memory().unwrap());

        // nodejs is dropped for node; luajit-node stays, its formula didn't match.
        let found = client.search_formulas("node").await.unwrap();
        assert_eq!(
            found,
            ["luajit-node", "node", "node@9", "node@22", "node-build"]
        );
        assert!(client.search_formulas("python").await.unwrap().is_empty());
        // node only comes up by spelling, and still hides the aliases that matched.
        assert_eq!(
            client.search_formulas("nodejs").await.unwrap(),
            ["node", "node@9", "node@22"]
        );
        assert_eq!(
            client.search_formulas("/nodejs/").await.unwrap(),
            ["nodejs"]
        );
    }

    #[tokio::test]
    async fn formulas_are_served_from_the_index_without_a_request_each() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(bulk(&[FOO, r#"{"name":"bar","versions":{"stable":"2"},"dependencies":[],"bottle":{"stable":{"files":{}}}}"#]))
                    .insert_header("etag", "\"abc123\""),
            )
            .expect(1)
            .mount(&mock_server)
            .await;
        // No per-formula endpoint at all: lookups must come from the index.

        let client = indexed_client(&mock_server, ApiCache::in_memory().unwrap());
        let foo = client.get_formula("foo").await.unwrap();
        let bar = client.get_formula("bar").await.unwrap();

        assert_eq!(foo.versions.stable, "1.2.3");
        assert_eq!(bar.versions.stable, "2");
        assert_eq!(client.index_len(), 2);
        let meta = client.cache.as_ref().unwrap().index_meta().unwrap();
        assert_eq!(meta.etag.as_deref(), Some("\"abc123\""));
    }

    #[tokio::test]
    async fn a_fresh_index_is_not_revalidated() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock_server)
            .await;
        let cache = ApiCache::in_memory().unwrap();
        cache
            .replace_index(
                index_entries(&bulk(&[FOO]), &[]).unwrap(),
                &IndexMeta {
                    etag: None,
                    last_modified: None,
                    fetched_at: unix_now(),
                },
            )
            .unwrap();

        let client = indexed_client(&mock_server, cache);
        assert_eq!(client.get_formula("foo").await.unwrap().name, "foo");
    }

    #[tokio::test]
    async fn a_stale_index_is_revalidated_and_kept_on_304() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .and(header("If-None-Match", "\"abc123\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&mock_server)
            .await;
        let cache = ApiCache::in_memory().unwrap();
        let stale = unix_now() - 24 * 3600;
        cache
            .replace_index(
                index_entries(&bulk(&[FOO]), &[]).unwrap(),
                &IndexMeta {
                    etag: Some("\"abc123\"".into()),
                    last_modified: None,
                    fetched_at: stale,
                },
            )
            .unwrap();

        let client = indexed_client(&mock_server, cache);
        assert_eq!(client.get_formula("foo").await.unwrap().name, "foo");
        let meta = client.cache.as_ref().unwrap().index_meta().unwrap();
        assert!(meta.fetched_at > stale, "304 must bump the timestamp");
        assert_eq!(meta.etag.as_deref(), Some("\"abc123\""));
    }

    #[tokio::test]
    async fn a_stale_index_is_replaced_on_200() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk(&[FOO])))
            .mount(&mock_server)
            .await;
        let cache = ApiCache::in_memory().unwrap();
        cache
            .replace_index(
                [IndexEntry {
                    name: "gone".into(),
                    body: r#"{"name":"gone"}"#,
                    aliases: vec![],
                }],
                &IndexMeta {
                    etag: None,
                    last_modified: None,
                    fetched_at: 0,
                },
            )
            .unwrap();

        let client = indexed_client(&mock_server, cache);
        assert_eq!(client.get_formula("foo").await.unwrap().name, "foo");
        assert!(
            client
                .cache
                .as_ref()
                .unwrap()
                .index_formula("gone")
                .is_none()
        );
    }

    #[tokio::test]
    async fn aliases_resolve_through_the_index() {
        let mock_server = MockServer::start().await;
        let foo_with_alias = FOO.replacen(
            "{",
            r#"{"aliases":["foo-alias"],"oldnames":["old-foo"],"#,
            1,
        );
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk(&[&foo_with_alias])))
            .mount(&mock_server)
            .await;

        let client = indexed_client(&mock_server, ApiCache::in_memory().unwrap());
        assert_eq!(client.get_formula("foo-alias").await.unwrap().name, "foo");
        assert_eq!(client.get_formula("old-foo").await.unwrap().name, "foo");
    }

    #[tokio::test]
    async fn an_alias_wins_over_a_formula_of_that_name() {
        let mock_server = MockServer::start().await;
        let foo = FOO.replacen("{", r#"{"aliases":["bar"],"oldnames":["baz"],"#, 1);
        let bar = FOO.replace("\"foo\"", "\"bar\"");
        let baz = FOO.replace("\"foo\"", "\"baz\"");
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk(&[&foo, &bar, &baz])))
            .mount(&mock_server)
            .await;

        let client = indexed_client(&mock_server, ApiCache::in_memory().unwrap());
        assert_eq!(client.get_formula("bar").await.unwrap().name, "foo");
        assert_eq!(client.get_formula("baz").await.unwrap().name, "foo");
    }

    #[tokio::test]
    async fn a_name_missing_from_the_index_is_asked_for_once() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk(&[FOO])))
            .mount(&mock_server)
            .await;
        // Newer than the index: the API still has it.
        Mock::given(method("GET"))
            .and(path("/formula/brand-new.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(FOO.replace("\"foo\"", "\"brand-new\"")),
            )
            .expect(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/nonexistent.json"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = indexed_client(&mock_server, ApiCache::in_memory().unwrap());
        assert_eq!(
            client.get_formula("brand-new").await.unwrap().name,
            "brand-new"
        );
        let err = client.get_formula("nonexistent").await.unwrap_err();
        assert!(matches!(err, Error::MissingFormula { name } if name == "nonexistent"));
    }

    #[tokio::test]
    async fn an_unreachable_index_falls_back_to_the_cached_one_or_the_api() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/foo.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FOO))
            .expect(1)
            .mount(&mock_server)
            .await;

        // No index yet: per-formula requests as before.
        let client = indexed_client(&mock_server, ApiCache::in_memory().unwrap());
        assert_eq!(client.get_formula("foo").await.unwrap().name, "foo");

        // A stale index that cannot be refreshed is still used.
        let cache = ApiCache::in_memory().unwrap();
        cache
            .replace_index(
                index_entries(&bulk(&[FOO.replace("\"foo\"", "\"cached\"").as_str()]), &[])
                    .unwrap(),
                &IndexMeta {
                    etag: None,
                    last_modified: None,
                    fetched_at: 0,
                },
            )
            .unwrap();
        let client = indexed_client(&mock_server, cache);
        assert_eq!(client.get_formula("cached").await.unwrap().name, "cached");
    }

    #[tokio::test]
    async fn refresh_index_forced_fetches_even_when_fresh() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk(&[FOO])))
            .expect(1)
            .mount(&mock_server)
            .await;
        let cache = ApiCache::in_memory().unwrap();
        cache
            .replace_index(
                [],
                &IndexMeta {
                    etag: None,
                    last_modified: None,
                    fetched_at: unix_now(),
                },
            )
            .unwrap();

        let client = indexed_client(&mock_server, cache);
        client.refresh_index(false).await.unwrap();
        assert_eq!(client.index_len(), 0);
        client.refresh_index(true).await.unwrap();
        assert_eq!(client.index_len(), 1);
    }

    #[test]
    fn index_entries_keep_bodies_verbatim_and_skip_nameless_entries() {
        let entries = index_entries(
            r#"[{"name":"python","aliases":["python@3.13"],"oldnames":["python3","py-cask"],"x":1},{"aliases":["orphan"]},{"name":" ripgrep ","aliases":["rg","ripgrep"]}]"#,
            &["py-cask".to_string()],
        )
        .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "python");
        assert_eq!(
            entries[0].body,
            r#"{"name":"python","aliases":["python@3.13"],"oldnames":["python3","py-cask"],"x":1}"#
        );
        assert_eq!(entries[0].aliases, ["python@3.13", "python3"]);
        assert_eq!(entries[1].name, "ripgrep");
        assert_eq!(entries[1].aliases, ["rg"], "a name is not its own alias");
        assert!(index_entries("{}", &[]).is_err());
    }
}
