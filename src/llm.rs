//! Optional LLM fallback for subjects where the rule extractors found nothing.
//!
//! A single batched OpenAI-compatible chat completion is attempted per run
//! (bounded by MAX_LLM_CALLS). Results are cached in /data/llm-cache.json so a
//! flaky model or a temporarily down gateway never blocks a run.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

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

/// OpenAI-compatible chat client used only for the extraction fallback.
pub struct LlmClient {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
    store: Arc<Store>,
    batch_size: usize,
    max_input_chars: usize,
    max_calls: usize,
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
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|error| format!("build llm http client: {error}"))?;
        Ok(Some(LlmClient {
            client,
            endpoint,
            api_key: cfg.llm_api_key.clone().unwrap_or_default(),
            model: cfg.llm_model.clone().unwrap_or_default(),
            store,
            batch_size: cfg.llm_batch_size.max(1),
            max_input_chars: cfg.llm_max_input_chars.max(200),
            max_calls: cfg.max_llm_calls,
        }))
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn model(&self) -> &str {
        &self.model
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
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Accept", "application/json")
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
}
