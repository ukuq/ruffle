use super::{InterceptedResponse, RequestInterceptor, spawn_tokio};
use aes::Aes192;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use reqwest::header::{IF_MODIFIED_SINCE, LAST_MODIFIED};
use ruffle_core::backend::navigator::{ErrorResponse, NavigationMethod, OwnedFuture, Request};
use ruffle_core::loader::Error;
use scrypt::{Params as ScryptParams, scrypt};
use std::collections::HashSet;
use std::collections::VecDeque;
use std::fmt::Display;
use std::fs::{self, FileTimes, OpenOptions};
use std::io::{self, ErrorKind};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use url::{Host, Url};

const SEER2_PATH: &str = "/seer2";
const BLOOM_PATH: &str = "/config/bloom-path.data";
const MAGIC_PATH: &str = "/seer2-next-client-hello";
const FLASH_POLICY_PATH: &str = "/crossdomain.xml";
const ROOT_DNS: &str = "next-client-root.733702.xyz";
const LEGACY_UPSTREAM: &str = "http://seer2.61.com/";
const MAX_CACHE_BYTES: u64 = 1024 * 1024 * 1024;
const CACHE_PRUNE_INTERVAL_WRITES: usize = 64;
const MAX_NETWORK_RECORDS: usize = 2_000;
const FLASH_POLICY_DATA: &str = concat!(
    r#"<?xml version="1.0"?>"#,
    r#"<!DOCTYPE cross-domain-policy SYSTEM "#,
    r#""http://www.macromedia.com/xml/dtds/cross-domain-policy.dtd">"#,
    r#"<cross-domain-policy><allow-access-from domain="*" /></cross-domain-policy>"#
);

type Aes192CbcEncryptor = cbc::Encryptor<Aes192>;
type Aes192CbcDecryptor = cbc::Decryptor<Aes192>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkRequestSource {
    Pending,
    Internal,
    Override,
    Cache,
    VersionedUpstream,
    LegacyUpstream,
    Error,
}

impl NetworkRequestSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Internal => "Internal",
            Self::Override => "Override",
            Self::Cache => "Cache",
            Self::VersionedUpstream => "Versioned",
            Self::LegacyUpstream => "Legacy",
            Self::Error => "Error",
        }
    }
}

#[derive(Clone, Debug)]
pub struct NetworkRequestRecord {
    pub id: u64,
    pub started_at_millis: u128,
    pub duration_millis: Option<u128>,
    pub method: String,
    pub url: String,
    pub status: Option<u16>,
    pub source: NetworkRequestSource,
    pub response_bytes: Option<usize>,
    pub upstream_url: Option<String>,
    pub error: Option<String>,
}

/// Cumulative Seer2 cache metrics, matching seer2-next-client's
/// `CacheMetricKey` counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Seer2CacheMetrics {
    pub hit: u64,
    pub cached: u64,
    pub expired: u64,
    pub checked: u64,
    pub unchanged: u64,
    pub changed: u64,
    pub proxy: u64,
    pub fetch: u64,
}

#[derive(Default)]
struct CacheMetricCounters {
    hit: AtomicU64,
    cached: AtomicU64,
    expired: AtomicU64,
    checked: AtomicU64,
    unchanged: AtomicU64,
    changed: AtomicU64,
    proxy: AtomicU64,
    fetch: AtomicU64,
}

impl CacheMetricCounters {
    fn snapshot(&self) -> Seer2CacheMetrics {
        Seer2CacheMetrics {
            hit: self.hit.load(Ordering::Relaxed),
            cached: self.cached.load(Ordering::Relaxed),
            expired: self.expired.load(Ordering::Relaxed),
            checked: self.checked.load(Ordering::Relaxed),
            unchanged: self.unchanged.load(Ordering::Relaxed),
            changed: self.changed.load(Ordering::Relaxed),
            proxy: self.proxy.load(Ordering::Relaxed),
            fetch: self.fetch.load(Ordering::Relaxed),
        }
    }

