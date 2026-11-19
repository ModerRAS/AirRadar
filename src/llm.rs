//! Optional LLM fallback for subjects where the rule extractors found nothing.
//!
//! A single batched OpenAI-compatible chat completion is attempted per run
//! (bounded by MAX_LLM_CALLS). Results are cached in /data/llm-cache.json so a
//! flaky model or a temporarily down gateway never blocks a run.
//!
//! OpenCode Go (base https://opencode.ai/inference/go/v1) requires an extra
//! x-opencode-session header on every request: without it the gateway answers
//! HTTP 400. The id is taken from LLM_SESSION_ID or generated once and persisted
//! as DATA_DIR/llm-session-id, so all requests share a stable session.
//!
//! Search credentials (BRAVE/TAVILY/DEEPSEEK_API_KEY) and LLM credentials
//! (LLM_API_KEY) are intentionally independent: this client only ever reads
//! LLM_* settings.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;
use crate::log;
use crate::store::Store;
use crate::util::{collapse_ws, format_time, hash_hex, next_weekday, truncate_chars};

const SYSTEM_PROMPT: &str = concat!(
    "You extract weekly anime broadcast times from Japanese and Chinese web text. ",
    "Return ONLY a JSON object: {\"results\":[{\"id\":<subject id>,\"weekday\":1-7,",
    "\"time\":\"HH:MM\",\"confidence\":\"high|medium|low\",\"reason\":\"short\"}]}. ",
    "weekday is the ACTUAL JST calendar day where 1=Monday and 7=Sunday. ",
    "time is a 24h JST clock time. Late-night notation like 25:15 means 01:15 ",
    "on the NEXT day, so increase weekday by one. Use null when the text does ",
    "not state a value. Never invent information that is not in the text."
);

/// Input for one subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRequest {
    pub subject_id: u64,
    pub title: String,
    pub text: String,
}

/// One structured suggestion returned by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmSuggestion {
    pub subject_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekday: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    #[serde(default)]
    pub reason: String,
}

/// Per-run LLM counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LlmRunStats {
    pub configured: bool,
    pub calls: usize,
    pub cached_hits: usize,
    pub results: usize,
    pub failures: usize,
    pub last_error: Option<String>,
}

/// Header required by the OpenCode Go gateway (missing header -> HTTP 400).
pub const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

/// Host fragment that identifies an OpenCode Go endpoint.
pub const OPENCODE_HOST_MARKER: &str = "opencode.ai";

/// Prefix used for auto-generated stable session ids.
pub const SESSION_ID_PREFIX: &str = "airradar";

/// OpenAI-compatible chat client used only for the extraction fallback.
pub struct LlmClient {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    store: Arc<Store>,
    batch_size: usize,
    max_input_chars: usize,
    max_calls: usize,
    /// x-opencode-session value; only set for OpenCode Go endpoints.
    session_id: Option<String>,
    /// Prebuilt headers applied to every request.
    headers: HeaderMap,
}

impl LlmClient {
    pub fn from_config(cfg: &Config, store: Arc<Store>) -> Result<Option<LlmClient>, String> {
        if !cfg.has_llm() {
            return Ok(None);
        }
        let base = cfg
            .llm_base_url
            .clone()
            .ok_or_else(|| "LLM_BASE_URL missing".to_string())?;
        let endpoint = if base.ends_with("/chat/completions") {
            base
        } else {
            format!("{}/chat/completions", base.trim_end_matches('/'))
        };
        let api_key = cfg.llm_api_key.clone().unwrap_or_default();
        let model = cfg.llm_model.clone().unwrap_or_default();
        let session_id = if is_opencode_endpoint(&endpoint) {
            let session_id = resolve_llm_session_id(&store, cfg.llm_session_id.as_deref());
            log::info("OpenCode Go endpoint detected; x-opencode-session header enabled");
            Some(session_id)
        } else {
            None
        };
        let headers = build_llm_headers(&endpoint, &api_key, session_id.as_deref())?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|error| format!("build llm http client: {error}"))?;
        Ok(Some(LlmClient {
            client,
            endpoint,
            model,
            store,
            batch_size: cfg.llm_batch_size.max(1),
            max_input_chars: cfg.llm_max_input_chars.max(200),
            max_calls: cfg.max_llm_calls,
            session_id,
            headers,
        }))
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Resolved x-opencode-session value (only set for OpenCode Go endpoints).
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Headers applied to every chat completion request.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn is_opencode(&self) -> bool {
        is_opencode_endpoint(&self.endpoint)
    }

