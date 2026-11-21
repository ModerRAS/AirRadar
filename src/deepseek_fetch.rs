//! DeepSeek official server-side page fetch fallback.
//!
//! When the local HTTP fetch fails (anti-bot, TLS, region blocking, empty JS
//! shell), AirRadar can ask the DeepSeek Tools Fetch endpoint to retrieve the
//! page server-side: POST https://api.deepseek.com/v1/tools/fetch with
//! {"url": "..."} and Bearer auth. The endpoint returns the page title and
//! Markdown text. Budget and 5 QPS friendly throttling are enforced locally.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::Config;
use crate::model::DeepseekFetchStat;
use crate::util::{collapse_ws, truncate_chars};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MIN_INTERVAL: Duration = Duration::from_millis(200);
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(5);
const MAX_TEXT_CHARS: usize = 60_000;

/// One server-side fetched page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepseekFetchPage {
    pub title: String,
    pub text: String,
}

impl DeepseekFetchPage {
    /// Title + Markdown text used for extraction/LLM evidence.
    pub fn corpus(&self) -> String {
        if self.title.trim().is_empty() {
            self.text.clone()
        } else {
            format!("{} {}", self.title, self.text)
        }
    }
}

#[derive(Debug, Clone, Default)]
struct RunCounters {
    calls: usize,
    successes: usize,
    failures: usize,
    rate_limited: usize,
    budget_exhausted: usize,
    last_error: Option<String>,
}

/// Client for the DeepSeek official Tools Fetch API.
pub struct DeepseekFetchClient {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    max_calls: usize,
    used: AtomicUsize,
    last_request: tokio::sync::Mutex<Option<Instant>>,
    cooldown_until: Mutex<Option<Instant>>,
    counters: Mutex<RunCounters>,
}

impl DeepseekFetchClient {
    pub fn from_config(cfg: &Config) -> Result<Option<DeepseekFetchClient>, String> {
        let Some(api_key) = cfg.deepseek_api_key.clone() else {
            return Ok(None);
        };
        DeepseekFetchClient::new(
            api_key,
            cfg.deepseek_fetch_url.clone(),
            cfg.max_deepseek_fetch_calls,
            &cfg.fetch_ua,
            DEFAULT_TIMEOUT_SECS,
        )
        .map(Some)
    }

