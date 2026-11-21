//! Concrete Brave / Tavily / DeepSeek search provider implementations.

use std::time::Duration;

use serde_json::{json, Value};

use super::{BoxFuture, ProviderKind, SearchError, SearchProvider, SearchResult};
use crate::util::{collapse_ws, is_http_url, percent_encode, truncate_chars};

const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const TAVILY_ENDPOINT: &str = "https://api.tavily.com/search";
const SEARCH_COUNT: usize = 8;
const SNIPPET_LIMIT: usize = 600;

fn build_client(user_agent: &str, timeout_secs: u64) -> Result<reqwest::Client, SearchError> {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(timeout_secs.max(1)))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|error| SearchError::new(format!("search http client: {error}")))
}

async fn response_json(response: reqwest::Response, provider: &str) -> Result<Value, SearchError> {
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| SearchError::new(format!("{provider}: read body: {error}")))?;
    if !status.is_success() {
        return Err(SearchError::new(format!(
            "{provider}: HTTP {}: {}",
            status.as_u16(),
            truncate_chars(&text, 240)
        )));
    }
    serde_json::from_str(&text)
        .map_err(|error| SearchError::new(format!("{provider}: invalid JSON: {error}")))
}

fn snippet_of(value: &Value, field: &str) -> String {
    collapse_ws(
        &truncate_chars(
            value.get(field).and_then(Value::as_str).unwrap_or_default(),
            SNIPPET_LIMIT,
        )
        .to_string(),
    )
}

/// Brave Search API (https://api.search.brave.com/res/v1/web/search).
pub struct BraveProvider {
    client: reqwest::Client,
    api_key: String,
}

impl BraveProvider {
    pub fn new(
        api_key: String,
        user_agent: &str,
        timeout_secs: u64,
    ) -> Result<BraveProvider, SearchError> {
        Ok(BraveProvider {
            client: build_client(user_agent, timeout_secs)?,
            api_key,
        })
    }
}

impl SearchProvider for BraveProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Brave
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
    ) -> BoxFuture<'a, Result<Vec<SearchResult>, SearchError>> {
        Box::pin(async move {
            let url = format!(
                "{BRAVE_ENDPOINT}?q={}&count={SEARCH_COUNT}&safesearch=off&text_decorations=false",
                percent_encode(query)
            );
            let response = self
                .client
                .get(url)
                .header("X-Subscription-Token", &self.api_key)
                .header("Accept", "application/json")
                .send()
                .await
                .map_err(|error| SearchError::new(format!("brave: request failed: {error}")))?;
            let value = response_json(response, "brave").await?;
            Ok(parse_brave_response(&value))
        })
    }
}

fn parse_brave_response(value: &Value) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let Some(items) = value
        .get("web")
        .and_then(|web| web.get("results"))
        .and_then(Value::as_array)
    else {
        return results;
    };
    for item in items {
        let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
        if !is_http_url(url) {
            continue;
        }
        let extra = item
            .get("extra_snippets")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let mut snippet = snippet_of(item, "description");
        if !extra.is_empty() {
            snippet.push(' ');
            snippet.push_str(&collapse_ws(&truncate_chars(&extra, SNIPPET_LIMIT)));
        }
        results.push(SearchResult {
            title: snippet_of(item, "title"),
            url: url.to_string(),
            snippet,
            provider: "brave".to_string(),
        });
    }
    results
}

/// Tavily Search API (https://api.tavily.com/search).
pub struct TavilyProvider {
    client: reqwest::Client,
    api_key: String,
}

impl TavilyProvider {
    pub fn new(
        api_key: String,
        user_agent: &str,
        timeout_secs: u64,
    ) -> Result<TavilyProvider, SearchError> {
        Ok(TavilyProvider {
            client: build_client(user_agent, timeout_secs)?,
            api_key,
        })
    }
}

