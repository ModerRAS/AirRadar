//! Shared application state and the refresh-controller abstraction.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use chrono::{DateTime, Utc};

use crate::cache::{Cache, CacheStats};
use crate::config::Config;
use crate::log;
use crate::model::{RunSummary, RunTrigger};
use crate::season::SeasonRef;
use crate::store::Store;
use crate::VERSION;

/// Boxed future used by the refresh controller trait.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Request to start one collection run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshRequest {
    pub season: Option<SeasonRef>,
    pub trigger: RunTrigger,
}

impl RefreshRequest {
    pub fn new(trigger: RunTrigger, season: Option<SeasonRef>) -> RefreshRequest {
        RefreshRequest { season, trigger }
    }
}

/// Accepted refresh trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerAccepted {
    pub run_id: String,
    pub season: Option<SeasonRef>,
}

/// Trigger failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerError {
    /// Another run is already active.
    Busy,
}

/// Abstraction used by HTTP and scheduler code so tests can inject a fake.
///
/// Implementations must guarantee at most one concurrent run.
pub trait RefreshController: Send + Sync + 'static {
    fn is_running(&self) -> bool;

    /// Start a run in the background, returning immediately.
    fn trigger(self: Arc<Self>, request: RefreshRequest) -> Result<TriggerAccepted, TriggerError>;

    /// Run and wait for completion (scheduler). None when a run is active.
    fn run_blocking(
        self: Arc<Self>,
        request: RefreshRequest,
    ) -> BoxFuture<'static, Option<RunSummary>>;
}

/// Application state shared by every axum handler.
pub struct AppState {
    pub cfg: Arc<Config>,
    pub store: Arc<Store>,
    pub cache: Arc<Cache>,
    pub controller: Arc<dyn RefreshController>,
    pub started_at: Instant,
    pub started_at_utc: DateTime<Utc>,
    /// Search providers enabled by configuration (for /api/status).
    pub search_providers: Vec<String>,
    /// Whether the LLM fallback is configured.
    pub llm_configured: bool,
    next_run_at: RwLock<Option<DateTime<Utc>>>,
}

impl AppState {
    pub fn new(
        cfg: Arc<Config>,
        store: Arc<Store>,
        cache: Arc<Cache>,
        controller: Arc<dyn RefreshController>,
    ) -> AppState {
        AppState {
            cfg,
            store,
            cache,
            controller,
            started_at: Instant::now(),
            started_at_utc: Utc::now(),
            search_providers: Vec::new(),
            llm_configured: false,
            next_run_at: RwLock::new(None),
        }
    }

    pub fn version(&self) -> &'static str {
        VERSION
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    pub fn next_run_at(&self) -> Option<DateTime<Utc>> {
        *self.next_run_at.read().expect("next_run_at lock poisoned")
    }

    pub fn set_next_run_at(&self, value: Option<DateTime<Utc>>) {
        *self.next_run_at.write().expect("next_run_at lock poisoned") = value;
    }

    pub fn cache_stats(&self) -> CacheStats {
        self.cache.stats()
    }

    /// Load the persisted last-run summary, logging (but ignoring) read errors.
    pub fn last_run_summary(&self) -> Option<RunSummary> {
        match self.store.load_status() {
            Ok(status) => status.and_then(|status| status.last_run),
            Err(error) => {
                log::warn(format!("status read failed: {error}"));
                None
            }
        }
    }
}
