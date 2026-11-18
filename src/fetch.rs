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

/// Page fetch failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    Http(String),
    Status(u16),
    UnsupportedContentType(String),
    Blocked(String),
    Empty(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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

    /// Fetch a page as plain text, or serve it from the cache.
    pub async fn get_text(&self, url: &str) -> Result<FetchOutcome, FetchError> {
        self.stats.lock().expect("stats mutex poisoned").requested += 1;
        if let Some(page) = self.cache.get(url) {
            self.stats.lock().expect("stats mutex poisoned").from_cache += 1;
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
        self.check_blocked(&domain)?;
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
                self.record_failure(&domain);
                self.stats.lock().expect("stats mutex poisoned").failed += 1;
                return Err(FetchError::Http(format!("{url}: {error}")));
            }
        };
        let status = response.status();
        if !status.is_success() {
            self.stats.lock().expect("stats mutex poisoned").failed += 1;
            if status.as_u16() == 429 {
                self.block_domain(&domain, RATE_LIMIT_BLOCK);
            } else {
                self.record_failure(&domain);
            }
            return Err(FetchError::Status(status.as_u16()));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("text/html")
            .to_string();
        if !is_supported_content_type(&content_type) {
            self.stats.lock().expect("stats mutex poisoned").failed += 1;
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
                    self.record_failure(&domain);
                    self.stats.lock().expect("stats mutex poisoned").failed += 1;
                    return Err(FetchError::Http(format!("{url}: read body: {error}")));
                }
            }
        }
        if buffer.is_empty() {
            self.record_failure(&domain);
            self.stats.lock().expect("stats mutex poisoned").failed += 1;
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
            self.stats.lock().expect("stats mutex poisoned").failed += 1;
            return Err(FetchError::Empty("extracted text too short".to_string()));
        }
        self.record_success(&domain);
        self.stats.lock().expect("stats mutex poisoned").fetched += 1;
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
}
