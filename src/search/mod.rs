//! Pluggable web-search providers with ordered fallback.
//!
//! Brave, Tavily and DeepSeek Search are configured independently by their
//! API key environment variables. SEARCH_PROVIDER_ORDER controls the order.

mod providers;

pub use providers::{BraveProvider, DeepseekSearchProvider, TavilyProvider};

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use crate::config::Config;

/// Supported search backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Brave,
    Tavily,
    Deepseek,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 3] = [
        ProviderKind::Brave,
        ProviderKind::Tavily,
        ProviderKind::Deepseek,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Brave => "brave",
            ProviderKind::Tavily => "tavily",
            ProviderKind::Deepseek => "deepseek",
        }
    }

    pub fn parse(value: &str) -> Option<ProviderKind> {
        match value.trim().to_ascii_lowercase().as_str() {
            "brave" => Some(ProviderKind::Brave),
            "tavily" => Some(ProviderKind::Tavily),
            "deepseek" | "deepseek-search" | "deepseek_search" => Some(ProviderKind::Deepseek),
            _ => None,
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One normalised web-search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub provider: String,
}

/// Search provider failure. Always recoverable: the router falls back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchError {
    pub message: String,
}

impl SearchError {
    pub fn new(message: impl Into<String>) -> SearchError {
        SearchError {
            message: message.into(),
        }
    }
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SearchError {}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A search backend. Implementations must be cheap to keep alive.
pub trait SearchProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    fn name(&self) -> &'static str {
        self.kind().as_str()
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
    ) -> BoxFuture<'a, Result<Vec<SearchResult>, SearchError>>;
}

/// One provider attempt inside a single logical search.
#[derive(Debug, Clone, Serialize)]
pub struct SearchAttempt {
    pub provider: String,
    pub ok: bool,
    pub results: usize,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Outcome of a logical search after provider fallback.
#[derive(Debug, Clone, Default)]
pub struct SearchOutcome {
    pub results: Vec<SearchResult>,
    pub attempts: Vec<SearchAttempt>,
    pub provider: Option<String>,
}

/// Ordered provider chain.
pub struct SearchRouter {
    providers: Vec<Arc<dyn SearchProvider>>,
    order: Vec<ProviderKind>,
    fallback_on_empty: bool,
}

impl SearchRouter {
    /// Build the chain from configuration. Providers without a key are skipped.
    pub fn from_config(cfg: &Config) -> Result<SearchRouter, SearchError> {
        let mut providers: Vec<Arc<dyn SearchProvider>> = Vec::new();
        for kind in &cfg.search_order {
            match kind {
                ProviderKind::Brave => {
                    if let Some(key) = cfg.brave_api_key.clone() {
                        providers.push(Arc::new(BraveProvider::new(
                            key,
                            &cfg.fetch_ua,
                            cfg.search_timeout_secs,
                        )?));
                    }
                }
                ProviderKind::Tavily => {
                    if let Some(key) = cfg.tavily_api_key.clone() {
                        providers.push(Arc::new(TavilyProvider::new(
                            key,
                            &cfg.fetch_ua,
                            cfg.search_timeout_secs,
                        )?));
                    }
                }
                ProviderKind::Deepseek => {
                    if let Some(key) = cfg.deepseek_api_key.clone() {
                        providers.push(Arc::new(DeepseekSearchProvider::new(
                            key,
                            cfg.deepseek_search_url.clone(),
                            cfg.deepseek_search_model.clone(),
                            cfg.deepseek_search_count,
                            &cfg.fetch_ua,
                            cfg.search_timeout_secs,
                        )?));
                    }
                }
            }
        }
        Ok(SearchRouter {
            providers,
            order: cfg.search_order.clone(),
            fallback_on_empty: cfg.search_fallback_on_empty,
        })
    }