    fn report(&self, key: CacheMetricKey) {
        let counter = match key {
            CacheMetricKey::Hit => &self.hit,
            CacheMetricKey::Cached => &self.cached,
            CacheMetricKey::Expired => &self.expired,
            CacheMetricKey::Checked => &self.checked,
            CacheMetricKey::Unchanged => &self.unchanged,
            CacheMetricKey::Changed => &self.changed,
            CacheMetricKey::Proxy => &self.proxy,
            CacheMetricKey::Fetch => &self.fetch,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
enum CacheMetricKey {
    Hit,
    Cached,
    Expired,
    Checked,
    Unchanged,
    Changed,
    Proxy,
    Fetch,
}

fn cache_metric_counters() -> &'static CacheMetricCounters {
    static METRICS: OnceLock<CacheMetricCounters> = OnceLock::new();
    METRICS.get_or_init(CacheMetricCounters::default)
}

fn report_cache_metric(key: CacheMetricKey) {
    cache_metric_counters().report(key);
}

pub fn seer2_cache_metrics() -> Seer2CacheMetrics {
    cache_metric_counters().snapshot()
}

static RUNTIME_CACHE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Clear the volatile request/runtime cache. Reqwest does not maintain a
/// Chromium-style response cache, so the native equivalent is to invalidate
/// the cached version root and Bloom manifest.
pub fn reset_seer2_version_manifest() {
    RUNTIME_CACHE_GENERATION.fetch_add(1, Ordering::SeqCst);
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Seer2FileCacheClearResult {
    pub removed_files: usize,
    pub removed_bytes: u64,
    pub failed_files: usize,
}

/// Delete only files that match the Electron-compatible encrypted game-cache
/// naming scheme, preserving unrelated files in the directory.
pub fn clear_seer2_file_cache(directory: &Path) -> io::Result<Seer2FileCacheClearResult> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error),
    };
    let mut result = Seer2FileCacheClearResult::default();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                result.failed_files += 1;
                tracing::warn!("Unable to inspect a Seer2 cache entry while clearing: {error}");
                continue;
            }
        };
        if !is_cache_file_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let bytes = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        match fs::remove_file(entry.path()) {
            Ok(()) => {
                result.removed_files += 1;
                result.removed_bytes = result.removed_bytes.saturating_add(bytes);
            }
            Err(error) => {
                result.failed_files += 1;
                tracing::warn!(
                    "Unable to remove Seer2 cache file {}: {error}",
                    entry.path().display()
                );
            }
        }
    }
    Ok(result)
}

#[derive(Default)]
struct NetworkMonitorState {
    next_id: u64,
    records: VecDeque<NetworkRequestRecord>,
}

#[derive(Default)]
pub struct NetworkMonitor {
    state: Mutex<NetworkMonitorState>,
}

impl NetworkMonitor {
    fn begin(&self, method: String, url: String) -> u64 {
        let Ok(mut state) = self.state.lock() else {
            return 0;
        };
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        if state.records.len() == MAX_NETWORK_RECORDS {
            state.records.pop_front();
        }
        state.records.push_back(NetworkRequestRecord {
            id,
            started_at_millis: system_time_millis(SystemTime::now()),
            duration_millis: None,
            method,
            url,
            status: None,
            source: NetworkRequestSource::Pending,
            response_bytes: None,
            upstream_url: None,
            error: None,
        });
        id
    }

    fn finish(
        &self,
        id: u64,
        duration_millis: u128,
        status: Option<u16>,
        source: NetworkRequestSource,
        response_bytes: Option<usize>,
        upstream_url: Option<String>,
        error: Option<String>,
    ) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some(record) = state.records.iter_mut().find(|record| record.id == id) {
            record.duration_millis = Some(duration_millis);
            record.status = status;
            record.source = source;
            record.response_bytes = response_bytes;
            record.upstream_url = upstream_url;
            record.error = error;
        }
    }

    pub fn snapshot(&self) -> Vec<NetworkRequestRecord> {
        self.state
            .lock()
            .map(|state| state.records.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.records.clear();
        }
    }
}

pub fn seer2_network_monitor() -> &'static NetworkMonitor {
    static MONITOR: OnceLock<NetworkMonitor> = OnceLock::new();
    MONITOR.get_or_init(NetworkMonitor::default)
}

struct Seer2Response {
    response: InterceptedResponse,
    source: NetworkRequestSource,
    upstream_url: Option<String>,
}