impl SearchProvider for TavilyProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Tavily
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
    ) -> BoxFuture<'a, Result<Vec<SearchResult>, SearchError>> {
        Box::pin(async move {
            let body = json!({
                "query": query,
                "max_results": SEARCH_COUNT,
                "search_depth": "basic",
                "include_answer": false,
                "include_raw_content": false
            });
            let response = self
                .client
                .post(TAVILY_ENDPOINT)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Accept", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|error| SearchError::new(format!("tavily: request failed: {error}")))?;
            let value = response_json(response, "tavily").await?;
            Ok(parse_tavily_response(&value))
        })
    }
}

fn parse_tavily_response(value: &Value) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let Some(items) = value.get("results").and_then(Value::as_array) else {
        return results;
    };
    for item in items {
        let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
        if !is_http_url(url) {
            continue;
        }
        results.push(SearchResult {
            title: snippet_of(item, "title"),
            url: url.to_string(),
            snippet: snippet_of(item, "content"),
            provider: "tavily".to_string(),
        });
    }
    results
}

const DEEPSEEK_SYSTEM_PROMPT: &str = concat!(
    "You are a web search assistant. Given a user query, return real, existing ",
    "web page results as JSON only: {\"results\":[{\"title\":\"...\",\"url\":\"https://...\",\"snippet\":\"...\"}]}. ",
    "Return at most 8 results. Never invent URLs. Do not wrap the JSON in markdown fences."
);

/// Default official DeepSeek search endpoint (POST /v1/tools/search).
pub const DEEPSEEK_TOOLS_SEARCH_PATH: &str = "/tools/search";
/// Legacy OpenAI-compatible chat endpoint (prompt-based pseudo search).
pub const DEEPSEEK_CHAT_PATH: &str = "/chat/completions";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeepseekSearchMode {
    /// Official server-side search API: POST /v1/tools/search.
    Tools,
    /// Legacy chat-completion prompt search (kept for old configurations).
    Chat,
}

/// DeepSeek search provider.
///
/// Defaults to the official server-side Search API
/// (https://api.deepseek.com/v1/tools/search) which returns real URLs plus
/// summary/content. A URL containing /chat/completions keeps the legacy
/// prompt-based behaviour for backwards compatibility.
pub struct DeepseekSearchProvider {
    client: reqwest::Client,
    api_key: String,
    endpoint: String,
    model: String,
    count: usize,
    mode: DeepseekSearchMode,
}

impl DeepseekSearchProvider {
    pub fn new(
        api_key: String,
        endpoint: String,
        model: String,
        count: usize,
        user_agent: &str,
        timeout_secs: u64,
    ) -> Result<DeepseekSearchProvider, SearchError> {
        let (endpoint, mode) = normalize_deepseek_search_endpoint(&endpoint);
        Ok(DeepseekSearchProvider {
            client: build_client(user_agent, timeout_secs)?,
            api_key,
            endpoint,
            model,
            count: count.clamp(1, 50),
            mode,
        })
    }

    pub fn mode(&self) -> &'static str {
        match self.mode {
            DeepseekSearchMode::Tools => "tools",
            DeepseekSearchMode::Chat => "chat",
        }
    }
}

fn normalize_deepseek_search_endpoint(endpoint: &str) -> (String, DeepseekSearchMode) {
    let trimmed = endpoint.trim().trim_end_matches('/');
    if trimmed.contains(DEEPSEEK_CHAT_PATH) {
        (trimmed.to_string(), DeepseekSearchMode::Chat)
    } else if trimmed.ends_with(DEEPSEEK_TOOLS_SEARCH_PATH) {
        (trimmed.to_string(), DeepseekSearchMode::Tools)
    } else if trimmed.ends_with("/v1") {
        (
            format!("{trimmed}{DEEPSEEK_TOOLS_SEARCH_PATH}"),
            DeepseekSearchMode::Tools,
        )
    } else {
        (trimmed.to_string(), DeepseekSearchMode::Tools)
    }
}

/// Body for the official POST /v1/tools/search API.
pub fn deepseek_tools_search_body(query: &str, count: usize) -> Value {
    json!({
        "query": query,
        "count": count.clamp(1, 50),
        "need_content": true,
        "ori_question": query
    })
}

