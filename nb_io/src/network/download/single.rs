use std::collections::HashMap;
use std::io::{Read, Seek, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT_RANGES, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use sha2::{Digest, Sha256};
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::network::tls::shared_tls_config;
use crate::progress::InstallProgress;
use crate::storage::blob::BlobCache;
use nb_core::Error;

use super::auth::{TokenCache, TokenEndpoint, fetch_download_response_internal, prefetch_tokens};
use super::{
    BodyConsumer, Commit, DownloadError, DownloadProgressCallback, GLOBAL_DOWNLOAD_CONCURRENCY,
    MAX_DOWNLOAD_ATTEMPTS,
};

fn get_alternate_urls(primary_url: &str) -> Vec<String> {
    let mut alternates = Vec::new();

    if let Ok(mirrors) = std::env::var("HOMEBREW_BOTTLE_MIRRORS") {
        for mirror in mirrors.split(',') {
            let mirror = mirror.trim();
            if !mirror.is_empty()
                && let Some(alt) = transform_url_to_mirror(primary_url, mirror)
            {
                alternates.push(alt);
            }
        }
    }

    alternates
}

fn transform_url_to_mirror(url: &str, mirror_domain: &str) -> Option<String> {
    if url.contains("ghcr.io") {
        Some(url.replace("ghcr.io", mirror_domain))
    } else {
        None
    }
}

/// Downloads bottles into the blob cache over one connection pool. Every
/// request reuses the registry and CDN connections of the ones before it,
/// and registry tokens are fetched up front for a whole plan where the
/// registry is known; see [`Downloader::prefetch_tokens`].
pub struct Downloader {
    client: reqwest::Client,
    /// HTTP/1.1 only, so the parts of a split body each get a connection of
    /// their own rather than sharing one HTTP/2 connection.
    range_client: reqwest::Client,
    pub(crate) blob_cache: BlobCache,
    token_cache: TokenCache,
    token_endpoints: HashMap<String, TokenEndpoint>,
}

impl Downloader {
    pub fn new(blob_cache: BlobCache) -> Self {
        let builder = || {
            reqwest::Client::builder()
                .user_agent("nextbrew/0.1")
                .use_preconfigured_tls((*shared_tls_config()).clone())
                .pool_max_idle_per_host(GLOBAL_DOWNLOAD_CONCURRENCY)
                .pool_idle_timeout(Duration::from_secs(90))
                .tcp_nodelay(true)
                .tcp_keepalive(Duration::from_secs(60))
                .connect_timeout(Duration::from_secs(30))
                .timeout(Duration::from_secs(300))
        };
        let client = builder()
            .http2_adaptive_window(true)
            .http2_initial_stream_window_size(Some(2 * 1024 * 1024))
            .http2_initial_connection_window_size(Some(4 * 1024 * 1024))
            .build()
            .expect("failed to build HTTP client");
        let range_client = builder()
            .http1_only()
            .build()
            .expect("failed to build HTTP client");

        Self {
            client,
            range_client,
            blob_cache,
            token_cache: Arc::new(RwLock::new(HashMap::new())),
            token_endpoints: TokenEndpoint::known(),
        }
    }

    /// Treat `host` as a registry handing out tokens at `endpoint`.
    #[cfg(test)]
    pub(crate) fn with_token_endpoint(mut self, host: &str, endpoint: TokenEndpoint) -> Self {
        self.token_endpoints.insert(host.to_string(), endpoint);
        self
    }

    pub fn remove_blob(&self, sha256: &str) -> bool {
        self.blob_cache.remove_blob(sha256).unwrap_or(false)
    }

    /// Fetch one registry token covering every blob in `urls` before they
    /// are downloaded, so none of them pays the 401 round trip.
    pub(crate) async fn prefetch_tokens(&self, urls: &[String]) {
        prefetch_tokens(&self.client, &self.token_cache, &self.token_endpoints, urls).await;
    }

    pub async fn download(&self, url: &str, expected_sha256: &str) -> Result<PathBuf, Error> {
        self.download_with_progress(url, expected_sha256, None, None, None)
            .await
    }

    pub async fn download_with_progress(
        &self,
        url: &str,
        expected_sha256: &str,
        name: Option<String>,
        progress: Option<DownloadProgressCallback>,
        consumer: Option<BodyConsumer>,
    ) -> Result<PathBuf, Error> {
        if self.blob_cache.has_blob(expected_sha256) {
            if let (Some(cb), Some(n)) = (&progress, &name) {
                cb(InstallProgress::DownloadCompleted {
                    name: n.clone(),
                    total_bytes: 0,
                });
            }
            return Ok(self.blob_cache.blob_path(expected_sha256));
        }

        let mut urls = vec![url.to_string()];
        urls.extend(get_alternate_urls(url));
        self.download_from(&urls, expected_sha256, name, progress, consumer)
            .await
    }

    /// Download from the first of `urls` that works. Each URL gets up to
    /// [`MAX_DOWNLOAD_ATTEMPTS`] tries with backoff while its failures look
    /// transient; a final failure moves on to the next URL. The consumer, if
    /// any, sees the first attempt only.
    async fn download_from(
        &self,
        urls: &[String],
        expected_sha256: &str,
        name: Option<String>,
        progress: Option<DownloadProgressCallback>,
        mut consumer: Option<BodyConsumer>,
    ) -> Result<PathBuf, Error> {
        let mut last_error = None;

        for url in urls {
            for attempt in 1..=MAX_DOWNLOAD_ATTEMPTS {
                match self
                    .download_once(
                        url,
                        expected_sha256,
                        name.clone(),
                        progress.clone(),
                        consumer.take(),
                    )
                    .await
                {
                    Ok(path) => return Ok(path),
                    Err(DownloadError { error, transient }) => {
                        let retry = transient && attempt < MAX_DOWNLOAD_ATTEMPTS;
                        warn!(
                            url,
                            attempt,
                            error = %error,
                            retry,
                            "download attempt failed"
                        );
                        last_error = Some(error);
                        if !retry {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(250 << (attempt - 1))).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| Error::NetworkFailure {
            message: "all download attempts failed".to_string(),
        }))
    }

    async fn download_once(
        &self,
        url: &str,
        expected_sha256: &str,
        name: Option<String>,
        progress: Option<DownloadProgressCallback>,
        consumer: Option<BodyConsumer>,
    ) -> Result<PathBuf, DownloadError> {
        let response =
            fetch_download_response_internal(&self.client, &self.token_cache, url).await?;
        download_response_internal(
            &self.blob_cache,
            &self.range_client,
            response,
            expected_sha256,
            name,
            progress,
            consumer,
        )
        .await
    }
}

/// How many body chunks may wait for a slow consumer before the download
/// pauses for it.
const CONSUMER_BACKLOG: usize = 32;

/// A [`BodyConsumer`] running on a blocking thread, fed chunk by chunk.
struct Tee {
    sender: Option<mpsc::Sender<Bytes>>,
    consumer: JoinHandle<Result<Commit, Error>>,
}

impl Tee {
    fn start(consumer: BodyConsumer) -> Self {
        let (sender, receiver) = mpsc::channel(CONSUMER_BACKLOG);
        let consumer = tokio::task::spawn_blocking(move || {
            let mut body = ChannelReader {
                receiver,
                current: Bytes::new(),
            };
            (consumer.consume)(&mut body)
        });
        Self {
            sender: Some(sender),
            consumer,
        }
    }

    /// Hand a chunk to the consumer. A consumer that has given up is left
    /// alone; the download itself goes on.
    async fn feed(&mut self, chunk: &Bytes) {
        if let Some(sender) = &self.sender
            && sender.send(chunk.clone()).await.is_err()
        {
            self.sender = None;
        }
    }

    /// Signal the end of the body, wait for the consumer and run its commit.
    async fn finish(mut self, name: &str) {
        self.sender = None;
        let commit = match self.consumer.await {
            Ok(Ok(commit)) => commit,
            Ok(Err(e)) => {
                debug!(name, error = %e, "could not unpack while downloading; unpacking the blob instead");
                return;
            }
            Err(e) => {
                warn!(name, error = %e, "unpack task failed; unpacking the blob instead");
                return;
            }
        };
        match tokio::task::spawn_blocking(commit).await {
            Ok(Ok(())) => debug!(name, "unpacked while downloading"),
            Ok(Err(e)) => {
                debug!(name, error = %e, "could not publish the streamed unpack; unpacking the blob instead")
            }
            Err(e) => warn!(name, error = %e, "commit task failed; unpacking the blob instead"),
        }
    }
}

/// The body as a blocking `Read`, for the consumer thread.
struct ChannelReader {
    receiver: mpsc::Receiver<Bytes>,
    current: Bytes,
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.current.is_empty() {
            match self.receiver.blocking_recv() {
                Some(chunk) => self.current = chunk,
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.current.len());
        buf[..n].copy_from_slice(&self.current[..n]);
        self.current = self.current.slice(n..);
        Ok(n)
    }
}

/// Bodies at least this large are fetched over several connections at once
/// (HTTP range requests). One connection can be far slower than the link:
/// behind some TLS-inspecting proxies a single transfer is held to about
/// 0.5 MB/s, and the CDNs bottles and casks come from serve byte ranges.
const SPLIT_MIN_BYTES: u64 = 4 * 1024 * 1024;
/// The size a part aims for, and the most parts one body is split into.
const SPLIT_PART_BYTES: u64 = 5 * 1024 * 1024 / 2;
const SPLIT_MAX_PARTS: u64 = 8;

/// Where the body goes, in order: the blob, its checksum and the consumer.
struct Sink<W> {
    writer: W,
    hasher: Sha256,
    tee: Option<Tee>,
}

impl<W: Write> Sink<W> {
    async fn push(&mut self, chunk: &Bytes) -> Result<(), DownloadError> {
        self.hasher.update(chunk);
        self.writer
            .write_all(chunk)
            .map_err(|e| DownloadError::permanent(Error::network("failed to write chunk")(e)))?;
        if let Some(tee) = &mut self.tee {
            tee.feed(chunk).await;
        }
        Ok(())
    }
}

/// Download progress, counted across the parts of a split body.
#[derive(Clone)]
struct Progress {
    callback: Option<(DownloadProgressCallback, String)>,
    downloaded: Arc<AtomicU64>,
    total_bytes: Option<u64>,
}

impl Progress {
    fn add(&self, bytes: u64) {
        let downloaded = self.downloaded.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if let Some((callback, name)) = &self.callback {
            callback(InstallProgress::DownloadProgress {
                name: name.clone(),
                downloaded,
                total_bytes: self.total_bytes,
            });
        }
    }
}

/// The byte ranges to fetch `response`'s body in, the first being the part
/// the response itself carries; `None` to read it in one piece.
fn split_plan(response: &reqwest::Response, total_bytes: Option<u64>) -> Option<Vec<Range<u64>>> {
    let total = total_bytes.filter(|len| *len >= SPLIT_MIN_BYTES)?;
    let headers = response.headers();
    if headers.get(ACCEPT_RANGES)?.as_bytes() != b"bytes" || headers.contains_key(CONTENT_ENCODING)
    {
        return None;
    }
    let parts = total.div_ceil(SPLIT_PART_BYTES).clamp(2, SPLIT_MAX_PARTS);
    let size = total.div_ceil(parts);
    Some(
        (0..parts)
            .map(|i| i * size..((i + 1) * size).min(total))
            .filter(|range| !range.is_empty())
            .collect(),
    )
}

/// A part being fetched into its file.
type PartTask = JoinHandle<Result<std::fs::File, Error>>;

/// Fetch `range` of `url` into an anonymous file in `dir`, rewound.
async fn fetch_part(
    client: reqwest::Client,
    url: reqwest::Url,
    range: Range<u64>,
    dir: PathBuf,
    progress: Progress,
) -> Result<std::fs::File, Error> {
    let wanted = format!("bytes {}-{}/", range.start, range.end - 1);
    let response = client
        .get(url)
        .header(RANGE, format!("bytes={}-{}", range.start, range.end - 1))
        .send()
        .await
        .map_err(Error::network("failed to request part"))?;
    let served = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with(&wanted));
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT || !served {
        return Err(Error::NetworkFailure {
            message: format!("range request answered with HTTP {}", response.status()),
        });
    }
    let mut file =
        tempfile::tempfile_in(&dir).map_err(Error::network("failed to create part file"))?;
    let mut stream = response.bytes_stream();
    let mut written = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(Error::network("failed to read part"))?;
        file.write_all(&chunk)
            .map_err(Error::network("failed to write part"))?;
        written += chunk.len() as u64;
        progress.add(chunk.len() as u64);
    }
    if written != range.end - range.start {
        return Err(Error::NetworkFailure {
            message: format!("part {wanted} ended after {written} bytes"),
        });
    }
    file.rewind()
        .map_err(Error::network("failed to rewind part"))?;
    Ok(file)
}

/// Fetch `url` from byte `start` to the end in one request and push it,
/// for when the parts could not be had: a range if the server gives one,
/// otherwise the whole body with its first `start` bytes skipped.
async fn push_remainder<W: Write>(
    client: &reqwest::Client,
    url: reqwest::Url,
    start: u64,
    sink: &mut Sink<W>,
    progress: &Progress,
) -> Result<(), DownloadError> {
    let response = client
        .get(url)
        .header(RANGE, format!("bytes={start}-"))
        .send()
        .await
        .map_err(|e| DownloadError::transient(Error::network("failed to request the rest")(e)))?;
    let mut skip = match response.status() {
        reqwest::StatusCode::PARTIAL_CONTENT => 0,
        reqwest::StatusCode::OK => start,
        status => {
            return Err(DownloadError::transient(Error::NetworkFailure {
                message: format!("HTTP {status} for the rest of the body"),
            }));
        }
    };
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let mut chunk = chunk
            .map_err(|e| DownloadError::transient(Error::network("failed to read chunk")(e)))?;
        if skip > 0 {
            let dropped = skip.min(chunk.len() as u64);
            chunk = chunk.slice(dropped as usize..);
            skip -= dropped;
        }
        progress.add(chunk.len() as u64);
        sink.push(&chunk).await?;
    }
    Ok(())
}