/// In-process equivalent of the Electron Seer2 HTTP interceptor.
///
/// Requests keep their original HTTP URL, but matching resources are served
/// from an override directory, the compatible encrypted cache, or an upstream
/// server. No TCP listener is created.
#[derive(Clone)]
pub struct Seer2VirtualHttp {
    proxy_root: Option<PathBuf>,
    cache_directory: PathBuf,
    legacy_upstream: Url,
    runtime: Arc<Mutex<RuntimeCache>>,
    file_locks: Arc<Mutex<HashSet<String>>>,
    initial_cache_maintenance: Arc<tokio::sync::OnceCell<()>>,
    cache_writes: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct Runtime {
    root_url: Url,
    bloom: BloomFilter,
}

struct RuntimeCache {
    generation: u64,
    value: Option<Runtime>,
}

#[derive(Clone)]
struct BloomFilter {
    function_count: usize,
    bits: Vec<u8>,
}

impl Seer2VirtualHttp {
    pub fn new(
        initial_url: &Url,
        cache_directory: PathBuf,
        proxy_root: Option<PathBuf>,
    ) -> Result<Self, String> {
        if initial_url.scheme() != "http" || !is_virtual_host(initial_url) {
            return Err(
                "Seer2 virtual HTTP requires a loopback or seer2.client HTTP movie URL".to_string(),
            );
        }

        seer2_network_monitor().clear();

        Ok(Self {
            proxy_root,
            cache_directory,
            legacy_upstream: Url::parse(LEGACY_UPSTREAM).map_err(|error| error.to_string())?,
            runtime: Arc::new(Mutex::new(RuntimeCache {
                generation: RUNTIME_CACHE_GENERATION.load(Ordering::SeqCst),
                value: None,
            })),
            file_locks: Arc::new(Mutex::new(HashSet::new())),
            initial_cache_maintenance: Arc::new(tokio::sync::OnceCell::new()),
            cache_writes: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn handles(&self, url: &Url) -> bool {
        url.scheme() == "http" && is_virtual_host(url)
    }

    async fn handle(
        &self,
        url: Url,
        method: NavigationMethod,
        client: Option<reqwest::Client>,
    ) -> Result<Seer2Response, String> {
        let original_url = url.to_string();
        let url_path = url.path().to_string();
        tracing::debug!("Seer2 virtual HTTP {method} {original_url}");

        if url_path == MAGIC_PATH {
            let body = format!(r#"{{"version":"{}"}}"#, env!("CARGO_PKG_VERSION")).into_bytes();
            return Ok(response(
                original_url,
                200,
                body,
                NetworkRequestSource::Internal,
                None,
            ));
        }

        if url_path == FLASH_POLICY_PATH {
            return Ok(response(
                original_url,
                200,
                FLASH_POLICY_DATA.as_bytes().to_vec(),
                NetworkRequestSource::Internal,
                None,
            ));
        }

        if url_path.ends_with('/') || url_path.ends_with('\\') || !url_path.starts_with("/seer2/") {
            return Ok(response(
                original_url,
                403,
                b"not a valid path".to_vec(),
                NetworkRequestSource::Internal,
                None,
            ));
        }

        if let Some(file_path) = self.proxy_file_path(&url_path)
            && file_path.is_file()
        {
            let read_path = file_path.clone();
            match tokio::task::spawn_blocking(move || fs::read(read_path)).await {
                Ok(Ok(body)) => {
                    tracing::info!("Seer2 virtual HTTP proxy file: {url_path}");
                    report_cache_metric(CacheMetricKey::Proxy);
                    return Ok(response(
                        original_url,
                        200,
                        body,
                        NetworkRequestSource::Override,
                        None,
                    ));
                }
                Ok(Err(error)) => {
                    tracing::warn!(
                        "Failed to read Seer2 proxy file {}: {error}",
                        file_path.display()
                    );
                }
                Err(error) => tracing::warn!(
                    "Failed to join Seer2 proxy file read for {}: {error}",
                    file_path.display()
                ),
            }
        }

        let bloom_path = url_path
            .strip_prefix(SEER2_PATH)
            .unwrap_or_default()
            .to_string();
        if bloom_path == BLOOM_PATH {
            return Ok(response(
                original_url,
                403,
                Vec::new(),
                NetworkRequestSource::Internal,
                None,
            ));
        }

        self.ensure_initial_cache_maintenance().await;

        let client =
            client.ok_or_else(|| "Network is unavailable for Seer2 virtual HTTP".to_string())?;
        let runtime = self.load_runtime(&client).await?;
        let path_hit_bloom = runtime.bloom.contains(&bloom_path);
        let cache_file = self.cache_file_path(&url_path);

        if !self.is_file_locked(&bloom_path)
            && let Ok(metadata) = fs::metadata(&cache_file)
            && metadata.is_file()
        {
            let modified = metadata.modified().ok();
            let cache_is_current = if path_hit_bloom {
                modified.is_some_and(|modified| {
                    runtime
                        .bloom
                        .contains(&format!("{bloom_path}?v={}", system_time_millis(modified)))
                })
            } else {
                true
            };

            if cache_is_current {
                let read_key = bloom_path.clone();
                let read_path = cache_file.clone();
                match tokio::task::spawn_blocking(move || {
                    let body = read_with_decipher(&read_key, &read_path)?;
                    touch_cache_file(&read_path);
                    Ok::<_, io::Error>(body)
                })
                .await
                {
                    Ok(Ok(body)) => {
                        tracing::info!("Seer2 virtual HTTP cache hit: {url_path}");
                        report_cache_metric(CacheMetricKey::Hit);
                        if !path_hit_bloom && let Some(modified) = modified {
                            self.spawn_cache_check(
                                client.clone(),
                                bloom_path.clone(),
                                cache_file,
                                modified,
                            );
                        }
                        return Ok(response(
                            original_url,
                            200,
                            body,
                            NetworkRequestSource::Cache,
                            None,
                        ));
                    }
                    Ok(Err(error)) => tracing::warn!(
                        "Failed to read Seer2 cache file {}: {error}",
                        cache_file.display()
                    ),
                    Err(error) => tracing::warn!(
                        "Failed to join Seer2 cache read for {}: {error}",
                        cache_file.display()
                    ),
                }
            } else {
                tracing::info!("Seer2 virtual HTTP cache expired: {url_path}");
                report_cache_metric(CacheMetricKey::Expired);
            }
        }

        let (upstream_url, source) = if path_hit_bloom {
            (
                runtime.root_url.join(bloom_path.trim_start_matches('/')),
                NetworkRequestSource::VersionedUpstream,
            )
        } else {
            (
                self.legacy_upstream
                    .join(bloom_path.trim_start_matches('/')),
                NetworkRequestSource::LegacyUpstream,
            )
        };
        let mut upstream_url = upstream_url.map_err(|error| error.to_string())?;
        upstream_url.set_query(url.query());
        let upstream_url_string = upstream_url.to_string();

        tracing::info!("Seer2 virtual HTTP fetch: {original_url} -> {upstream_url}");
        report_cache_metric(CacheMetricKey::Fetch);
        let upstream_response = client
            .get(upstream_url)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = upstream_response.status().as_u16();
        let modified = response_modified_time(&upstream_response);
        let body = upstream_response
            .bytes()
            .await
            .map_err(|error| error.to_string())?
            .to_vec();

        if status == 200 && self.lock_file(&bloom_path) {
            let server = self.clone();
            let cache_body = body.clone();
            let cache_key = bloom_path.clone();
            drop(tokio::task::spawn_blocking(move || {
                if let Err(error) =
                    server.store_cache_file(&cache_key, &cache_file, &cache_body, modified)
                {
                    tracing::warn!(
                        "Failed to write Seer2 cache file {}: {error}",
                        cache_file.display()
                    );
                }
                server.unlock_file(&cache_key);
            }));
        }

        Ok(response(
            original_url,
            status,
            body,
            source,
            Some(upstream_url_string),
        ))
    }

    async fn ensure_initial_cache_maintenance(&self) {
        let cache_directory = self.cache_directory.clone();
        self.initial_cache_maintenance
            .get_or_init(|| async move {
                match tokio::task::spawn_blocking(move || {
                    prune_cache_directory(&cache_directory, MAX_CACHE_BYTES)
                })
                .await
                {
                    Ok(Ok(result)) if result.removed_files > 0 => tracing::info!(
                        "Pruned {} Seer2 cache files ({} bytes); remaining size: {} bytes",
                        result.removed_files,
                        result.removed_bytes,
                        result.remaining_bytes
                    ),
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => tracing::warn!("Failed to prune Seer2 cache: {error}"),
                    Err(error) => {
                        tracing::warn!("Failed to join initial Seer2 cache maintenance: {error}")
                    }
                }
            })
            .await;
    }

    fn store_cache_file(
        &self,
        cache_key: &str,
        cache_file: &Path,
        body: &[u8],
        modified: Option<SystemTime>,
    ) -> io::Result<()> {
        write_with_cipher(cache_key, cache_file, body, modified)?;
        report_cache_metric(CacheMetricKey::Cached);
        let writes = self.cache_writes.fetch_add(1, Ordering::Relaxed) + 1;
        if writes.is_multiple_of(CACHE_PRUNE_INTERVAL_WRITES) {
            let result = prune_cache_directory(&self.cache_directory, MAX_CACHE_BYTES)?;
            if result.removed_files > 0 {
                tracing::info!(
                    "Pruned {} Seer2 cache files ({} bytes); remaining size: {} bytes",
                    result.removed_files,
                    result.removed_bytes,
                    result.remaining_bytes
                );
            }
        }
        Ok(())
    }

    async fn load_runtime(&self, client: &reqwest::Client) -> Result<Runtime, String> {
        let generation = RUNTIME_CACHE_GENERATION.load(Ordering::SeqCst);
        if let Some(runtime) = self.current_runtime(generation) {
            return Ok(runtime);
        }

        let addresses = tokio::net::lookup_host((ROOT_DNS, 80))
            .await
            .map_err(|error| error.to_string())?;
        let mut first = None;
        let mut first_v4 = None;
        for address in addresses {
            first.get_or_insert(address.ip());
            if address.ip().is_ipv4() {
                first_v4 = Some(address.ip());
                break;
            }
        }
        let root_ip = first_v4
            .or(first)
            .ok_or_else(|| "Version root DNS returned no address".to_string())?;
        let root_url = version_root_url(root_ip).map_err(|error| error.to_string())?;
        let bloom_url = root_url
            .join(BLOOM_PATH.trim_start_matches('/'))
            .map_err(|error| error.to_string())?;

        let bloom_response = client
            .get(bloom_url.clone())
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !bloom_response.status().is_success() {
            return Err(format!(
                "Version manifest request {bloom_url} returned {}",
                bloom_response.status()
            ));
        }
        let bloom_text = bloom_response
            .text()
            .await
            .map_err(|error| error.to_string())?;
        let runtime = Runtime {
            root_url,
            bloom: BloomFilter::parse(&bloom_text)?,
        };

        let mut state = self
            .runtime
            .lock()
            .map_err(|_| "Virtual HTTP runtime lock was poisoned".to_string())?;
        let stored = if state.generation == generation {
            state.value.get_or_insert_with(|| runtime.clone()).clone()
        } else {
            runtime
        };
        tracing::info!("Seer2 virtual HTTP version root: {}", stored.root_url);
        Ok(stored)
    }

    fn current_runtime(&self, generation: u64) -> Option<Runtime> {
        let mut state = self.runtime.lock().ok()?;
        if state.generation != generation {
            state.generation = generation;
            state.value = None;
        }
        state.value.clone()
    }

    fn proxy_file_path(&self, url_path: &str) -> Option<PathBuf> {
        let relative = safe_relative_path(url_path)?;
        Some(self.proxy_root.as_ref()?.join(relative))
    }

    fn cache_file_path(&self, url_path: &str) -> PathBuf {
        self.cache_directory.join(cache_file_name(url_path))
    }

    fn is_file_locked(&self, key: &str) -> bool {
        self.file_locks
            .lock()
            .map_or(true, |locks| locks.contains(key))
    }

    fn lock_file(&self, key: &str) -> bool {
        self.file_locks
            .lock()
            .is_ok_and(|mut locks| locks.insert(key.to_string()))
    }

    fn unlock_file(&self, key: &str) {
        if let Ok(mut locks) = self.file_locks.lock() {
            locks.remove(key);
        }
    }

    fn spawn_cache_check(
        &self,
        client: reqwest::Client,
        bloom_path: String,
        cache_file: PathBuf,
        modified: SystemTime,
    ) {
        let server = self.clone();
        tokio::spawn(async move {
            if let Err(error) = server
                .check_cache(client, bloom_path, cache_file, modified)
                .await
            {
                tracing::warn!("Failed to revalidate Seer2 cache: {error}");
            }
        });
    }

    async fn check_cache(
        &self,
        client: reqwest::Client,
        bloom_path: String,
        cache_file: PathBuf,
        modified: SystemTime,
    ) -> Result<(), String> {
        report_cache_metric(CacheMetricKey::Checked);
        let upstream_url = self
            .legacy_upstream
            .join(bloom_path.trim_start_matches('/'))
            .map_err(|error| error.to_string())?;
        let response = client
            .get(upstream_url)
            .header(IF_MODIFIED_SINCE, httpdate::fmt_http_date(modified))
            .send()
            .await
            .map_err(|error| error.to_string())?;

        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            report_cache_metric(CacheMetricKey::Unchanged);
            return Ok(());
        }
        if response.status() != reqwest::StatusCode::OK {
            return Ok(());
        }

        let new_modified = response_modified_time(&response);
        if new_modified == Some(modified) {
            report_cache_metric(CacheMetricKey::Unchanged);
            return Ok(());
        }
        report_cache_metric(CacheMetricKey::Changed);
        let body = response
            .bytes()
            .await
            .map_err(|error| error.to_string())?
            .to_vec();

        if self.lock_file(&bloom_path) {
            let server = self.clone();
            let cache_key = bloom_path.clone();
            tokio::task::spawn_blocking(move || {
                let result = server
                    .store_cache_file(&cache_key, &cache_file, &body, new_modified)
                    .map_err(|error| error.to_string());
                server.unlock_file(&cache_key);
                result
            })
            .await
            .map_err(|error| error.to_string())??;
        }
        Ok(())
    }
}

impl RequestInterceptor for Seer2VirtualHttp {
    fn intercept(
        &self,
        request: &Request,
        resolved_url: &Url,
        client: Option<reqwest::Client>,
    ) -> Option<OwnedFuture<InterceptedResponse, ErrorResponse>> {
        if !self.handles(resolved_url) {
            return None;
        }

        let server = self.clone();
        let url = resolved_url.clone();
        let request_url = url.to_string();
        let method = request.method();
        let monitor_id = seer2_network_monitor().begin(method.to_string(), request_url.clone());
        let started = Instant::now();
        Some(Box::pin(async move {
            let result = spawn_tokio(async move { server.handle(url, method, client).await }).await;
            let duration = started.elapsed().as_millis();
            match result {
                Ok(result) => {
                    seer2_network_monitor().finish(
                        monitor_id,
                        duration,
                        Some(result.response.status),
                        result.source,
                        Some(result.response.body.len()),
                        result.upstream_url,
                        None,
                    );
                    Ok(result.response)
                }
                Err(error) => {
                    seer2_network_monitor().finish(
                        monitor_id,
                        duration,
                        None,
                        NetworkRequestSource::Error,
                        None,
                        None,
                        Some(error.clone()),
                    );
                    Err(fetch_error(&request_url, error))
                }
            }
        }))
    }
}

impl BloomFilter {
    fn parse(value: &str) -> Result<Self, String> {
        let mut lines = value.lines();
        let _version = lines.next();
        let function_count = lines
            .next()
            .ok_or_else(|| "Bloom filter is missing its function count".to_string())?
            .trim()
            .parse::<usize>()
            .map_err(|error| error.to_string())?;
        let encoded_bits = lines
            .next()
            .ok_or_else(|| "Bloom filter is missing its bit data".to_string())?
            .trim();
        let bits = BASE64
            .decode(encoded_bits)
            .map_err(|error| error.to_string())?;
        if function_count == 0 || bits.is_empty() {
            return Err("Bloom filter is empty".to_string());
        }
        Ok(Self {
            function_count,
            bits,
        })
    }

    fn contains(&self, value: &str) -> bool {
        let digest = md5::compute(value.as_bytes()).0;
        let first = u32::from_be_bytes(digest[0..4].try_into().unwrap_or_default())
            ^ u32::from_be_bytes(digest[4..8].try_into().unwrap_or_default());
        let increment = u32::from_be_bytes(digest[8..12].try_into().unwrap_or_default())
            ^ u32::from_be_bytes(digest[12..16].try_into().unwrap_or_default());
        let bit_count = self.bits.len() * 8;
        let mut hash = first;

        for _ in 0..self.function_count {
            let bit = hash as usize % bit_count;
            if self.bits[bit / 8] & (1 << (bit % 8)) == 0 {
                return false;
            }
            hash = hash.wrapping_add(increment);
        }
        true
    }
}

fn response(
    url: String,
    status: u16,
    body: Vec<u8>,
    source: NetworkRequestSource,
    upstream_url: Option<String>,
) -> Seer2Response {
    Seer2Response {
        response: InterceptedResponse {
            url,
            body,
            text_encoding: None,
            status,
            redirected: false,
        },
        source,
        upstream_url,
    }
}

fn fetch_error(url: &str, error: impl Display) -> ErrorResponse {
    ErrorResponse {
        url: url.to_string(),
        error: Error::FetchError(error.to_string()),
    }
}

fn version_root_url(ip: IpAddr) -> Result<Url, url::ParseError> {
    match ip {
        IpAddr::V4(ip) => Url::parse(&format!("http://{ip}/seer2/")),
        IpAddr::V6(ip) => Url::parse(&format!("http://[{ip}]/seer2/")),
    }
}

fn is_virtual_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(host)) => {
            let host = host.trim_end_matches('.');
            host.eq_ignore_ascii_case("localhost") || host.eq_ignore_ascii_case("seer2.client")
        }
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

fn safe_relative_path(url_path: &str) -> Option<PathBuf> {
    let decoded = urlencoding::decode(url_path).ok()?;
    let mut relative = PathBuf::new();
    for segment in decoded.trim_start_matches(['/', '\\']).split(['/', '\\']) {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." || segment.contains('\0') || segment.contains(':') {
            return None;
        }
        relative.push(segment);
    }
    Some(relative)
}

fn cache_file_name(url_path: &str) -> String {
    let digest = md5::compute(url_path.trim_start_matches('/').as_bytes());
    let javascript_length = url_path.encode_utf16().count();
    format!("{digest:x}_{javascript_length}")
}

fn system_time_millis(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn response_modified_time(response: &reqwest::Response) -> Option<SystemTime> {
    response
        .headers()
        .get(LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| httpdate::parse_http_date(value).ok())
}

fn derive_cache_key(url_path: &str) -> io::Result<[u8; 24]> {
    let parameters = ScryptParams::new(14, 8, 1, 24)
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string()))?;
    let mut key = [0; 24];
    scrypt(url_path.as_bytes(), b"salt", &parameters, &mut key)
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string()))?;
    Ok(key)
}

