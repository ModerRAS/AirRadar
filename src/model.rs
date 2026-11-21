//! Persisted and published data models.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::season::SeasonRef;
use crate::util::is_valid_time;

/// Version of the persisted state, schedule and API schema.
pub const SCHEMA_VERSION: u32 = 1;

/// All published times are JST (Asia/Tokyo, UTC+9, no DST).
pub const SCHEDULE_TIMEZONE: &str = "Asia/Tokyo";

/// Confidence assigned by cross-source verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
    Unknown,
}

impl Default for Confidence {
    fn default() -> Self {
        Confidence::Unknown
    }
}

impl Confidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
            Confidence::Unknown => "unknown",
        }
    }
}

/// Publication status of one subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewStatus {
    /// Resolved and published in the schedule.
    Published,
    /// Conflicting evidence, excluded from the published schedule.
    Review,
    /// No usable time found, excluded from the published schedule.
    Unknown,
}

impl Default for ReviewStatus {
    fn default() -> Self {
        ReviewStatus::Unknown
    }
}

/// Normalised broadcast slot: ISO weekday (1=Mon..7=Sun) + HH:MM JST.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ResolvedSlot {
    pub weekday: u8,
    pub time: String,
}

impl ResolvedSlot {
    pub fn new(weekday: u8, time: impl Into<String>) -> ResolvedSlot {
        ResolvedSlot {
            weekday,
            time: time.into(),
        }
    }
}

/// One observation of a broadcast slot from a single source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimeCandidate {
    #[serde(default)]
    pub weekday: Option<u8>,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(default)]
    pub source_domain: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub evidence: String,
    pub observed_at: DateTime<Utc>,
}

impl TimeCandidate {
    /// Full slot when both weekday and a valid HH:MM time are known.
    pub fn slot(&self) -> Option<ResolvedSlot> {
        let weekday = self.weekday?;
        let time = self.time.as_ref()?;
        if (1..=7).contains(&weekday) && is_valid_time(time) {
            Some(ResolvedSlot::new(weekday, time.clone()))
        } else {
            None
        }
    }

    pub fn has_slot(&self) -> bool {
        self.slot().is_some()
    }
}

/// Latest knowledge about one Bangumi subject.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubjectState {
    pub subject_id: u64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_cn: Option<String>,
    pub season: SeasonRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub air_date: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub official_site: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default)]
    pub candidates: Vec<TimeCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedSlot>,
    /// Weekday-only fallback used when no verified time exists (published with
    /// confidence unknown if PUBLISH_WEEKDAY_ONLY is enabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekday_only: Option<u8>,
    #[serde(default)]
    pub confidence: Confidence,
    #[serde(default)]
    pub status: ReviewStatus,
    #[serde(default)]
    pub review_reasons: Vec<String>,
    /// True when no new evidence was found this run and a previous resolution was kept.
    #[serde(default)]
    pub carried_over: bool,
    pub first_seen_at: DateTime<Utc>,
    pub last_checked_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SubjectState {
    pub fn is_published(&self) -> bool {
        self.status == ReviewStatus::Published
            && (self.resolved.is_some() || self.weekday_only.is_some())
    }
}

/// Aggregate state persisted in /data/state.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFile {
    pub schema_version: u32,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub subjects: BTreeMap<String, SubjectState>,
}

impl StateFile {
    pub fn empty() -> StateFile {
        StateFile {
            schema_version: SCHEMA_VERSION,
            updated_at: Utc::now(),
            subjects: BTreeMap::new(),
        }
    }
}

/// One independent source behind a published item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleSource {
    pub url: String,
    pub domain: String,
    pub provider: String,
}

/// One published schedule row.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleItem {
    pub subject_id: u64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_cn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub air_date: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// ISO weekday of the actual JST calendar day (1=Mon..7=Sun).
    pub weekday: u8,
    /// HH:MM JST, 24h clock; null when only the weekday is known.
    #[serde(default)]
    pub time: Option<String>,
    /// False for weekday-only rows (published with confidence unknown).
    /// Missing in old payloads means true.
    #[serde(default = "default_true")]
    pub time_known: bool,
    pub timezone: String,
    pub confidence: Confidence,
    pub sources: Vec<ScheduleSource>,
    pub updated_at: DateTime<Utc>,
}

