//! Rate-limited, bounded HTTP fetcher with disk cache and failure backoff.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use tokio::sync::Semaphore;

use crate::cache::{Cache, CachedPage};
use crate::config::Config;
use crate::html;
use crate::log;
use crate::util::{domain_of, truncate_chars};

pub const MAX_REDIRECTS: usize = 5;
const FAILURES_BEFORE_BLOCK: u32 = 2;
const BLOCK_DURATION: Duration = Duration::from_secs(600);
const RATE_LIMIT_BLOCK: Duration = Duration::from_secs(1800);
/// One immediate retry for transient failures (timeout/connect/5xx/429).
const RETRY_ATTEMPTS: usize = 2;
const RETRY_BACKOFF: Duration = Duration::from_millis(500);

/// Page fetch failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    Timeout(String),
    Connect(String),
    Http(String),
    Status(u16),
    UnsupportedContentType(String),
    Blocked(String),
    Empty(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Timeout(message) => write!(f, "timeout: {message}"),
            FetchError::Connect(message) => write!(f, "connect error: {message}"),
            FetchError::Http(message) => write!(f, "http error: {message}"),
            FetchError::Status(status) => write!(f, "http status {status}"),
            FetchError::UnsupportedContentType(content_type) => {
                write!(f, "unsupported content type: {content_type}")
            }
            FetchError::Blocked(domain) => write!(f, "domain temporarily blocked: {domain}"),
            FetchError::Empty(message) => write!(f, "empty page: {message}"),
        }
    }
}

impl std::error::Error for FetchError {}

/// Failure bucket used in run statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchFailureCategory {
    Timeout,
    Http,
    Other,
}

impl FetchError {
    fn from_reqwest(context: &str, error: reqwest::Error) -> FetchError {
        let message = format!("{context}: {error}");
        if error.is_timeout() {
            FetchError::Timeout(message)
        } else if error.is_connect() {
            FetchError::Connect(message)
        } else {
            FetchError::Http(message)
        }
    }

    /// Retry once on transient network errors, 5xx and 429.
    pub fn is_retryable(&self) -> bool {
        match self {
            FetchError::Timeout(_) | FetchError::Connect(_) | FetchError::Http(_) => true,
            FetchError::Status(status) => *status >= 500 || *status == 429,
            FetchError::UnsupportedContentType(_)
            | FetchError::Blocked(_)
            | FetchError::Empty(_) => false,
        }
    }

    fn failure_category(&self) -> FetchFailureCategory {
        match self {
            FetchError::Timeout(_) => FetchFailureCategory::Timeout,
            FetchError::Status(_) => FetchFailureCategory::Http,
            FetchError::Connect(_)
            | FetchError::Http(_)
            | FetchError::UnsupportedContentType(_)
            | FetchError::Blocked(_)
            | FetchError::Empty(_) => FetchFailureCategory::Other,
        }
    }
}

/// Successful fetch result.
#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub url: String,
    pub final_url: String,
    pub text: String,
    pub from_cache: bool,
    pub status: u16,
    pub truncated: bool,
    pub bytes: usize,
}

/// Aggregated fetch counters for one run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchStats {
    pub requested: usize,
    pub fetched: usize,
    pub from_cache: usize,
    pub failed: usize,
    /// Extra requests made by the one-shot retry.
    pub retried: usize,
    pub failures_timeout: usize,
    pub failures_http: usize,
    pub failures_other: usize,
}