fn read_with_decipher(url_path: &str, file_path: &Path) -> io::Result<Vec<u8>> {
    let key = derive_cache_key(url_path)?;
    let mut data = fs::read(file_path)?;
    let decrypted = Aes192CbcDecryptor::new(&key.into(), &[0; 16].into())
        .decrypt_padded_mut::<Pkcs7>(&mut data)
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, format!("{error:?}")))?;
    Ok(decrypted.to_vec())
}

fn touch_cache_file(file_path: &Path) {
    if let Ok(file) = OpenOptions::new().write(true).open(file_path) {
        let _ = file.set_times(FileTimes::new().set_accessed(SystemTime::now()));
    }
}

fn write_with_cipher(
    url_path: &str,
    file_path: &Path,
    body: &[u8],
    modified: Option<SystemTime>,
) -> io::Result<()> {
    let key = derive_cache_key(url_path)?;
    let padded_length = (body.len() / 16 + 1) * 16;
    let mut data = vec![0; padded_length];
    data[..body.len()].copy_from_slice(body);
    let encrypted = Aes192CbcEncryptor::new(&key.into(), &[0; 16].into())
        .encrypt_padded_mut::<Pkcs7>(&mut data, body.len())
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, format!("{error:?}")))?;

    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(file_path, encrypted)?;
    if let Some(modified) = modified {
        OpenOptions::new()
            .write(true)
            .open(file_path)?
            .set_times(FileTimes::new().set_modified(modified))?;
    }
    Ok(())
}