impl SearchProvider for DeepseekSearchProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Deepseek
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
    ) -> BoxFuture<'a, Result<Vec<SearchResult>, SearchError>> {
        Box::pin(async move {
            let body = match self.mode {
                DeepseekSearchMode::Tools => deepseek_tools_search_body(query, self.count),
                DeepseekSearchMode::Chat => json!({
                    "model": self.model,
                    "temperature": 0,
                    "max_tokens": 1200,
                    "messages": [
                        {"role": "system", "content": DEEPSEEK_SYSTEM_PROMPT},
                        {"role": "user", "content": query}
                    ]
                }),
            };
            let response = self
                .client
                .post(&self.endpoint)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Accept", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|error| {
                    SearchError::new(format!("deepseek-search: request failed: {error}"))
                })?;
            let value = response_json(response, "deepseek-search").await?;
            match self.mode {
                DeepseekSearchMode::Tools => parse_deepseek_tools_response(&value),
                DeepseekSearchMode::Chat => parse_deepseek_response(&value),
            }
        })
    }
}

fn parse_deepseek_tools_response(value: &Value) -> Result<Vec<SearchResult>, SearchError> {
    let items = value.get("results").and_then(Value::as_array).or_else(|| {
        value
            .get("data")
            .and_then(|data| data.get("results"))
            .and_then(Value::as_array)
    });
    let Some(items) = items else {
        return Err(SearchError::new(
            "deepseek-search: response has no results array",
        ));
    };
    let mut results = Vec::new();
    for item in items {
        let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
        if !is_http_url(url) {
            continue;
        }
        let summary = snippet_of(item, "summary");
        let content = snippet_of(item, "content");
        let snippet = if summary.is_empty() {
            content
        } else if content.is_empty() {
            summary
        } else {
            collapse_ws(&truncate_chars(
                &format!("{summary} {content}"),
                SNIPPET_LIMIT,
            ))
        };
        results.push(SearchResult {
            title: snippet_of(item, "title"),
            url: url.to_string(),
            snippet,
            provider: "deepseek".to_string(),
        });
    }
    Ok(results)
}

