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
use std::fmt::Display;
use std::fs::{self, FileTimes, OpenOptions};
use std::io::{self, ErrorKind};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use url::{Host, Url};

const SEER2_PATH: &str = "/seer2";
const BLOOM_PATH: &str = "/config/bloom-path.data";
const MAGIC_PATH: &str = "/seer2-next-client-hello";
const FLASH_POLICY_PATH: &str = "/crossdomain.xml";
const ROOT_DNS: &str = "next-client-root.733702.xyz";
const LEGACY_UPSTREAM: &str = "http://seer2.61.com/";
const FLASH_POLICY_DATA: &str = concat!(
    r#"<?xml version="1.0"?>"#,
    r#"<!DOCTYPE cross-domain-policy SYSTEM "#,
    r#""http://www.macromedia.com/xml/dtds/cross-domain-policy.dtd">"#,
    r#"<cross-domain-policy><allow-access-from domain="*" /></cross-domain-policy>"#
);

type Aes192CbcEncryptor = cbc::Encryptor<Aes192>;
type Aes192CbcDecryptor = cbc::Decryptor<Aes192>;

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
    runtime: Arc<Mutex<Option<Runtime>>>,
    file_locks: Arc<Mutex<HashSet<String>>>,
}

#[derive(Clone)]
struct Runtime {
    root_url: Url,
    bloom: BloomFilter,
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

        Ok(Self {
            proxy_root,
            cache_directory,
            legacy_upstream: Url::parse(LEGACY_UPSTREAM).map_err(|error| error.to_string())?,
            runtime: Arc::new(Mutex::new(None)),
            file_locks: Arc::new(Mutex::new(HashSet::new())),
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
    ) -> Result<InterceptedResponse, String> {
        let original_url = url.to_string();
        let url_path = url.path().to_string();
        tracing::debug!("Seer2 virtual HTTP {method} {original_url}");

        if url_path == MAGIC_PATH {
            let body = format!(r#"{{"version":"{}"}}"#, env!("CARGO_PKG_VERSION")).into_bytes();
            return Ok(response(original_url, 200, body));
        }

        if url_path == FLASH_POLICY_PATH {
            return Ok(response(
                original_url,
                200,
                FLASH_POLICY_DATA.as_bytes().to_vec(),
            ));
        }

        if url_path.ends_with('/') || url_path.ends_with('\\') || !url_path.starts_with("/seer2/") {
            return Ok(response(original_url, 403, b"not a valid path".to_vec()));
        }

        if let Some(file_path) = self.proxy_file_path(&url_path)
            && file_path.is_file()
        {
            match fs::read(&file_path) {
                Ok(body) => {
                    tracing::info!("Seer2 virtual HTTP proxy file: {url_path}");
                    return Ok(response(original_url, 200, body));
                }
                Err(error) => {
                    tracing::warn!(
                        "Failed to read Seer2 proxy file {}: {error}",
                        file_path.display()
                    );
                }
            }
        }

        let bloom_path = url_path
            .strip_prefix(SEER2_PATH)
            .unwrap_or_default()
            .to_string();
        if bloom_path == BLOOM_PATH {
            return Ok(response(original_url, 403, Vec::new()));
        }

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
                match read_with_decipher(&bloom_path, &cache_file) {
                    Ok(body) => {
                        tracing::info!("Seer2 virtual HTTP cache hit: {url_path}");
                        if !path_hit_bloom && let Some(modified) = modified {
                            self.spawn_cache_check(
                                client.clone(),
                                bloom_path.clone(),
                                cache_file,
                                modified,
                            );
                        }
                        return Ok(response(original_url, 200, body));
                    }
                    Err(error) => tracing::warn!(
                        "Failed to read Seer2 cache file {}: {error}",
                        cache_file.display()
                    ),
                }
            } else {
                tracing::info!("Seer2 virtual HTTP cache expired: {url_path}");
            }
        }

        let mut upstream_url = if path_hit_bloom {
            runtime.root_url.join(bloom_path.trim_start_matches('/'))
        } else {
            self.legacy_upstream
                .join(bloom_path.trim_start_matches('/'))
        }
        .map_err(|error| error.to_string())?;
        upstream_url.set_query(url.query());

        tracing::info!("Seer2 virtual HTTP fetch: {original_url} -> {upstream_url}");
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
                    write_with_cipher(&cache_key, &cache_file, &cache_body, modified)
                {
                    tracing::warn!(
                        "Failed to write Seer2 cache file {}: {error}",
                        cache_file.display()
                    );
                }
                server.unlock_file(&cache_key);
            }));
        }

        Ok(response(original_url, status, body))
    }

    async fn load_runtime(&self, client: &reqwest::Client) -> Result<Runtime, String> {
        if let Some(runtime) = self.current_runtime() {
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
        let stored = state.get_or_insert_with(|| runtime.clone()).clone();
        tracing::info!("Seer2 virtual HTTP version root: {}", stored.root_url);
        Ok(stored)
    }

    fn current_runtime(&self) -> Option<Runtime> {
        self.runtime.lock().ok()?.clone()
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
            return Ok(());
        }
        if response.status() != reqwest::StatusCode::OK {
            return Ok(());
        }

        let new_modified = response_modified_time(&response);
        if new_modified == Some(modified) {
            return Ok(());
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| error.to_string())?
            .to_vec();

        if self.lock_file(&bloom_path) {
            let result = write_with_cipher(&bloom_path, &cache_file, &body, new_modified)
                .map_err(|error| error.to_string());
            self.unlock_file(&bloom_path);
            result?;
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
        Some(Box::pin(async move {
            spawn_tokio(async move { server.handle(url, method, client).await })
                .await
                .map_err(|error| fetch_error(&request_url, error))
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

fn response(url: String, status: u16, body: Vec<u8>) -> InterceptedResponse {
    InterceptedResponse {
        url,
        body,
        text_encoding: None,
        status,
        redirected: false,
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