#[derive(Default)]
struct CachePruneResult {
    removed_files: usize,
    removed_bytes: u64,
    remaining_bytes: u64,
}

fn prune_cache_directory(directory: &Path, max_bytes: u64) -> io::Result<CachePruneResult> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    let mut total_bytes = 0_u64;

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!("Unable to inspect a Seer2 cache entry: {error}");
                continue;
            }
        };
        if !is_cache_file_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => continue,
            Err(error) => {
                tracing::debug!(
                    "Unable to inspect Seer2 cache file {}: {error}",
                    entry.path().display()
                );
                continue;
            }
        };
        let bytes = metadata.len();
        let last_used = metadata
            .accessed()
            .or_else(|_| metadata.modified())
            .unwrap_or(UNIX_EPOCH);
        total_bytes = total_bytes.saturating_add(bytes);
        files.push((last_used, entry.path(), bytes));
    }

    let mut result = CachePruneResult {
        remaining_bytes: total_bytes,
        ..Default::default()
    };
    if total_bytes <= max_bytes {
        return Ok(result);
    }

    files.sort_unstable_by_key(|(last_used, _, _)| *last_used);
    for (_, path, bytes) in files {
        if result.remaining_bytes <= max_bytes {
            break;
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                result.removed_files += 1;
                result.removed_bytes = result.removed_bytes.saturating_add(bytes);
                result.remaining_bytes = result.remaining_bytes.saturating_sub(bytes);
            }
            Err(error) => tracing::debug!(
                "Unable to remove Seer2 cache file {} during pruning: {error}",
                path.display()
            ),
        }
    }

    Ok(result)
}