    /// Resolve suggestions for the given requests, using the on-disk result
    /// cache first. Never returns an error: failures are counted in the stats.
    pub async fn extract(&self, requests: &[LlmRequest]) -> (Vec<LlmSuggestion>, LlmRunStats) {
        let mut stats = LlmRunStats {
            configured: true,
            ..LlmRunStats::default()
        };
        if requests.is_empty() {
            return (Vec::new(), stats);
        }
        let mut cache = match self.store.load_llm_cache() {
            Ok(cache) => cache,
            Err(error) => {
                log::warn(format!("llm cache read failed: {error}"));
                HashMap::new()
            }
        };
        let mut suggestions: Vec<LlmSuggestion> = Vec::new();
        let mut pending: Vec<(LlmRequest, String)> = Vec::new();
        for request in requests {
            let key = cache_key(request);
            if let Some(value) = cache.get(&key) {
                if let Ok(suggestion) = serde_json::from_value::<LlmSuggestion>(value.clone()) {
                    stats.cached_hits += 1;
                    suggestions.push(suggestion);
                    continue;
                }
            }
            pending.push((request.clone(), key));
        }
        if pending.is_empty() {
            return (suggestions, stats);
        }
        let mut fresh: Vec<(String, LlmSuggestion)> = Vec::new();
        for batch in pending.chunks(self.batch_size).take(self.max_calls) {
            stats.calls += 1;
            match self.call_batch(batch).await {
                Ok(found) => {
                    stats.results += found.len();
                    for suggestion in found {
                        if let Some((_, key)) = batch
                            .iter()
                            .find(|(request, _)| request.subject_id == suggestion.subject_id)
                        {
                            fresh.push((key.clone(), suggestion.clone()));
                        }
                        suggestions.push(suggestion);
                    }
                }
                Err(message) => {
                    stats.failures += 1;
                    stats.last_error = Some(message);
                }
            }
        }
        for (key, suggestion) in &fresh {
            if let Ok(value) = serde_json::to_value(suggestion) {
                cache.insert(key.clone(), value);
            }
        }
        if !fresh.is_empty() {
            if let Err(error) = self.store.save_llm_cache(&cache) {
                log::warn(format!("llm cache write failed: {error}"));
            }
        }
        (suggestions, stats)
    }

    async fn call_batch(
        &self,
        batch: &[(LlmRequest, String)],
    ) -> Result<Vec<LlmSuggestion>, String> {
        let items: Vec<Value> = batch
            .iter()
            .map(|(request, _)| {
                json!({
                    "id": request.subject_id,
                    "title": request.title,
                    "text": truncate_chars(&request.text, self.max_input_chars),
                })
            })
            .collect();
        let user_content = serde_json::to_string(&json!({ "items": items }))
            .map_err(|error| format!("encode llm request: {error}"))?;
        let body = json!({
            "model": self.model,
            "temperature": 0,
            "max_tokens": 1200,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": user_content}
            ]
        });
        let response = self
            .client
            .post(&self.endpoint)
            .headers(self.headers.clone())
            .json(&body)
            .send()
            .await
            .map_err(|error| format!("llm request failed: {error}"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| format!("llm read body failed: {error}"))?;
        if !status.is_success() {
            return Err(format!(
                "llm http {}: {}",
                status.as_u16(),
                truncate_chars(&text, 240)
            ));
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| format!("llm response is not JSON: {error}"))?;
        let content = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .ok_or_else(|| "llm response missing choices[0].message.content".to_string())?;
        let allowed: Vec<u64> = batch
            .iter()
            .map(|(request, _)| request.subject_id)
            .collect();
        Ok(parse_llm_response(content, &allowed))
    }
}

/// Deterministic cache key for one suggestion request.
pub fn cache_key(request: &LlmRequest) -> String {
    format!(
        "{}:{}",
        request.subject_id,
        hash_hex(&format!("{}|{}", request.title, request.text))
    )
}

/// True when the endpoint host points at the OpenCode Go gateway.
pub fn is_opencode_endpoint(endpoint: &str) -> bool {
    crate::util::url_host(endpoint)
        .map(|host| host.contains(OPENCODE_HOST_MARKER))
        .unwrap_or(false)
}

/// Generate a stable-looking session id for gateways that require one.
pub fn generate_session_id() -> String {
    format!("{SESSION_ID_PREFIX}-{:016x}", crate::util::random_u64())
}