impl FetchStats {
    /// Counters added since an earlier snapshot (used for per-run summaries).
    pub fn delta_since(&self, before: &FetchStats) -> FetchStats {
        FetchStats {
            requested: self.requested.saturating_sub(before.requested),
            fetched: self.fetched.saturating_sub(before.fetched),
            from_cache: self.from_cache.saturating_sub(before.from_cache),
            failed: self.failed.saturating_sub(before.failed),
            retried: self.retried.saturating_sub(before.retried),
            failures_timeout: self
                .failures_timeout
                .saturating_sub(before.failures_timeout),
            failures_http: self.failures_http.saturating_sub(before.failures_http),
            failures_other: self.failures_other.saturating_sub(before.failures_other),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct DomainState {
    failures: u32,
    blocked_until: Option<Instant>,
}

/// Bounded HTTP client used for schedule pages.
pub struct Fetcher {
    client: reqwest::Client,
    semaphore: Arc<Semaphore>,
    min_interval: Duration,
    last_request: tokio::sync::Mutex<Option<Instant>>,
    cache: Arc<Cache>,
    max_bytes: usize,
    domains: Mutex<HashMap<String, DomainState>>,
    stats: Mutex<FetchStats>,
}

impl Fetcher {
    pub fn new(cfg: &Config, cache: Arc<Cache>) -> Result<Fetcher, FetchError> {
        let client = reqwest::Client::builder()
            .user_agent(&cfg.fetch_ua)
            .timeout(Duration::from_secs(cfg.fetch_timeout_secs))
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .gzip(true)
            .build()
            .map_err(|error| FetchError::Http(format!("build http client: {error}")))?;
        Ok(Fetcher {
            client,
            semaphore: Arc::new(Semaphore::new(cfg.fetch_concurrency.max(1))),
            min_interval: Duration::from_millis(cfg.fetch_min_interval_ms),
            last_request: tokio::sync::Mutex::new(None),
            cache,
            max_bytes: cfg.fetch_max_bytes.max(1),
            domains: Mutex::new(HashMap::new()),
            stats: Mutex::new(FetchStats::default()),
        })
    }

    pub fn stats(&self) -> FetchStats {
        self.stats
            .lock()
            .expect("fetch stats mutex poisoned")
            .clone()
    }

    pub fn cache(&self) -> &Arc<Cache> {
        &self.cache
    }

    fn update_stats(&self, update: impl FnOnce(&mut FetchStats)) {
        let mut stats = self.stats.lock().expect("fetch stats mutex poisoned");
        update(&mut stats);
    }

    fn record_failure_category(&self, error: &FetchError) {
        let category = error.failure_category();
        self.update_stats(|stats| match category {
            FetchFailureCategory::Timeout => stats.failures_timeout += 1,
            FetchFailureCategory::Http => stats.failures_http += 1,
            FetchFailureCategory::Other => stats.failures_other += 1,
        });
    }

    /// Fetch a page as plain text, or serve it from the cache.
    ///
    /// Transient failures (timeout/connect/5xx/429) get exactly one retry;
    /// permanent errors are returned immediately. Failure categories are
    /// counted in FetchStats for the run summary and /api/status.
    pub async fn get_text(&self, url: &str) -> Result<FetchOutcome, FetchError> {
        self.update_stats(|stats| stats.requested += 1);
        if let Some(page) = self.cache.get(url) {
            self.update_stats(|stats| stats.from_cache += 1);
            return Ok(FetchOutcome {
                url: page.url,
                final_url: page.final_url.unwrap_or_else(|| url.to_string()),
                text: page.text,
                from_cache: true,
                status: page.status,
                truncated: page.truncated,
                bytes: page.raw_bytes,
            });
        }
        let domain = domain_of(url);
        let mut last_error: Option<FetchError> = None;
        for attempt in 0..RETRY_ATTEMPTS {
            if attempt > 0 {
                self.update_stats(|stats| stats.retried += 1);
                log::debug(format!("retrying fetch for {url} after transient failure"));
                tokio::time::sleep(RETRY_BACKOFF).await;
            }
            match self.fetch_once(url, &domain).await {
                Ok(outcome) => {
                    self.update_stats(|stats| stats.fetched += 1);
                    self.record_success(&domain);
                    return Ok(outcome);
                }
                Err(error) => {
                    let will_retry = attempt + 1 < RETRY_ATTEMPTS && error.is_retryable();
                    if will_retry {
                        last_error = Some(error);
                        continue;
                    }
                    self.update_stats(|stats| stats.failed += 1);
                    self.record_failure_category(&error);
                    match &error {
                        FetchError::Blocked(_) => {}
                        FetchError::Status(429) => self.block_domain(&domain, RATE_LIMIT_BLOCK),
                        _ => self.record_failure(&domain),
                    }
                    return Err(error);
                }
            }
        }
        let error = last_error.unwrap_or_else(|| FetchError::Http("fetch failed".to_string()));
        self.update_stats(|stats| stats.failed += 1);
        self.record_failure_category(&error);
        self.record_failure(&domain);
        Err(error)
    }

    /// One network attempt (permit + throttle + request + parse + cache write).
    async fn fetch_once(&self, url: &str, domain: &str) -> Result<FetchOutcome, FetchError> {
        self.check_blocked(domain)?;
        let permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| FetchError::Http("fetch semaphore closed".to_string()))?;
        self.throttle().await;
        let response = self
            .client
            .get(url)
            .header(
                ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,text/plain;q=0.8,*/*;q=0.5",
            )
            .send()
            .await;
        drop(permit);
        let mut response = match response {
            Ok(response) => response,
            Err(error) => {
                return Err(FetchError::from_reqwest(
                    &format!("{url}: request failed"),
                    error,
                ))
            }
        };
        let status = response.status();
        if !status.is_success() {
            return Err(FetchError::Status(status.as_u16()));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("text/html")
            .to_string();
        if !is_supported_content_type(&content_type) {
            return Err(FetchError::UnsupportedContentType(content_type));
        }
        let final_url = response.url().to_string();
        let mut buffer: Vec<u8> = Vec::new();
        let mut truncated = false;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if buffer.len() + chunk.len() > self.max_bytes {
                        let room = self.max_bytes.saturating_sub(buffer.len());
                        buffer.extend_from_slice(&chunk[..room]);
                        truncated = true;
                        break;
                    }
                    buffer.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(error) => {
                    return Err(FetchError::from_reqwest(
                        &format!("{url}: read body"),
                        error,
                    ))
                }
            }
        }
        if buffer.is_empty() {
            return Err(FetchError::Empty("zero bytes".to_string()));
        }
        let raw_bytes = buffer.len();
        let decoded = decode_body(&buffer, &content_type);
        let text = if content_type.to_ascii_lowercase().contains("text/plain") {
            decoded
        } else {
            html::to_text(&decoded)
        };
        let text = truncate_chars(&text, html::DEFAULT_TEXT_LIMIT).to_string();
        if text.trim().len() < 16 {
            return Err(FetchError::Empty("extracted text too short".to_string()));
        }
        let page = CachedPage {
            url: url.to_string(),
            key: crate::util::hash_hex(url),
            fetched_at: Utc::now(),
            status: status.as_u16(),
            content_type: content_type.clone(),
            text: text.clone(),
            raw_bytes,
            truncated,
            final_url: Some(final_url.clone()),
        };
        if let Err(error) = self.cache.put(page) {
            log::warn(format!("cache write failed for {url}: {error}"));
        }
        Ok(FetchOutcome {
            url: url.to_string(),
            final_url,
            text,
            from_cache: false,
            status: status.as_u16(),
            truncated,
            bytes: raw_bytes,
        })
    }

    async fn throttle(&self) {
        if self.min_interval.is_zero() {
            return;
        }
        let mut last = self.last_request.lock().await;
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if elapsed < self.min_interval {
                tokio::time::sleep(self.min_interval - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }

    fn check_blocked(&self, domain: &str) -> Result<(), FetchError> {
        let domains = self.domains.lock().expect("domain mutex poisoned");
        if let Some(state) = domains.get(domain) {
            if let Some(until) = state.blocked_until {
                if until > Instant::now() {
                    return Err(FetchError::Blocked(domain.to_string()));
                }
            }
        }
        Ok(())
    }

    fn record_failure(&self, domain: &str) {
        let mut domains = self.domains.lock().expect("domain mutex poisoned");
        let state = domains.entry(domain.to_string()).or_default();
        state.failures += 1;
        if state.failures >= FAILURES_BEFORE_BLOCK {
            state.blocked_until = Some(Instant::now() + BLOCK_DURATION);
            log::warn(format!(
                "domain {domain} failed {} times, pausing fetches for {}s",
                state.failures,
                BLOCK_DURATION.as_secs()
            ));
        }
    }

    fn block_domain(&self, domain: &str, duration: Duration) {
        let mut domains = self.domains.lock().expect("domain mutex poisoned");
        let state = domains.entry(domain.to_string()).or_default();
        state.blocked_until = Some(Instant::now() + duration);
        log::warn(format!(
            "domain {domain} rate limited, pausing fetches for {}s",
            duration.as_secs()
        ));
    }

    fn record_success(&self, domain: &str) {
        let mut domains = self.domains.lock().expect("domain mutex poisoned");
        domains.remove(domain);
    }
}

/// Content types accepted for schedule extraction.
pub fn is_supported_content_type(content_type: &str) -> bool {
    let content_type = content_type.trim().to_ascii_lowercase();
    content_type.is_empty()
        || content_type.starts_with("text/html")
        || content_type.starts_with("application/xhtml")
        || content_type.starts_with("text/plain")
        || content_type.starts_with("application/xml")
        || content_type.starts_with("text/xml")
}

fn charset_from_content_type(content_type: &str) -> Option<String> {
    let media_type = content_type.to_ascii_lowercase();
    media_type.split(';').skip(1).find_map(|part| {
        let (key, value) = part.split_once('=')?;
        if key.trim().eq_ignore_ascii_case("charset") {
            Some(value.trim().trim_matches('"').to_string())
        } else {
            None
        }
    })
}

fn sniff_charset(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(4096)];
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    let index = text.find("charset=")?;
    let rest = &text[index + "charset=".len()..];
    let rest = rest.trim_start_matches(|ch: char| matches!(ch, '"' | '\'' | ' ' | '\t'));
    let value: String = rest
        .chars()
        .take_while(|ch| !matches!(*ch, '"' | '\'' | ';' | ' ' | '\t' | '\r' | '\n' | '>'))
        .collect();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Decode a page body.
///
/// Valid UTF-8 bytes always win (some servers mislabel their charset). Otherwise
/// the declared charset is tried first, then common Japanese/Chinese encodings;
/// the first candidate without replacement characters is used.
pub fn decode_body(bytes: &[u8], content_type: &str) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    let mut candidates: Vec<String> = Vec::new();
    if let Some(declared) = charset_from_content_type(content_type).or_else(|| sniff_charset(bytes))
    {
        candidates.push(declared);
    }
    for fallback in ["utf-8", "shift_jis", "euc-jp", "gb18030"] {
        if !candidates
            .iter()
            .any(|value| value.eq_ignore_ascii_case(fallback))
        {
            candidates.push(fallback.to_string());
        }
    }
    for charset in &candidates {
        let Some(encoding) = encoding_rs::Encoding::for_label(charset.as_bytes()) else {
            continue;
        };
        let (decoded, _, had_errors) = encoding.decode(bytes);
        let text = decoded.into_owned();
        if !had_errors && !text.contains('�') {
            return text;
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_content_types() {
        assert!(is_supported_content_type("text/html; charset=utf-8"));
        assert!(is_supported_content_type("application/xhtml+xml"));
        assert!(is_supported_content_type("text/plain"));
        assert!(is_supported_content_type(""));
        assert!(!is_supported_content_type("image/png"));
        assert!(!is_supported_content_type("application/pdf"));
    }

    #[test]
    fn charset_from_header() {
        assert_eq!(
            charset_from_content_type("text/html; charset=Shift_JIS"),
            Some("shift_jis".to_string())
        );
        assert_eq!(
            charset_from_content_type("text/html; charset=\"utf-8\""),
            Some("utf-8".to_string())
        );
        assert_eq!(charset_from_content_type("text/html"), None);
    }

    #[test]
    fn detects_meta_charset() {
        let bytes = b"<html><head><meta charset=\"EUC-JP\"></head></html>";
        assert_eq!(sniff_charset(bytes), Some("euc-jp".to_string()));
        assert_eq!(sniff_charset(b"<html></html>"), None);
    }

    #[test]
    fn decodes_utf8() {
        let text = decode_body(
            "<p>毎週土曜 23:30</p>".as_bytes(),
            "text/html; charset=utf-8",
        );
        assert!(text.contains("毎週土曜 23:30"));
    }

    #[test]
    fn decodes_shift_jis() {
        let text = "毎週土曜 23:30 放送";
        let bytes = encoding_rs::SHIFT_JIS.encode(text).0;
        let decoded = decode_body(&bytes, "text/html; charset=shift_jis");
        assert!(decoded.contains("毎週土曜"));
        // Undeclared Shift_JIS is detected by the fallback chain.
        let sniffed = decode_body(&bytes, "text/html");
        assert!(sniffed.contains("毎週土曜"), "sniffed: {sniffed:?}");
    }

    #[test]
    fn decodes_mislabeled_shift_jis() {
        let text = "毎週水曜 24:30 放送";
        let bytes = encoding_rs::SHIFT_JIS.encode(text).0;
        // A server that claims UTF-8 but sends Shift_JIS must not produce
        // replacement characters.
        let decoded = decode_body(&bytes, "text/html; charset=utf-8");
        assert!(decoded.contains("毎週水曜"), "decoded: {decoded:?}");
        assert!(!decoded.contains('�'));
    }

    #[test]
    fn valid_utf8_wins_over_wrong_header() {
        let text = "<p>毎週金曜 22:00</p>";
        let decoded = decode_body(text.as_bytes(), "text/html; charset=shift_jis");
        assert_eq!(decoded, text);
    }

    #[test]
    fn decodes_euc_jp() {
        let text = "毎週木曜 21:00 放送";
        let bytes = encoding_rs::EUC_JP.encode(text).0;
        let decoded = decode_body(&bytes, "text/html; charset=euc-jp");
        assert!(decoded.contains("毎週木曜"));
    }

    #[test]
    fn fetcher_defaults_are_sane() {
        let cfg = crate::config::Config::for_tests(std::path::PathBuf::from("./data-test"));
        let cache = Arc::new(Cache::new(
            std::path::PathBuf::from("./data-test/cache"),
            1024,
            None,
        ));
        let fetcher = Fetcher::new(&cfg, cache).unwrap();
        assert_eq!(fetcher.stats(), FetchStats::default());
        assert_eq!(fetcher.max_bytes, 204_800);
    }

    #[test]
    fn classifies_and_retries_transient_errors() {
        assert!(FetchError::Timeout("t".to_string()).is_retryable());
        assert!(FetchError::Connect("c".to_string()).is_retryable());
        assert!(FetchError::Http("h".to_string()).is_retryable());
        assert!(FetchError::Status(500).is_retryable());
        assert!(FetchError::Status(503).is_retryable());
        assert!(FetchError::Status(429).is_retryable());
        assert!(!FetchError::Status(404).is_retryable());
        assert!(!FetchError::Empty("e".to_string()).is_retryable());
        assert!(!FetchError::Blocked("example.com".to_string()).is_retryable());

        assert_eq!(
            FetchError::Timeout("t".to_string()).failure_category(),
            FetchFailureCategory::Timeout
        );
        assert_eq!(
            FetchError::Status(503).failure_category(),
            FetchFailureCategory::Http
        );
        assert_eq!(
            FetchError::Empty("e".to_string()).failure_category(),
            FetchFailureCategory::Other
        );

        let before = FetchStats {
            requested: 1,
            fetched: 1,
            ..FetchStats::default()
        };
        let after = FetchStats {
            requested: 4,
            fetched: 2,
            retried: 1,
            failed: 1,
            failures_http: 1,
            ..FetchStats::default()
        };
        let delta = after.delta_since(&before);
        assert_eq!(delta.requested, 3);
        assert_eq!(delta.fetched, 1);
        assert_eq!(delta.retried, 1);
        assert_eq!(delta.failed, 1);
        assert_eq!(delta.failures_http, 1);
        assert_eq!(delta.failures_timeout, 0);
    }

    fn spawn_http_server(
        responses: Vec<(u16, &'static str)>,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                    }
                }
                requests.push(String::from_utf8_lossy(&buffer).to_string());
                let reason = match status {
                    200 => "OK",
                    404 => "Not Found",
                    429 => "Too Many Requests",
                    500 => "Internal Server Error",
                    503 => "Service Unavailable",
                    _ => "Unknown",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
            requests
        });
        (address, handle)
    }

    fn test_fetcher(dir: &tempfile::TempDir) -> Fetcher {
        let mut cfg = crate::config::Config::for_tests(dir.path().to_path_buf());
        cfg.fetch_timeout_secs = 2;
        cfg.fetch_concurrency = 1;
        cfg.fetch_min_interval_ms = 0;
        let cache = Arc::new(Cache::new(dir.path().join("cache"), 1024 * 1024, None));
        Fetcher::new(&cfg, cache).unwrap()
    }

    #[tokio::test]
    async fn retries_once_on_5xx_then_succeeds() {
        let (address, server) = spawn_http_server(vec![
            (500, "oops"),
            (
                200,
                "<html><body><h1>Schedule</h1><p>毎週土曜 23:30 放送</p></body></html>",
            ),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let fetcher = test_fetcher(&dir);
        let outcome = fetcher
            .get_text(&format!("http://{address}/schedule"))
            .await
            .unwrap();
        assert!(outcome.text.contains("毎週土曜"));
        let stats = fetcher.stats();
        assert_eq!(stats.requested, 1);
        assert_eq!(stats.retried, 1);
        assert_eq!(stats.fetched, 1);
        assert_eq!(stats.failed, 0);
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn counts_http_failures_after_retry() {
        let (address, server) = spawn_http_server(vec![(500, "oops"), (500, "still down")]);
        let dir = tempfile::tempdir().unwrap();
        let fetcher = test_fetcher(&dir);
        let error = fetcher
            .get_text(&format!("http://{address}/schedule"))
            .await
            .unwrap_err();
        assert_eq!(error, FetchError::Status(500));
        let stats = fetcher.stats();
        assert_eq!(stats.requested, 1);
        assert_eq!(stats.retried, 1);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.failures_http, 1);
        assert_eq!(stats.failures_timeout, 0);
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn does_not_retry_404() {
        let (address, server) = spawn_http_server(vec![(404, "missing")]);
        let dir = tempfile::tempdir().unwrap();
        let fetcher = test_fetcher(&dir);
        let error = fetcher
            .get_text(&format!("http://{address}/missing"))
            .await
            .unwrap_err();
        assert_eq!(error, FetchError::Status(404));
        let stats = fetcher.stats();
        assert_eq!(stats.retried, 0);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.failures_http, 1);
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