fn is_cache_file_name(name: &str) -> bool {
    let Some((digest, length)) = name.split_once('_') else {
        return false;
    };
    digest.len() == 32
        && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !length.is_empty()
        && length.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn electron_cache_filename_is_compatible() {
        assert_eq!(
            cache_file_name("/seer2/Client.swf"),
            "0141acc6d6952f6d8549a7d5e4251b14_17"
        );
    }

    #[test]
    fn encrypted_cache_round_trip() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("cache");
        let expected = b"CWS virtual cache test";
        write_with_cipher("/Client.swf", &path, expected, None).expect("cache write");
        assert_eq!(
            read_with_decipher("/Client.swf", &path).expect("cache read"),
            expected
        );
    }

    #[test]
    fn network_monitor_tracks_completed_requests() {
        let monitor = NetworkMonitor::default();
        let id = monitor.begin("GET".to_string(), "http://seer2.client/test".to_string());
        monitor.finish(
            id,
            12,
            Some(200),
            NetworkRequestSource::Cache,
            Some(42),
            None,
            None,
        );
        let records = monitor.snapshot();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].duration_millis, Some(12));
        assert_eq!(records[0].status, Some(200));
        assert_eq!(records[0].source, NetworkRequestSource::Cache);
        assert_eq!(records[0].response_bytes, Some(42));
    }

    #[test]
    fn cache_metrics_track_all_report_types() {
        let counters = CacheMetricCounters::default();
        for key in [
            CacheMetricKey::Hit,
            CacheMetricKey::Cached,
            CacheMetricKey::Expired,
            CacheMetricKey::Checked,
            CacheMetricKey::Unchanged,
            CacheMetricKey::Changed,
            CacheMetricKey::Proxy,
            CacheMetricKey::Fetch,
        ] {
            counters.report(key);
        }
        assert_eq!(
            counters.snapshot(),
            Seer2CacheMetrics {
                hit: 1,
                cached: 1,
                expired: 1,
                checked: 1,
                unchanged: 1,
                changed: 1,
                proxy: 1,
                fetch: 1,
            }
        );
    }

    #[test]
    fn clearing_file_cache_preserves_unrelated_files() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let cache_file = directory.path().join("00000000000000000000000000000000_1");
        let unrelated = directory.path().join("keep-me.txt");
        fs::write(&cache_file, [0; 8]).expect("cache file");
        fs::write(&unrelated, [0; 3]).expect("unrelated file");

        let result = clear_seer2_file_cache(directory.path()).expect("clear file cache");
        assert_eq!(
            result,
            Seer2FileCacheClearResult {
                removed_files: 1,
                removed_bytes: 8,
                failed_files: 0,
            }
        );
        assert!(!cache_file.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn cache_pruning_keeps_recent_files_under_the_limit() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let old = directory.path().join("00000000000000000000000000000000_1");
        let recent = directory.path().join("11111111111111111111111111111111_1");
        fs::write(&old, [0; 8]).expect("old cache file");
        fs::write(&recent, [0; 8]).expect("recent cache file");
        OpenOptions::new()
            .write(true)
            .open(&old)
            .expect("open old file")
            .set_times(
                FileTimes::new().set_accessed(UNIX_EPOCH + std::time::Duration::from_secs(1)),
            )
            .expect("set old access time");
        OpenOptions::new()
            .write(true)
            .open(&recent)
            .expect("open recent file")
            .set_times(
                FileTimes::new().set_accessed(UNIX_EPOCH + std::time::Duration::from_secs(2)),
            )
            .expect("set recent access time");

        let result = prune_cache_directory(directory.path(), 8).expect("prune cache");
        assert_eq!(result.removed_files, 1);
        assert!(!old.exists());
        assert!(recent.exists());
    }

    #[test]
    fn bloom_filter_uses_little_endian_bits() {
        let full = BloomFilter::parse("v1\n2\n/w==").expect("full bloom filter");
        let empty = BloomFilter::parse("v1\n2\nAA==").expect("empty bloom filter");
        assert!(full.contains("/Client.swf"));
        assert!(!empty.contains("/Client.swf"));
    }

    #[test]
    fn proxy_paths_cannot_escape_the_root() {
        assert_eq!(
            safe_relative_path("/seer2/res/a.swf"),
            Some(PathBuf::from("seer2").join("res").join("a.swf"))
        );
        assert!(safe_relative_path("/seer2/%2e%2e/secret").is_none());
        assert!(safe_relative_path("/seer2/C:/secret").is_none());
    }

    #[test]
    fn only_local_http_hosts_are_intercepted() {
        let initial_url =
            Url::parse("http://127.0.0.1:7337/seer2/Client.swf").expect("initial URL");
        let server = Seer2VirtualHttp::new(&initial_url, PathBuf::from("cache"), None)
            .expect("virtual HTTP");

        assert!(server.handles(&initial_url));
        assert!(server.handles(
            &Url::parse("http://localhost:8123/seer2/config/a.xml").expect("localhost URL")
        ));
        assert!(server.handles(
            &Url::parse("http://seer2.client/seer2/config/a.xml").expect("virtual host URL")
        ));
        assert!(
            !server
                .handles(&Url::parse("http://example.com/seer2/Client.swf").expect("remote URL"))
        );
        assert!(
            !server.handles(&Url::parse("https://127.0.0.1/seer2/Client.swf").expect("HTTPS URL"))
        );

        let remote_url =
            Url::parse("http://example.com/seer2/Client.swf").expect("remote initial URL");
        assert!(Seer2VirtualHttp::new(&remote_url, PathBuf::from("cache"), None).is_err());
    }
}