/// Resolve the OpenCode Go session id: explicit LLM_SESSION_ID wins, then the
/// value persisted in DATA_DIR/llm-session-id, then a freshly generated id that
/// is persisted for later runs. Persistence errors are logged, never fatal.
pub fn resolve_llm_session_id(store: &Store, configured: Option<&str>) -> String {
    if let Some(value) = configured.map(str::trim).filter(|value| !value.is_empty()) {
        return value.to_string();
    }
    match store.load_llm_session_id() {
        Ok(Some(value)) => return value,
        Ok(None) => {}
        Err(error) => log::warn(format!("llm session id read failed: {error}")),
    }
    let generated = generate_session_id();
    match store.save_llm_session_id(&generated) {
        Ok(()) => log::info("generated and persisted stable LLM session id in the data dir"),
        Err(error) => log::warn(format!("llm session id persist failed: {error}")),
    }
    generated
}

/// Build the headers for an OpenAI-compatible chat endpoint.
///
/// Always sends Authorization (Bearer LLM_API_KEY) and Accept. OpenCode Go
/// endpoints additionally require x-opencode-session; a missing/invalid session
/// id is a configuration error rather than a silent HTTP 400 later.
pub fn build_llm_headers(
    endpoint: &str,
    api_key: &str,
    session_id: Option<&str>,
) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|error| format!("invalid LLM_API_KEY header value: {error}"))?,
    );
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    if is_opencode_endpoint(endpoint) {
        let session_id = session_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "OpenCode Go endpoint requires a session id".to_string())?;
        headers.insert(
            OPENCODE_SESSION_HEADER,
            HeaderValue::from_str(session_id)
                .map_err(|error| format!("invalid LLM session id header value: {error}"))?,
        );
    }
    Ok(headers)
}

fn strip_code_fence(input: &str) -> &str {
    let trimmed = input.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        let rest = match rest.find('\n') {
            Some(index) => &rest[index + 1..],
            None => rest,
        };
        return rest.strip_suffix("```").unwrap_or(rest);
    }
    trimmed
}

fn weekday_from_text(text: &str) -> Option<u8> {
    match text.trim().to_ascii_lowercase().as_str() {
        "monday" | "mon" | "月曜" | "月曜日" | "周一" | "星期一" => Some(1),
        "tuesday" | "tue" | "火曜" | "火曜日" | "周二" | "星期二" => Some(2),
        "wednesday" | "wed" | "水曜" | "水曜日" | "周三" | "星期三" => Some(3),
        "thursday" | "thu" | "木曜" | "木曜日" | "周四" | "星期四" => Some(4),
        "friday" | "fri" | "金曜" | "金曜日" | "周五" | "星期五" => Some(5),
        "saturday" | "sat" | "土曜" | "土曜日" | "周六" | "星期六" => Some(6),
        "sunday" | "sun" | "日曜" | "日曜日" | "周日" | "星期日" | "星期天" => Some(7),
        _ => None,
    }
}

fn parse_weekday_value(value: &Value) -> Option<u8> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .and_then(|number| u8::try_from(number).ok())
            .filter(|number| (1..=7).contains(number)),
        Value::String(text) => weekday_from_text(text),
        _ => None,
    }
}

/// Normalise an LLM time into (day_shift, HH:MM). Handles 24:00..29:59.
fn normalize_time(raw: &str) -> Option<(u8, String)> {
    let text = raw.trim().replace('：', ":");
    let (hour_text, minute_text) = text.split_once(':')?;
    let hour: u32 = hour_text.trim().parse().ok()?;
    let minute: u32 = minute_text.trim().parse().ok()?;
    if minute > 59 {
        return None;
    }
    if hour >= 24 {
        if hour > 29 {
            return None;
        }
        return Some((1, format_time(hour - 24, minute)));
    }
    Some((0, format_time(hour, minute)))
}

fn sanitize_reason(reason: &str) -> String {
    truncate_chars(&collapse_ws(reason), 160).to_string()
}