    pub fn new(
        api_key: String,
        endpoint: String,
        max_calls: usize,
        user_agent: &str,
        timeout_secs: u64,
    ) -> Result<DeepseekFetchClient, String> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .timeout(Duration::from_secs(timeout_secs.max(1)))
            .build()
            .map_err(|error| format!("build deepseek fetch client: {error}"))?;
        Ok(DeepseekFetchClient {
            client,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            api_key,
            max_calls,
            used: AtomicUsize::new(0),
            last_request: tokio::sync::Mutex::new(None),
            cooldown_until: Mutex::new(None),
            counters: Mutex::new(RunCounters::default()),
        })
    }

    /// Reset budget and counters at the start of a collection run.
    pub fn reset_run(&self) {
        self.used.store(0, Ordering::SeqCst);
        *self.cooldown_until.lock().expect("cooldown mutex poisoned") = None;
        *self
            .counters
            .lock()
            .expect("deepseek counters mutex poisoned") = RunCounters::default();
    }

    pub fn stats(&self) -> DeepseekFetchStat {
        let counters = self
            .counters
            .lock()
            .expect("deepseek counters mutex poisoned")
            .clone();
        DeepseekFetchStat {
            configured: true,
            calls: counters.calls,
            successes: counters.successes,
            failures: counters.failures,
            rate_limited: counters.rate_limited,
            budget_exhausted: counters.budget_exhausted,
            last_error: counters.last_error,
        }
    }

    pub fn remaining_budget(&self) -> usize {
        self.max_calls
            .saturating_sub(self.used.load(Ordering::SeqCst))
    }

    /// Fetch one page server-side. Never panics; all errors are returned as
    /// strings and counted in stats.
    pub async fn fetch(&self, url: &str) -> Result<DeepseekFetchPage, String> {
        if let Some(until) = *self.cooldown_until.lock().expect("cooldown mutex poisoned") {
            if until > Instant::now() {
                self.note_failure("rate limited (cooldown)", true);
                return Err("deepseek fetch: rate limit cooldown active".to_string());
            }
        }
        let slot = self
            .used
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                (current < self.max_calls).then_some(current + 1)
            });
        if slot.is_err() {
            let mut counters = self
                .counters
                .lock()
                .expect("deepseek counters mutex poisoned");
            counters.budget_exhausted += 1;
            counters.last_error = Some("deepseek fetch budget exhausted".to_string());
            return Err("deepseek fetch: budget exhausted".to_string());
        }
        {
            let mut counters = self
                .counters
                .lock()
                .expect("deepseek counters mutex poisoned");
            counters.calls += 1;
        }
        self.throttle().await;
        let response = self
            .client
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Accept", "application/json")
            .json(&json!({ "url": url }))
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                self.note_failure(&format!("request failed: {error}"), false);
                return Err(format!("deepseek fetch request failed: {error}"));
            }
        };
        let status = response.status();
        if status.as_u16() == 429 {
            *self.cooldown_until.lock().expect("cooldown mutex poisoned") =
                Some(Instant::now() + RATE_LIMIT_COOLDOWN);
            self.note_failure("HTTP 429 (rate limited)", true);
            return Err("deepseek fetch: HTTP 429 rate limited".to_string());
        }
        if !status.is_success() {
            self.note_failure(&format!("HTTP {}", status.as_u16()), false);
            return Err(format!("deepseek fetch: HTTP {}", status.as_u16()));
        }
        let body = match response.text().await {
            Ok(body) => body,
            Err(error) => {
                self.note_failure(&format!("read body: {error}"), false);
                return Err(format!("deepseek fetch read body failed: {error}"));
            }
        };
        let value: Value = match serde_json::from_str(&body) {
            Ok(value) => value,
            Err(error) => {
                self.note_failure(&format!("invalid JSON: {error}"), false);
                return Err(format!("deepseek fetch invalid JSON: {error}"));
            }
        };
        match parse_deepseek_fetch_response(&value) {
            Some(page) => {
                let mut counters = self
                    .counters
                    .lock()
                    .expect("deepseek counters mutex poisoned");
                counters.successes += 1;
                Ok(page)
            }
            None => {
                self.note_failure("response has no page text", false);
                Err("deepseek fetch: response has no page text".to_string())
            }
        }
    }

    async fn throttle(&self) {
        let mut last = self.last_request.lock().await;
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if elapsed < MIN_INTERVAL {
                tokio::time::sleep(MIN_INTERVAL - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }

    fn note_failure(&self, message: &str, rate_limited: bool) {
        let mut counters = self
            .counters
            .lock()
            .expect("deepseek counters mutex poisoned");
        counters.failures += 1;
        if rate_limited {
            counters.rate_limited += 1;
        }
        counters.last_error = Some(truncate_chars(message, 200).to_string());
    }
}

/// Parse the JSON body of POST /v1/tools/fetch.
///
/// Accepts a few plausible field names so the client keeps working if the
/// gateway returns content/markdown/text instead of a single documented field.
pub fn parse_deepseek_fetch_response(value: &Value) -> Option<DeepseekFetchPage> {
    if let Some(text) = value.as_str() {
        let text = collapse_ws(text);
        if text.is_empty() {
            return None;
        }
        return Some(DeepseekFetchPage {
            title: String::new(),
            text: truncate_chars(&text, MAX_TEXT_CHARS).to_string(),
        });
    }
    let object = value.as_object()?;
    let title = object
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let text = ["content", "markdown", "text", "body"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .or_else(|| {
            object
                .get("data")
                .and_then(Value::as_object)
                .and_then(|data| {
                    ["content", "markdown", "text", "body"]
                        .iter()
                        .find_map(|key| data.get(*key).and_then(Value::as_str))
                })
        })?;
    let text = collapse_ws(text);
    if text.is_empty() {
        return None;
    }
    Some(DeepseekFetchPage {
        title,
        text: truncate_chars(&text, MAX_TEXT_CHARS).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::Arc;

    #[test]
    fn parses_fetch_payloads() {
        let page = parse_deepseek_fetch_response(&json!({
            "title": "放送情報",
            "content": "毎週土曜 23:30 放送"
        }))
        .unwrap();
        assert_eq!(page.title, "放送情報");
        assert!(page.text.contains("23:30"));
        assert!(page.corpus().contains("放送情報"));

        let markdown = parse_deepseek_fetch_response(&json!({
            "markdown": "# 作品A\n毎週日曜 01:00"
        }))
        .unwrap();
        assert!(markdown.text.contains("01:00"));

        let wrapped = parse_deepseek_fetch_response(&json!({
            "data": {"content": "毎週金曜 24:30"}
        }))
        .unwrap();
        assert!(wrapped.text.contains("24:30"));

        assert!(parse_deepseek_fetch_response(&json!({"title": "empty"})).is_none());
        assert!(parse_deepseek_fetch_response(&json!({})).is_none());
    }

    fn spawn_server(
        status: u16,
        body: &'static str,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 1024];
            while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            }
            let request = String::from_utf8_lossy(&buffer).to_string();
            let reason = match status {
                200 => "OK",
                429 => "Too Many Requests",
                _ => "Error",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            request
        });
        (address, handle)
    }

    #[tokio::test]
    async fn fetches_page_from_official_endpoint() {
        let (address, server) =
            spawn_server(200, r#"{"title":"T","content":"毎週土曜 23:30 放送"}"#);
        let client = Arc::new(
            DeepseekFetchClient::new(
                "test-key".to_string(),
                format!("http://{address}/v1/tools/fetch"),
                10,
                "AirRadar-Test",
                5,
            )
            .unwrap(),
        );
        let page = client.fetch("https://example.com/schedule").await.unwrap();
        assert!(page.text.contains("23:30"));
        let stats = client.stats();
        assert_eq!(stats.calls, 1);
        assert_eq!(stats.successes, 1);
        let request = server.join().unwrap().to_ascii_lowercase();
        assert!(request.contains("authorization: bearer test-key"));
        assert!(request.contains("https://example.com/schedule"));
    }

    #[tokio::test]
    async fn enforces_budget() {
        let (address, server) = spawn_server(200, r#"{"content":"毎週土曜 23:30"}"#);
        let client = DeepseekFetchClient::new(
            "key".to_string(),
            format!("http://{address}/v1/tools/fetch"),
            1,
            "AirRadar-Test",
            5,
        )
        .unwrap();
        client.fetch("https://example.com/one").await.unwrap();
        let error = client.fetch("https://example.com/two").await.unwrap_err();
        assert!(error.contains("budget"), "error: {error}");
        let stats = client.stats();
        assert_eq!(stats.calls, 1);
        assert_eq!(stats.budget_exhausted, 1);
        assert_eq!(server.join().unwrap().is_empty(), false);
        assert_eq!(client.remaining_budget(), 0);
    }

    #[tokio::test]
    async fn handles_429_with_cooldown() {
        let (address, server) = spawn_server(429, r#"{"error":"rate limited"}"#);
        let client = DeepseekFetchClient::new(
            "key".to_string(),
            format!("http://{address}/v1/tools/fetch"),
            10,
            "AirRadar-Test",
            5,
        )
        .unwrap();
        let error = client.fetch("https://example.com/a").await.unwrap_err();
        assert!(error.contains("429"), "error: {error}");
        let stats = client.stats();
        assert_eq!(stats.rate_limited, 1);
        // The second call must fail via cooldown without touching the network.
        let error = client.fetch("https://example.com/b").await.unwrap_err();
        assert!(error.contains("cooldown"), "error: {error}");
        assert_eq!(client.stats().calls, 1);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn reset_run_restores_budget_and_stats() {
        let (address, server) = spawn_server(200, r#"{"content":"毎週土曜 23:30"}"#);
        let client = DeepseekFetchClient::new(
            "key".to_string(),
            format!("http://{address}/v1/tools/fetch"),
            1,
            "AirRadar-Test",
            5,
        )
        .unwrap();
        client.fetch("https://example.com/a").await.unwrap();
        client.reset_run();
        assert_eq!(client.remaining_budget(), 1);
        assert_eq!(client.stats().calls, 0);
        server.join().unwrap();
    }
}