/// Stream a response into the blob cache, verifying its checksum, and
/// through the consumer if there is one. A large body is fetched in parts
/// over several connections and pushed on in order.
async fn download_response_internal(
    blob_cache: &BlobCache,
    range_client: &reqwest::Client,
    response: reqwest::Response,
    expected_sha256: &str,
    name: Option<String>,
    progress: Option<DownloadProgressCallback>,
    consumer: Option<BodyConsumer>,
) -> Result<PathBuf, DownloadError> {
    let total_bytes = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    if let (Some(cb), Some(n)) = (&progress, &name) {
        cb(InstallProgress::DownloadStarted {
            name: n.clone(),
            total_bytes,
        });
    }

    let writer = blob_cache
        .start_write(expected_sha256)
        .map_err(|e| DownloadError::permanent(Error::network("failed to create blob writer")(e)))?;
    let mut sink = Sink {
        writer,
        hasher: Sha256::new(),
        tee: consumer.map(Tee::start),
    };
    let counter = Progress {
        callback: progress.clone().zip(name.clone()),
        downloaded: Arc::new(AtomicU64::new(0)),
        total_bytes,
    };

    // The other parts start at once; this response carries the first.
    let url = response.url().clone();
    let plan = split_plan(&response, total_bytes);
    let part_dir = blob_cache
        .blob_path(expected_sha256)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let mut parts: Vec<(Range<u64>, PartTask)> = plan
        .iter()
        .flatten()
        .skip(1)
        .map(|range| {
            let task = tokio::spawn(fetch_part(
                range_client.clone(),
                url.clone(),
                range.clone(),
                part_dir.clone(),
                counter.clone(),
            ));
            (range.clone(), task)
        })
        .collect();
    let first_end = plan.as_ref().map_or(u64::MAX, |plan| plan[0].end);

    let mut received = 0u64;
    let mut stream = response.bytes_stream();
    while received < first_end {
        let Some(chunk) = stream.next().await else {
            break;
        };
        // A connection dropped mid-body; the next attempt starts over.
        let mut chunk = chunk
            .map_err(|e| DownloadError::transient(Error::network("failed to read chunk")(e)))?;
        if received + chunk.len() as u64 > first_end {
            chunk = chunk.slice(..(first_end - received) as usize);
        }
        received += chunk.len() as u64;
        counter.add(chunk.len() as u64);
        sink.push(&chunk).await?;
    }
    drop(stream);

    let mut parts = parts.drain(..);
    while let Some((range, task)) = parts.next() {
        match task.await {
            Ok(Ok(mut file)) => {
                let mut buf = vec![0u8; 256 * 1024];
                loop {
                    let n = file.read(&mut buf).map_err(|e| {
                        DownloadError::permanent(Error::network("failed to read part")(e))
                    })?;
                    if n == 0 {
                        break;
                    }
                    sink.push(&Bytes::copy_from_slice(&buf[..n])).await?;
                }
            }
            outcome => {
                let reason = match outcome {
                    Ok(Err(e)) => e.to_string(),
                    Err(e) => e.to_string(),
                    Ok(Ok(_)) => unreachable!(),
                };
                debug!(%reason, "fetching the body in parts failed; fetching the rest at once");
                for (_, task) in parts.by_ref() {
                    task.abort();
                }
                push_remainder(range_client, url.clone(), range.start, &mut sink, &counter).await?;
            }
        }
    }

    let Sink {
        mut writer,
        hasher,
        tee,
    } = sink;
    let actual_hash = crate::checksum::sha256_hex(hasher);

    if actual_hash != expected_sha256 {
        // Most often a truncated or mangled transfer; the next attempt may
        // get the real thing. Whatever the consumer made of it is dropped.
        return Err(DownloadError::transient(Error::ChecksumMismatch {
            expected: expected_sha256.to_string(),
            actual: actual_hash,
        }));
    }

    writer
        .flush()
        .map_err(|e| DownloadError::permanent(Error::network("failed to flush download")(e)))?;

    if let (Some(cb), Some(n)) = (&progress, &name) {
        cb(InstallProgress::DownloadCompleted {
            name: n.clone(),
            total_bytes: counter.downloaded.load(Ordering::Relaxed),
        });
    }

    let blob = writer.commit().map_err(DownloadError::permanent)?;
    if let Some(tee) = tee {
        tee.finish(name.as_deref().unwrap_or(expected_sha256)).await;
    }
    Ok(blob)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use wiremock::matchers::{header_exists, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const HELLO_SHA: &str = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

    fn downloader(tmp: &TempDir) -> Downloader {
        Downloader::new(BlobCache::new(tmp.path()).unwrap())
    }

    #[tokio::test]
    async fn valid_checksum_passes() {
        let mock_server = MockServer::start().await;
        let content = b"hello world";

        Mock::given(method("GET"))
            .and(path("/test.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let downloader = downloader(&tmp);

        let url = format!("{}/test.tar.gz", mock_server.uri());
        let result = downloader.download(&url, HELLO_SHA).await;

        assert!(result.is_ok());
        let blob_path = result.unwrap();
        assert!(blob_path.exists());
        assert_eq!(std::fs::read(&blob_path).unwrap(), content);
    }

    #[tokio::test]
    async fn mismatch_deletes_blob_and_errors() {
        let mock_server = MockServer::start().await;
        let content = b"hello world";
        let wrong_sha256 = "0000000000000000000000000000000000000000000000000000000000000000";

        Mock::given(method("GET"))
            .and(path("/test.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .expect(MAX_DOWNLOAD_ATTEMPTS as u64)
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let downloader = downloader(&tmp);

        let url = format!("{}/test.tar.gz", mock_server.uri());
        let result = downloader.download(&url, wrong_sha256).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::ChecksumMismatch { .. }));

        let blob_path = tmp
            .path()
            .join("blobs")
            .join(format!("{wrong_sha256}.tar.gz"));
        assert!(!blob_path.exists());

        let tmp_path = tmp
            .path()
            .join("tmp")
            .join(format!("{wrong_sha256}.tar.gz.part"));
        assert!(!tmp_path.exists());
    }

    #[tokio::test]
    async fn skips_download_if_blob_exists() {
        let mock_server = MockServer::start().await;
        let content = b"hello world";

        Mock::given(method("GET"))
            .and(path("/test.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .expect(0)
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let blob_cache = BlobCache::new(tmp.path()).unwrap();

        let mut writer = blob_cache.start_write(HELLO_SHA).unwrap();
        writer.write_all(content).unwrap();
        writer.commit().unwrap();

        let downloader = Downloader::new(blob_cache);
        let url = format!("{}/test.tar.gz", mock_server.uri());
        let result = downloader.download(&url, HELLO_SHA).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn retries_a_server_error_and_then_succeeds() {
        let mock_server = MockServer::start().await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();

        Mock::given(method("GET"))
            .and(path("/flaky.tar.gz"))
            .respond_with(move |_: &Request| {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(503)
                } else {
                    ResponseTemplate::new(200).set_body_bytes(b"hello world".to_vec())
                }
            })
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let url = format!("{}/flaky.tar.gz", mock_server.uri());
        let path = downloader(&tmp).download(&url, HELLO_SHA).await.unwrap();

        assert!(path.exists());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_client_error_is_not_retried() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/missing.tar.gz"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let url = format!("{}/missing.tar.gz", mock_server.uri());
        let err = downloader(&tmp)
            .download(&url, HELLO_SHA)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("HTTP 404"), "{err}");
    }

    #[tokio::test]
    async fn falls_back_to_the_next_url_after_the_first_gives_up() {
        let primary = MockServer::start().await;
        let mirror = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/x.tar.gz"))
            .respond_with(ResponseTemplate::new(500))
            .expect(MAX_DOWNLOAD_ATTEMPTS as u64)
            .mount(&primary)
            .await;
        Mock::given(method("GET"))
            .and(path("/x.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello world".to_vec()))
            .expect(1)
            .mount(&mirror)
            .await;

        let tmp = TempDir::new().unwrap();
        let urls = [
            format!("{}/x.tar.gz", primary.uri()),
            format!("{}/x.tar.gz", mirror.uri()),
        ];
        let path = downloader(&tmp)
            .download_from(&urls, HELLO_SHA, None, None, None)
            .await
            .unwrap();

        assert_eq!(std::fs::read(path).unwrap(), b"hello world");
    }

    /// A consumer that collects the body and records whether its commit ran.
    fn recording_consumer() -> (
        BodyConsumer,
        Arc<std::sync::Mutex<Vec<u8>>>,
        Arc<AtomicUsize>,
    ) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let committed = Arc::new(AtomicUsize::new(0));
        let (seen_in, committed_in) = (seen.clone(), committed.clone());
        let consumer = BodyConsumer {
            consume: Box::new(move |body| {
                let mut bytes = Vec::new();
                body.read_to_end(&mut bytes)
                    .map_err(Error::store("read body"))?;
                *seen_in.lock().unwrap() = bytes;
                Ok(Box::new(move || {
                    committed_in.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }) as Commit)
            }),
        };
        (consumer, seen, committed)
    }

    /// Serves `body`, answering a `Range: bytes=a-b` request with that slice
    /// as a CDN does (unless `honour` is off), and counts ranged requests.
    struct Ranged {
        body: Arc<Vec<u8>>,
        honour: bool,
        ranged: Arc<AtomicUsize>,
    }

    impl wiremock::Respond for Ranged {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let range = request
                .headers
                .get("range")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("bytes="))
                .and_then(|v| v.split_once('-'))
                .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)));
            match range {
                Some((start, end)) if self.honour => {
                    self.ranged.fetch_add(1, Ordering::SeqCst);
                    let len = self.body.len();
                    ResponseTemplate::new(206)
                        .insert_header("accept-ranges", "bytes")
                        .insert_header("content-range", format!("bytes {start}-{end}/{len}"))
                        .set_body_bytes(self.body[start..=end].to_vec())
                }
                _ => ResponseTemplate::new(200)
                    .insert_header("accept-ranges", "bytes")
                    .set_body_bytes(self.body.to_vec()),
            }
        }
    }

    async fn serve_ranged(
        server: &MockServer,
        size: usize,
        honour: bool,
    ) -> (Vec<u8>, String, Arc<AtomicUsize>) {
        let body: Vec<u8> = (0..size)
            .map(|i| (i % 251) as u8 ^ (i >> 16) as u8)
            .collect();
        let sha = crate::checksum::sha256_hex(Sha256::new_with_prefix(&body));
        let ranged = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/large.tar.gz"))
            .respond_with(Ranged {
                body: Arc::new(body.clone()),
                honour,
                ranged: ranged.clone(),
            })
            .mount(server)
            .await;
        (body, sha, ranged)
    }

    #[tokio::test]
    async fn a_large_body_is_fetched_in_parallel_ranges() {
        let server = MockServer::start().await;
        let (body, sha, ranged) = serve_ranged(&server, 9 * 1024 * 1024 + 123, true).await;
        let (consumer, seen, committed) = recording_consumer();

        let tmp = TempDir::new().unwrap();
        let url = format!("{}/large.tar.gz", server.uri());
        let blob = downloader(&tmp)
            .download_with_progress(&url, &sha, None, None, Some(consumer))
            .await
            .unwrap();

        assert!(ranged.load(Ordering::SeqCst) >= 2, "{ranged:?}");
        assert!(std::fs::read(blob).unwrap() == body);
        assert!(
            *seen.lock().unwrap() == body,
            "the consumer sees the body in order"
        );
        assert_eq!(committed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_small_body_is_fetched_in_one_request() {
        let server = MockServer::start().await;
        let (body, sha, ranged) = serve_ranged(&server, 1024 * 1024, true).await;

        let tmp = TempDir::new().unwrap();
        let url = format!("{}/large.tar.gz", server.uri());
        let blob = downloader(&tmp).download(&url, &sha).await.unwrap();

        assert_eq!(ranged.load(Ordering::SeqCst), 0);
        assert!(std::fs::read(blob).unwrap() == body);
    }

    #[tokio::test]
    async fn a_server_that_ignores_ranges_still_gives_the_whole_body() {
        let server = MockServer::start().await;
        let (body, sha, _) = serve_ranged(&server, 9 * 1024 * 1024, false).await;

        let tmp = TempDir::new().unwrap();
        let url = format!("{}/large.tar.gz", server.uri());
        let blob = downloader(&tmp).download(&url, &sha).await.unwrap();

        assert!(std::fs::read(blob).unwrap() == body);
    }

    #[tokio::test]
    async fn the_consumer_sees_the_whole_body_and_commits_after_the_checksum() {
        let mock_server = MockServer::start().await;
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let sha = {
            let mut h = Sha256::new();
            h.update(&body);
            crate::checksum::sha256_hex(h)
        };
        Mock::given(method("GET"))
            .and(path("/big.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&mock_server)
            .await;
        let (consumer, seen, committed) = recording_consumer();

        let tmp = TempDir::new().unwrap();
        let url = format!("{}/big.tar.gz", mock_server.uri());
        let blob = downloader(&tmp)
            .download_with_progress(&url, &sha, None, None, Some(consumer))
            .await
            .unwrap();

        assert_eq!(*seen.lock().unwrap(), body);
        assert_eq!(committed.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(blob).unwrap(), body);
    }

    #[tokio::test]
    async fn a_checksum_mismatch_never_commits_the_consumer() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/wrong.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello world".to_vec()))
            .mount(&mock_server)
            .await;
        let (consumer, seen, committed) = recording_consumer();
        let wrong = "0".repeat(64);

        let tmp = TempDir::new().unwrap();
        let url = format!("{}/wrong.tar.gz", mock_server.uri());
        let err = downloader(&tmp)
            .download_with_progress(&url, &wrong, None, None, Some(consumer))
            .await
            .unwrap_err();

        assert!(matches!(err, Error::ChecksumMismatch { .. }));
        assert_eq!(*seen.lock().unwrap(), b"hello world", "the consumer ran");
        assert_eq!(committed.load(Ordering::SeqCst), 0, "but was not committed");
    }

    #[tokio::test]
    async fn a_failing_consumer_does_not_fail_the_download() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/test.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello world".to_vec()))
            .mount(&mock_server)
            .await;
        let consumer = BodyConsumer {
            consume: Box::new(|body| {
                // Give up after the first bytes, like a corrupt archive would.
                let mut first = [0u8; 4];
                body.read_exact(&mut first)
                    .map_err(Error::store("read body"))?;
                Err(Error::StoreCorruption {
                    message: "not an archive".into(),
                })
            }),
        };

        let tmp = TempDir::new().unwrap();
        let url = format!("{}/test.tar.gz", mock_server.uri());
        let blob = downloader(&tmp)
            .download_with_progress(&url, HELLO_SHA, None, None, Some(consumer))
            .await
            .unwrap();

        assert_eq!(std::fs::read(blob).unwrap(), b"hello world");
    }

    #[tokio::test]
    async fn a_cached_blob_skips_the_consumer() {
        let tmp = TempDir::new().unwrap();
        let blob_cache = BlobCache::new(tmp.path()).unwrap();
        let mut writer = blob_cache.start_write(HELLO_SHA).unwrap();
        writer.write_all(b"hello world").unwrap();
        writer.commit().unwrap();
        let (consumer, seen, committed) = recording_consumer();

        Downloader::new(blob_cache)
            .download_with_progress(
                "http://127.0.0.1:9/unused",
                HELLO_SHA,
                None,
                None,
                Some(consumer),
            )
            .await
            .unwrap();

        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(committed.load(Ordering::SeqCst), 0);
    }

    /// A registry that demands a token: anonymous requests get a challenge.
    async fn registry(server: &MockServer, blob_path: &str) {
        Mock::given(method("GET"))
            .and(path(blob_path))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello world".to_vec()))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(blob_path))
            .respond_with(ResponseTemplate::new(401).append_header(
                "WWW-Authenticate",
                format!(
                    "Bearer realm=\"{}/token\",service=\"test\",scope=\"repository:homebrew/core/x:pull\"",
                    server.uri()
                ),
            ))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn answers_a_401_challenge_with_a_token() {
        let server = MockServer::start().await;
        registry(&server, "/v2/homebrew/core/x/blobs/sha256:abc").await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(query_param("service", "test"))
            .and(query_param("scope", "repository:homebrew/core/x:pull"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "test-token-12345"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = TempDir::new().unwrap();
        let downloader = downloader(&tmp);
        let url = format!("{}/v2/homebrew/core/x/blobs/sha256:abc", server.uri());

        let path = downloader.download(&url, HELLO_SHA).await.unwrap();
        assert!(path.exists());

        // The token is cached for the scope: a second blob of the same
        // formula needs no challenge.
        downloader.remove_blob(HELLO_SHA);
        downloader.download(&url, HELLO_SHA).await.unwrap();
        assert_eq!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.url.path().contains("/blobs/"))
                .count(),
            3,
            "challenge, authenticated GET, then one authenticated GET"
        );
    }

    #[tokio::test]
    async fn prefetches_one_token_for_every_blob_of_a_known_registry() {
        let server = MockServer::start().await;
        for name in ["a", "b", "c"] {
            registry(
                &server,
                &format!("/v2/homebrew/core/{name}/blobs/sha256:abc"),
            )
            .await;
        }
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(query_param("scope", "repository:homebrew/core/a:pull"))
            .and(query_param("scope", "repository:homebrew/core/b:pull"))
            .and(query_param("scope", "repository:homebrew/core/c:pull"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "shared-token",
                "expires_in": 300
            })))
            .expect(1)
            .mount(&server)
            .await;

        let host = server.uri().trim_start_matches("http://").to_string();
        let tmp = TempDir::new().unwrap();
        let downloader = downloader(&tmp).with_token_endpoint(
            &host,
            TokenEndpoint {
                realm: format!("{}/token", server.uri()),
                service: "test".into(),
            },
        );
        let urls: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| format!("{}/v2/homebrew/core/{name}/blobs/sha256:abc", server.uri()))
            .collect();

        downloader.prefetch_tokens(&urls).await;
        for url in &urls {
            downloader.download(url, HELLO_SHA).await.unwrap();
            downloader.remove_blob(HELLO_SHA);
        }

        let challenged = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| {
                r.url.path().contains("/blobs/") && !r.headers.contains_key("authorization")
            })
            .count();
        assert_eq!(
            challenged, 0,
            "every blob GET carried the pre-fetched token"
        );
    }

    #[tokio::test]
    async fn prefetch_skips_unknown_registries_and_cached_scopes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let tmp = TempDir::new().unwrap();
        let downloader = downloader(&tmp);
        downloader
            .prefetch_tokens(&[format!(
                "{}/v2/homebrew/core/a/blobs/sha256:abc",
                server.uri()
            )])
            .await;
    }
}