/// Parse the JSON body returned by the model into validated suggestions.
pub fn parse_llm_response(content: &str, allowed_ids: &[u64]) -> Vec<LlmSuggestion> {
    let cleaned = strip_code_fence(content);
    let Some(start) = cleaned.find('{') else {
        return Vec::new();
    };
    let Some(end) = cleaned.rfind('}') else {
        return Vec::new();
    };
    if end < start {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(&cleaned[start..=end]) else {
        return Vec::new();
    };
    let Some(results) = value.get("results").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut suggestions = Vec::new();
    for item in results {
        let Some(subject_id) = item.get("id").and_then(Value::as_u64) else {
            continue;
        };
        if !allowed_ids.is_empty() && !allowed_ids.contains(&subject_id) {
            continue;
        }
        let weekday = item.get("weekday").and_then(parse_weekday_value);
        let mut time = None;
        let mut day_shift = 0u8;
        if let Some(raw_time) = item.get("time").and_then(Value::as_str) {
            if let Some((shift, normalized)) = normalize_time(raw_time) {
                day_shift = shift;
                time = Some(normalized);
            }
        }
        let Some(weekday) = weekday else {
            // A time without a weekday cannot be placed on the schedule.
            continue;
        };
        let weekday = if day_shift > 0 {
            next_weekday(weekday)
        } else {
            weekday
        };
        let weekday = Some(weekday);
        let reason = item
            .get("reason")
            .and_then(Value::as_str)
            .map(sanitize_reason)
            .unwrap_or_default();
        suggestions.push(LlmSuggestion {
            subject_id,
            weekday,
            time,
            reason,
        });
    }
    suggestions
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn request(id: u64, text: &str) -> LlmRequest {
        LlmRequest {
            subject_id: id,
            title: "Test Show".to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn parses_fenced_response() {
        let content = "```json\n{\"results\":[{\"id\":393010,\"weekday\":3,\"time\":\"23:00\",\"reason\":\"毎週水曜 23:00\"}]}\n```";
        let suggestions = parse_llm_response(content, &[393010]);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].subject_id, 393010);
        assert_eq!(suggestions[0].weekday, Some(3));
        assert_eq!(suggestions[0].time.as_deref(), Some("23:00"));
        assert!(suggestions[0].reason.contains("23:00"));
    }

    #[test]
    fn late_night_time_is_shifted() {
        let content = r#"{"results":[{"id":1,"weekday":6,"time":"25:15","reason":"土曜25:15"}]}"#;
        let suggestions = parse_llm_response(content, &[1]);
        assert_eq!(suggestions[0].weekday, Some(7));
        assert_eq!(suggestions[0].time.as_deref(), Some("01:15"));
    }

    #[test]
    fn invalid_entries_are_dropped() {
        let content = r#"{"results":[
            {"id":1,"weekday":9,"time":"23:00"},
            {"id":2,"weekday":3,"time":"25:99"},
            {"id":3,"weekday":0,"time":null},
            {"id":4,"weekday":"Saturday","time":"23:30"}
        ]}"#;
        let suggestions = parse_llm_response(content, &[1, 2, 3, 4]);
        assert_eq!(suggestions.len(), 2, "suggestions: {suggestions:?}");
        assert!(suggestions
            .iter()
            .any(|suggestion| suggestion.subject_id == 2
                && suggestion.weekday == Some(3)
                && suggestion.time.is_none()));
        assert!(suggestions
            .iter()
            .any(|suggestion| suggestion.subject_id == 4
                && suggestion.weekday == Some(6)
                && suggestion.time.as_deref() == Some("23:30")));
    }

    #[test]
    fn unknown_and_empty_responses_yield_nothing() {
        assert!(parse_llm_response("no json here", &[1]).is_empty());
        assert!(parse_llm_response("", &[1]).is_empty());
        assert!(parse_llm_response(
            r#"{"results":[{"id":99,"weekday":1,"time":"10:00"}]}"#,
            &[1]
        )
        .is_empty());
        assert!(parse_llm_response(r#"{"results":[]}"#, &[1]).is_empty());
    }

    #[test]
    fn cache_key_changes_with_text() {
        let first = cache_key(&request(1, "a text"));
        let second = cache_key(&request(1, "a text"));
        let third = cache_key(&request(1, "another text"));
        assert_eq!(first, second);
        assert_ne!(first, third);
    }

    #[test]
    fn from_config_is_off_without_all_settings() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::new(dir.path().to_path_buf()));
        let cfg = Config::for_tests(PathBuf::from(dir.path()));
        assert!(LlmClient::from_config(&cfg, store.clone())
            .unwrap()
            .is_none());

        let mut cfg = cfg;
        cfg.llm_base_url = Some("http://127.0.0.1:9/v1/".to_string());
        cfg.llm_api_key = Some("k".to_string());
        cfg.llm_model = Some("m".to_string());
        let client = LlmClient::from_config(&cfg, store).unwrap().unwrap();
        assert_eq!(client.endpoint(), "http://127.0.0.1:9/v1/chat/completions");
        assert_eq!(client.model(), "m");
    }

    #[test]
    fn full_endpoint_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::new(dir.path().to_path_buf()));
        let mut cfg = Config::for_tests(PathBuf::from(dir.path()));
        cfg.llm_base_url = Some("http://127.0.0.1:9/chat/completions".to_string());
        cfg.llm_api_key = Some("k".to_string());
        cfg.llm_model = Some("m".to_string());
        let client = LlmClient::from_config(&cfg, store).unwrap().unwrap();
        assert_eq!(client.endpoint(), "http://127.0.0.1:9/chat/completions");
    }

    #[test]
    fn system_prompt_documents_rules() {
        assert!(SYSTEM_PROMPT.contains("weekday"));
        assert!(SYSTEM_PROMPT.contains("Never invent"));
    }

    #[test]
    fn detects_opencode_endpoints() {
        assert!(is_opencode_endpoint(
            "https://opencode.ai/inference/go/v1/chat/completions"
        ));
        assert!(is_opencode_endpoint(
            "http://opencode.ai:8080/inference/go/v1/chat/completions"
        ));
        assert!(!is_opencode_endpoint(
            "https://api.openai.com/v1/chat/completions"
        ));
        assert!(!is_opencode_endpoint(
            "http://127.0.0.1:9/v1/chat/completions"
        ));
        assert!(!is_opencode_endpoint("not a url"));
    }

    #[test]
    fn opencode_endpoint_gets_session_header() {
        let headers = build_llm_headers(
            "https://opencode.ai/inference/go/v1/chat/completions",
            "opencode-go-key",
            Some("session-abc"),
        )
        .unwrap();
        assert_eq!(
            headers
                .get(OPENCODE_SESSION_HEADER)
                .unwrap()
                .to_str()
                .unwrap(),
            "session-abc"
        );
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer opencode-go-key"
        );
        assert_eq!(
            headers.get(ACCEPT).unwrap().to_str().unwrap(),
            "application/json"
        );
    }

    #[test]
    fn generic_endpoint_has_no_session_header() {
        let headers = build_llm_headers(
            "https://api.openai.com/v1/chat/completions",
            "llm-key",
            Some("should-be-ignored"),
        )
        .unwrap();
        assert!(headers.get(OPENCODE_SESSION_HEADER).is_none());
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer llm-key"
        );
    }

    #[test]
    fn opencode_endpoint_requires_session_id() {
        let error = build_llm_headers(
            "https://opencode.ai/inference/go/v1/chat/completions",
            "key",
            None,
        )
        .unwrap_err();
        assert!(error.contains("session id"), "error: {error}");
    }

    #[test]
    fn rejects_invalid_session_id_header() {
        let error = build_llm_headers(
            "https://opencode.ai/inference/go/v1/chat/completions",
            "key",
            Some("bad\nsession"),
        )
        .unwrap_err();
        assert!(error.contains("invalid LLM session id"), "error: {error}");
    }

    #[test]
    fn resolves_explicit_and_persisted_session_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        assert_eq!(
            resolve_llm_session_id(&store, Some("explicit-id")),
            "explicit-id"
        );
        assert!(
            store.load_llm_session_id().unwrap().is_none(),
            "explicit session ids are not persisted"
        );

        let generated = resolve_llm_session_id(&store, None);
        assert!(
            generated.starts_with(SESSION_ID_PREFIX),
            "generated: {generated}"
        );
        assert_eq!(
            store.load_llm_session_id().unwrap().as_deref(),
            Some(generated.as_str())
        );
        // Stable across calls and restarts.
        assert_eq!(resolve_llm_session_id(&store, None), generated);
        assert_eq!(resolve_llm_session_id(&store, None), generated);
    }

    #[test]
    fn from_config_wires_stable_opencode_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::new(dir.path().to_path_buf()));
        let mut cfg = Config::for_tests(PathBuf::from(dir.path()));
        cfg.llm_base_url = Some("https://opencode.ai/inference/go/v1".to_string());
        cfg.llm_api_key = Some("opencode-go-key".to_string());
        cfg.llm_model = Some("model".to_string());

        let client = LlmClient::from_config(&cfg, store.clone())
            .unwrap()
            .unwrap();
        assert!(client.is_opencode());
        let session_id = client.session_id().unwrap().to_string();
        assert!(session_id.starts_with(SESSION_ID_PREFIX));
        assert_eq!(
            client
                .headers()
                .get(OPENCODE_SESSION_HEADER)
                .unwrap()
                .to_str()
                .unwrap(),
            session_id
        );
        assert_eq!(
            store.load_llm_session_id().unwrap().as_deref(),
            Some(session_id.as_str())
        );

        // A restart reuses the persisted id instead of generating a new one.
        let second = LlmClient::from_config(&cfg, store).unwrap().unwrap();
        assert_eq!(second.session_id(), Some(session_id.as_str()));
    }

    #[test]
    fn explicit_session_id_overrides_persisted_value() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::new(dir.path().to_path_buf()));
        store.save_llm_session_id("persisted-id").unwrap();
        let mut cfg = Config::for_tests(PathBuf::from(dir.path()));
        cfg.llm_base_url = Some("https://opencode.ai/inference/go/v1".to_string());
        cfg.llm_api_key = Some("key".to_string());
        cfg.llm_model = Some("model".to_string());
        cfg.llm_session_id = Some("explicit-id".to_string());
        let client = LlmClient::from_config(&cfg, store).unwrap().unwrap();
        assert_eq!(client.session_id(), Some("explicit-id"));
        assert_eq!(
            client
                .headers()
                .get(OPENCODE_SESSION_HEADER)
                .unwrap()
                .to_str()
                .unwrap(),
            "explicit-id"
        );
    }

    #[test]
    fn llm_never_falls_back_to_search_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::new(dir.path().to_path_buf()));
        let mut cfg = Config::for_tests(PathBuf::from(dir.path()));
        cfg.deepseek_api_key = Some("deepseek-search-key".to_string());
        cfg.brave_api_key = Some("brave-search-key".to_string());
        cfg.llm_base_url = Some("https://opencode.ai/inference/go/v1".to_string());
        cfg.llm_model = Some("model".to_string());
        cfg.llm_api_key = None;
        assert!(!cfg.has_llm());
        assert!(
            LlmClient::from_config(&cfg, store).unwrap().is_none(),
            "search keys must never satisfy the LLM configuration"
        );
    }

    #[test]
    fn llm_authorization_uses_only_llm_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::new(dir.path().to_path_buf()));
        let mut cfg = Config::for_tests(PathBuf::from(dir.path()));
        cfg.llm_base_url = Some("https://api.openai.com/v1".to_string());
        cfg.llm_api_key = Some("opencode-go-key".to_string());
        cfg.llm_model = Some("model".to_string());
        cfg.deepseek_api_key = Some("deepseek-search-key".to_string());
        cfg.brave_api_key = Some("brave-search-key".to_string());
        let client = LlmClient::from_config(&cfg, store).unwrap().unwrap();
        assert_eq!(
            client
                .headers()
                .get(AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer opencode-go-key"
        );
        assert!(!client.is_opencode());
        assert!(client.session_id().is_none());
    }

    #[tokio::test]
    async fn sends_bearer_header_over_the_wire_and_omits_session_for_generic_endpoint() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Guard against regressions that would otherwise hang the test run.
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
            let request = String::from_utf8_lossy(&buffer).to_ascii_lowercase();
            let content = r#"{"results":[{"id":1,"weekday":6,"time":"23:30"}]}"#;
            let body = json!({ "choices": [{ "message": { "content": content } }] }).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            request
        });

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::new(dir.path().to_path_buf()));
        let mut cfg = Config::for_tests(PathBuf::from(dir.path()));
        cfg.llm_base_url = Some(format!("http://{address}/v1"));
        cfg.llm_api_key = Some("test-llm-key".to_string());
        cfg.llm_model = Some("test-model".to_string());
        cfg.max_llm_calls = 1;
        let client = LlmClient::from_config(&cfg, store).unwrap().unwrap();
        let (suggestions, stats) = client.extract(&[request(1, "毎週土曜 23:30 放送")]).await;
        let request_text = server.join().unwrap();
        assert!(
            request_text.contains("authorization: bearer test-llm-key"),
            "request: {request_text}"
        );
        assert!(
            request_text.contains("accept: application/json"),
            "request: {request_text}"
        );
        assert!(
            !request_text.contains(OPENCODE_SESSION_HEADER),
            "generic endpoints must not send the OpenCode session header: {request_text}"
        );
        assert_eq!(stats.calls, 1);
        assert_eq!(suggestions.len(), 1, "suggestions: {suggestions:?}");
        assert_eq!(suggestions[0].weekday, Some(6));
    }
}