fn parse_deepseek_response(value: &Value) -> Result<Vec<SearchResult>, SearchError> {
    let content = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .ok_or_else(|| SearchError::new("deepseek-search: missing choices[0].message.content"))?;
    parse_deepseek_content(content)
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

fn parse_deepseek_content(content: &str) -> Result<Vec<SearchResult>, SearchError> {
    let cleaned = strip_code_fence(content);
    let start = cleaned
        .find('{')
        .ok_or_else(|| SearchError::new("deepseek-search: response contains no JSON object"))?;
    let end = cleaned
        .rfind('}')
        .ok_or_else(|| SearchError::new("deepseek-search: response contains no JSON object"))?;
    if end < start {
        return Err(SearchError::new("deepseek-search: malformed JSON object"));
    }
    let value: Value = serde_json::from_str(&cleaned[start..=end])
        .map_err(|error| SearchError::new(format!("deepseek-search: invalid JSON: {error}")))?;
    let Some(items) = value.get("results").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut results = Vec::new();
    for item in items {
        let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
        if !is_http_url(url) {
            continue;
        }
        results.push(SearchResult {
            title: snippet_of(item, "title"),
            url: url.to_string(),
            snippet: snippet_of(item, "snippet"),
            provider: "deepseek".to_string(),
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_brave_response() {
        let value: Value = serde_json::from_str(
            r#"{"web":{"results":[
                {"title":"A","url":"https://a.example/schedule","description":"毎週土曜 23:30"},
                {"title":"B","url":"https://b.example/x","description":"每周日 01:00","extra_snippets":["extra"]},
                {"title":"bad","url":"javascript:void(0)","description":"x"}
            ]}}"#,
        )
        .unwrap();
        let results = parse_brave_response(&value);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://a.example/schedule");
        assert!(results[0].snippet.contains("23:30"));
        assert_eq!(results[1].provider, "brave");
        assert!(results[1].snippet.contains("extra"));
    }

    #[test]
    fn parses_brave_without_results() {
        let value: Value = serde_json::from_str(r#"{"web":{}}"#).unwrap();
        assert!(parse_brave_response(&value).is_empty());
    }

    #[test]
    fn parses_tavily_response() {
        let value: Value = serde_json::from_str(
            r#"{"results":[{"title":"T","url":"https://t.example/a","content":"毎週金曜 24:30"}]}"#,
        )
        .unwrap();
        let results = parse_tavily_response(&value);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].provider, "tavily");
        assert!(results[0].snippet.contains("24:30"));
    }

    #[test]
    fn parses_deepseek_official_tools_response() {
        let value: Value = serde_json::from_str(
            r#"{"results":[
                {"title":"D","url":"https://d.example/s","summary":"毎週水曜 23:00","content":"放送情報 ..."},
                {"title":"bad","url":"not-a-url","summary":"x","content":"y"}
            ]}"#,
        )
        .unwrap();
        let results = parse_deepseek_tools_response(&value).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://d.example/s");
        assert_eq!(results[0].provider, "deepseek");
        assert!(results[0].snippet.contains("23:00"));
        assert!(results[0].snippet.contains("放送情報"));
    }

    #[test]
    fn parses_deepseek_tools_response_wrapped_in_data() {
        let value: Value = serde_json::from_str(
            r#"{"data":{"results":[{"title":"D","url":"https://d.example/s","summary":"毎週土曜 23:30","content":"text"}]}}"#,
        )
        .unwrap();
        let results = parse_deepseek_tools_response(&value).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].snippet.contains("23:30"));
    }

    #[test]
    fn deepseek_tools_body_has_expected_fields() {
        let body = deepseek_tools_search_body("作品A 放送時間", 10);
        assert_eq!(body["query"], "作品A 放送時間");
        assert_eq!(body["count"], 10);
        assert_eq!(body["need_content"], true);
        assert_eq!(body["ori_question"], "作品A 放送時間");
    }

    #[test]
    fn normalizes_deepseek_search_endpoints() {
        let (endpoint, mode) = normalize_deepseek_search_endpoint("https://api.deepseek.com/v1/");
        assert_eq!(endpoint, "https://api.deepseek.com/v1/tools/search");
        assert_eq!(mode, DeepseekSearchMode::Tools);

        let (endpoint, mode) =
            normalize_deepseek_search_endpoint("https://api.deepseek.com/v1/tools/search");
        assert_eq!(endpoint, "https://api.deepseek.com/v1/tools/search");
        assert_eq!(mode, DeepseekSearchMode::Tools);

        // Legacy configuration keeps the chat-completion pseudo search.
        let (endpoint, mode) =
            normalize_deepseek_search_endpoint("https://api.deepseek.com/v1/chat/completions");
        assert_eq!(endpoint, "https://api.deepseek.com/v1/chat/completions");
        assert_eq!(mode, DeepseekSearchMode::Chat);
    }

    #[test]
    fn parses_deepseek_fenced_json() {
        let content = "```json\n{\n  \"results\": [\n    {\"title\": \"D\", \"url\": \"https://d.example/s\", \"snippet\": \"水曜 23:00\"}\n  ]\n}\n```";
        let results = parse_deepseek_content(content).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://d.example/s");
        assert_eq!(results[0].provider, "deepseek");
    }

    #[test]
    fn rejects_deepseek_non_json() {
        let error = parse_deepseek_content("I could not find anything").unwrap_err();
        assert!(error.message.contains("no JSON"));
    }

    #[test]
    fn deepseek_skips_invalid_urls() {
        let content = r#"{"results":[{"title":"x","url":"not-a-url","snippet":"x"},{"title":"y","url":"https://ok.example","snippet":"y"}]}"#;
        let results = parse_deepseek_content(content).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://ok.example");
    }
}