fn default_true() -> bool {
    true
}

/// Published schedule document (GET /api/schedule/latest and /data/schedule/<season>.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleFile {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub season: String,
    pub season_start: NaiveDate,
    pub season_end: NaiveDate,
    pub timezone: String,
    pub item_count: usize,
    pub items: Vec<ScheduleItem>,
}

/// One human-review entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewEntry {
    pub subject_id: u64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_cn: Option<String>,
    pub status: ReviewStatus,
    pub confidence: Confidence,
    pub reasons: Vec<String>,
    pub candidates: Vec<TimeCandidate>,
    pub last_checked_at: DateTime<Utc>,
}

/// /data/review.json document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFile {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub season: String,
    pub total: usize,
    pub conflicts: usize,
    pub missing: usize,
    pub items: Vec<ReviewEntry>,
}

/// What started a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunTrigger {
    Schedule,
    Manual,
    Startup,
    Cli,
}

impl RunTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunTrigger::Schedule => "schedule",
            RunTrigger::Manual => "manual",
            RunTrigger::Startup => "startup",
            RunTrigger::Cli => "cli",
        }
    }
}

/// Per search-provider statistics for the last run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStat {
    pub provider: String,
    pub attempts: usize,
    pub queries: usize,
    pub results: usize,
    pub elapsed_ms: u64,
    pub failures: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// LLM fallback statistics for the last run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmStat {
    pub configured: bool,
    pub calls: usize,
    pub cached_hits: usize,
    pub results: usize,
    pub failures: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// One sampled fetch failure (domain only, never a full URL with a path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchFailureSample {
    pub domain: String,
    pub category: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub reason: String,
}

/// Aggregated fetch failure count per detailed category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchFailureCategory {
    pub category: String,
    pub count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example_domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example_reason: Option<String>,
}

/// DeepSeek official server-side page fetch fallback counters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepseekFetchStat {
    pub configured: bool,
    #[serde(default)]
    pub calls: usize,
    #[serde(default)]
    pub successes: usize,
    #[serde(default)]
    pub failures: usize,
    #[serde(default)]
    pub rate_limited: usize,
    #[serde(default)]
    pub budget_exhausted: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// HTTP fetch statistics for the last run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchStat {
    pub requested: usize,
    pub fetched: usize,
    pub from_cache: usize,
    pub failed: usize,
    /// Extra requests made by the one-shot retry.
    #[serde(default)]
    pub retried: usize,
    #[serde(default)]
    pub failures_timeout: usize,
    #[serde(default)]
    pub failures_http: usize,
    #[serde(default)]
    pub failures_other: usize,
    /// Detailed top failure categories for diagnosis (sorted by count desc).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_categories: Vec<FetchFailureCategory>,
    /// Up to five sampled failures (domain + reason) for diagnosis.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_failures: Vec<FetchFailureSample>,
}

/// Result of one collection run (persisted in /data/status.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub run_id: String,
    pub trigger: RunTrigger,
    pub season: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub ok: bool,
    pub subject_list_source: String,
    pub subjects_fetched: usize,
    pub subjects_processed: usize,
    pub items_published: usize,
    /// Published rows that only have a weekday (time unknown).
    #[serde(default)]
    pub weekday_only_count: usize,
    pub high_confidence: usize,
    pub medium_confidence: usize,
    pub review_count: usize,
    pub unknown_count: usize,
    pub search: Vec<ProviderStat>,
    pub llm: LlmStat,
    pub fetch: FetchStat,
    #[serde(default)]
    pub deepseek_fetch: DeepseekFetchStat,
    #[serde(default)]
    pub errors: Vec<String>,
}

/// /data/status.json document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusFile {
    pub schema_version: u32,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub last_run: Option<RunSummary>,
}

impl StatusFile {
    pub fn empty() -> StatusFile {
        StatusFile {
            schema_version: SCHEMA_VERSION,
            updated_at: Utc::now(),
            last_run: None,
        }
    }
}