    /// Explicit construction, mainly for tests.
    pub fn from_providers(
        providers: Vec<Arc<dyn SearchProvider>>,
        order: Vec<ProviderKind>,
        fallback_on_empty: bool,
    ) -> SearchRouter {
        SearchRouter {
            providers,
            order,
            fallback_on_empty,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    pub fn configured(&self) -> Vec<&'static str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    pub fn order(&self) -> Vec<&'static str> {
        self.order.iter().map(|k| k.as_str()).collect()
    }

    /// Run one logical search with ordered fallback.
    ///
    /// A provider error always falls back to the next provider. Empty results
    /// fall back only when SEARCH_FALLBACK_ON_EMPTY=true, to keep API cost low.
    pub async fn search(&self, query: &str) -> SearchOutcome {
        let mut outcome = SearchOutcome::default();
        for provider in &self.providers {
            let started = Instant::now();
            let result = provider.search(query).await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            match result {
                Ok(results) if results.is_empty() => {
                    outcome.attempts.push(SearchAttempt {
                        provider: provider.name().to_string(),
                        ok: true,
                        results: 0,
                        elapsed_ms,
                        error: None,
                    });
                    if self.fallback_on_empty {
                        continue;
                    }
                    outcome.provider = Some(provider.name().to_string());
                    return outcome;
                }
                Ok(results) => {
                    outcome.attempts.push(SearchAttempt {
                        provider: provider.name().to_string(),
                        ok: true,
                        results: results.len(),
                        elapsed_ms,
                        error: None,
                    });
                    outcome.results = results;
                    outcome.provider = Some(provider.name().to_string());
                    return outcome;
                }
                Err(error) => {
                    outcome.attempts.push(SearchAttempt {
                        provider: provider.name().to_string(),
                        ok: false,
                        results: 0,
                        elapsed_ms,
                        error: Some(error.message),
                    });
                }
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockProvider {
        kind: ProviderKind,
        results: Vec<SearchResult>,
        error: Option<String>,
        calls: Arc<AtomicUsize>,
    }

    impl SearchProvider for MockProvider {
        fn kind(&self) -> ProviderKind {
            self.kind
        }

        fn search<'a>(
            &'a self,
            _query: &'a str,
        ) -> BoxFuture<'a, Result<Vec<SearchResult>, SearchError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let outcome = match &self.error {
                Some(message) => Err(SearchError::new(message.clone())),
                None => Ok(self.results.clone()),
            };
            Box::pin(async move { outcome })
        }
    }

    fn result(title: &str) -> SearchResult {
        SearchResult {
            title: title.to_string(),
            url: format!("https://example.com/{title}"),
            snippet: format!("{title} 毎週土曜 23:30"),
            provider: "mock".to_string(),
        }
    }

    #[tokio::test]
    async fn falls_back_on_error() {
        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));
        let router = SearchRouter::from_providers(
            vec![
                Arc::new(MockProvider {
                    kind: ProviderKind::Brave,
                    results: vec![],
                    error: Some("boom".to_string()),
                    calls: first_calls.clone(),
                }),
                Arc::new(MockProvider {
                    kind: ProviderKind::Tavily,
                    results: vec![result("a")],
                    error: None,
                    calls: second_calls.clone(),
                }),
            ],
            vec![ProviderKind::Brave, ProviderKind::Tavily],
            false,
        );
        let outcome = router.search("q").await;
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.provider.as_deref(), Some("tavily"));
        assert_eq!(outcome.attempts.len(), 2);
        assert!(!outcome.attempts[0].ok);
        assert!(outcome.attempts[1].ok);
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stops_on_success_by_default() {
        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));
        let router = SearchRouter::from_providers(
            vec![
                Arc::new(MockProvider {
                    kind: ProviderKind::Brave,
                    results: vec![result("a")],
                    error: None,
                    calls: first_calls.clone(),
                }),
                Arc::new(MockProvider {
                    kind: ProviderKind::Tavily,
                    results: vec![result("b")],
                    error: None,
                    calls: second_calls.clone(),
                }),
            ],
            vec![ProviderKind::Brave, ProviderKind::Tavily],
            false,
        );
        let outcome = router.search("q").await;
        assert_eq!(outcome.provider.as_deref(), Some("brave"));
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn search_does_not_fall_back_to_llm_key() {
        let mut cfg = Config::for_tests(PathBuf::from("./data-test"));
        cfg.search_order = vec![ProviderKind::Deepseek];
        cfg.deepseek_api_key = None;
        cfg.llm_base_url = Some("https://opencode.ai/inference/go/v1".to_string());
        cfg.llm_api_key = Some("opencode-go-key".to_string());
        cfg.llm_model = Some("model".to_string());
        cfg.llm_session_id = Some("session".to_string());
        let router = SearchRouter::from_config(&cfg).unwrap();
        assert!(
            router.is_empty(),
            "the LLM key must never enable a search provider"
        );
        assert!(router.configured().is_empty());
    }

    #[test]
    fn deepseek_search_only_reads_deepseek_key() {
        let mut cfg = Config::for_tests(PathBuf::from("./data-test"));
        cfg.search_order = vec![ProviderKind::Deepseek];
        cfg.deepseek_api_key = Some("deepseek-search-key".to_string());
        cfg.llm_base_url = Some("https://opencode.ai/inference/go/v1".to_string());
        cfg.llm_api_key = Some("opencode-go-key".to_string());
        cfg.llm_model = Some("model".to_string());
        cfg.llm_session_id = Some("session".to_string());
        let router = SearchRouter::from_config(&cfg).unwrap();
        assert_eq!(router.configured(), vec!["deepseek"]);
    }

    #[tokio::test]
    async fn empty_results_only_fall_back_when_enabled() {
        let make = |fallback_on_empty: bool| {
            SearchRouter::from_providers(
                vec![
                    Arc::new(MockProvider {
                        kind: ProviderKind::Brave,
                        results: vec![],
                        error: None,
                        calls: Arc::new(AtomicUsize::new(0)),
                    }),
                    Arc::new(MockProvider {
                        kind: ProviderKind::Tavily,
                        results: vec![result("b")],
                        error: None,
                        calls: Arc::new(AtomicUsize::new(0)),
                    }),
                ],
                vec![ProviderKind::Brave, ProviderKind::Tavily],
                fallback_on_empty,
            )
        };
        let strict = make(false).search("q").await;
        assert!(strict.results.is_empty());
        assert_eq!(strict.provider.as_deref(), Some("brave"));
        let lenient = make(true).search("q").await;
        assert_eq!(lenient.results.len(), 1);
        assert_eq!(lenient.provider.as_deref(), Some("tavily"));
    }
}
