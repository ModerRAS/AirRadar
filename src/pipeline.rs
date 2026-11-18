//! Collection pipeline and refresh coordinator.
//!
//! One run:
//! 1. load previous state and the current-season Bangumi subject list;
//! 2. rotate the MAX_SUBJECTS_PER_RUN window by last-checked time;
//! 3. per subject: search (Brave/Tavily/DeepSeek), fetch pages, extract rules;
//! 4. optional batched LLM fallback for subjects still unresolved;
//! 5. cross-source verification, state merge, publish schedule + review files.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::app::{BoxFuture, RefreshController, RefreshRequest, TriggerAccepted, TriggerError};
use crate::bangumi::BangumiClient;
use crate::cache::Cache;
use crate::config::Config;
use crate::extract::{self, RawSlot};
use crate::fetch::Fetcher;
use crate::llm::{LlmClient, LlmRequest, LlmRunStats, LlmSuggestion};
use crate::log;
use crate::model::{
    Confidence, FetchStat, LlmStat, ProviderStat, ReviewEntry, ReviewFile, ReviewStatus,
    RunSummary, RunTrigger, ScheduleFile, ScheduleItem, ScheduleSource, StateFile, StatusFile,
    SubjectState, TimeCandidate, SCHEDULE_TIMEZONE, SCHEMA_VERSION,
};
use crate::search::{SearchAttempt, SearchResult, SearchRouter};
use crate::season::SeasonRef;
use crate::store::Store;
use crate::util::{collapse_ws, domain_of, is_http_url, new_run_id, truncate_chars};
use crate::verify::{self, Verification};

/// Hosts that never contain usable schedule text (social networks/video).
const BLOCKED_HOSTS: &[&str] = &[
    "twitter.com",
    "x.com",
    "facebook.com",
    "instagram.com",
    "youtube.com",
    "youtu.be",
    "tiktok.com",
    "reddit.com",
    "weibo.com",
    "pinterest.com",
    "threads.net",
];

fn is_blocked_host(domain: &str) -> bool {
    BLOCKED_HOSTS
        .iter()
        .any(|blocked| domain == *blocked || domain.ends_with(&format!(".{blocked}")))
}

fn try_take(counter: &AtomicUsize) -> bool {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        if current == 0 {
            return false;
        }
        match counter.compare_exchange_weak(
            current,
            current - 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

fn subject_names(subject: &crate::bangumi::BangumiSubject) -> Vec<String> {
    let mut names = vec![subject.name.clone()];
    if let Some(name_cn) = &subject.name_cn {
        if !name_cn.trim().is_empty() && name_cn != &subject.name {
            names.push(name_cn.clone());
        }
    }
    names
}

fn apply_publish_policy(verification: &mut Verification, publish_medium: bool) {
    if verification.status == ReviewStatus::Published
        && verification.confidence == Confidence::Medium
        && !publish_medium
    {
        verification.status = ReviewStatus::Review;
        verification
            .reasons
            .push("medium_confidence_not_published".to_string());
    }
}

/// One extracted slot printed by the --probe-url debugging command.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeSlot {
    pub weekday: u8,
    pub time: Option<String>,
    pub method: String,
    pub matched: String,
}

/// Result of the --probe-url debugging command.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeReport {
    pub url: String,
    pub from_cache: bool,
    pub text_chars: usize,
    pub truncated: bool,
    pub slots: Vec<ProbeSlot>,
    pub preview: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub trigger: RunTrigger,
    pub season: Option<SeasonRef>,
    pub max_subjects: Option<usize>,
    pub run_id: Option<String>,
}

impl RunOptions {
    pub fn new(trigger: RunTrigger) -> RunOptions {
        RunOptions {
            trigger,
            season: None,
            max_subjects: None,
            run_id: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct FetchCounters {
    requested: usize,
    fetched: usize,
    cached: usize,
    failed: usize,
}

#[derive(Debug)]
struct SubjectOutcome {
    subject: crate::bangumi::BangumiSubject,
    candidates: Vec<TimeCandidate>,
    verification: Verification,
    search_attempts: Vec<SearchAttempt>,
    fetch: FetchCounters,
    llm_text: String,
}

/// Collection pipeline. Build once at process start.
pub struct Pipeline {
    cfg: Arc<Config>,
    store: Arc<Store>,
    cache: Arc<Cache>,
    fetcher: Arc<Fetcher>,
    search: Arc<SearchRouter>,
    llm: Option<Arc<LlmClient>>,
    bangumi: Arc<BangumiClient>,
}

impl Pipeline {
    pub fn new(cfg: Arc<Config>) -> Result<Arc<Pipeline>, String> {
        std::fs::create_dir_all(&cfg.data_dir)
            .map_err(|error| format!("create data dir {:?}: {error}", cfg.data_dir))?;
        let store = Arc::new(Store::new(cfg.data_dir.clone()));
        store
            .ensure_dirs()
            .map_err(|error| format!("prepare data dirs: {error}"))?;
        let ttl = if cfg.cache_ttl_hours > 0.0 {
            Some(std::time::Duration::from_secs_f64(
                cfg.cache_ttl_hours * 3600.0,
            ))
        } else {
            None
        };
        let cache = Arc::new(Cache::new(cfg.cache_dir.clone(), cfg.cache_max_bytes, ttl));
        let fetcher = Arc::new(
            Fetcher::new(&cfg, cache.clone()).map_err(|error| format!("build fetcher: {error}"))?,
        );
        let search = Arc::new(
            SearchRouter::from_config(&cfg).map_err(|error| format!("build search: {error}"))?,
        );
        let llm = LlmClient::from_config(&cfg, store.clone())
            .map_err(|error| format!("build llm client: {error}"))?
            .map(Arc::new);
        let bangumi = Arc::new(
            BangumiClient::new(&cfg).map_err(|error| format!("build bangumi client: {error}"))?,
        );
        Ok(Arc::new(Pipeline {
            cfg,
            store,
            cache,
            fetcher,
            search,
            llm,
            bangumi,
        }))
    }

    pub fn config(&self) -> &Arc<Config> {
        &self.cfg
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn cache(&self) -> &Arc<Cache> {
        &self.cache
    }

    pub fn search_configured(&self) -> Vec<&'static str> {
        self.search.configured()
    }

    pub fn llm_configured(&self) -> bool {
        self.llm.is_some()
    }

    /// Fetch one URL, convert it to text and print every extracted slot.
    /// Intended for operators debugging why a source did or did not match.
    pub async fn probe_url(&self, url: &str) -> Result<ProbeReport, String> {
        let page = self
            .fetcher
            .get_text(url)
            .await
            .map_err(|error| format!("fetch failed: {error}"))?;
        let slots = extract::extract_slots(&page.text)
            .into_iter()
            .map(|slot| ProbeSlot {
                weekday: slot.weekday,
                time: slot.time,
                method: slot.method.to_string(),
                matched: slot.matched,
            })
            .collect();
        let preview = page
            .text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .take(8)
            .map(|line| truncate_chars(line, 160).to_string())
            .collect();
        Ok(ProbeReport {
            url: url.to_string(),
            from_cache: page.from_cache,
            text_chars: page.text.chars().count(),
            truncated: page.truncated,
            slots,
            preview,
        })
    }

    /// Execute one collection run.
    pub async fn run(self: Arc<Self>, options: RunOptions) -> RunSummary {
        let started = Utc::now();
        let started_instant = Instant::now();
        let run_id = options.run_id.clone().unwrap_or_else(new_run_id);
        let season = options
            .season
            .or(self.cfg.season_override)
            .unwrap_or_else(|| SeasonRef::current(started));
        let max_subjects = options
            .max_subjects
            .unwrap_or(self.cfg.max_subjects_per_run)
            .max(1);
        log::info(format!(
            "run {run_id}: start (trigger={}, season={season}, maxSubjects={max_subjects})",
            options.trigger.as_str()
        ));
        let mut summary = RunSummary {
            run_id: run_id.clone(),
            trigger: options.trigger,
            season: season.label(),
            started_at: started,
            finished_at: started,
            duration_ms: 0,
            ok: false,
            subject_list_source: String::new(),
            subjects_fetched: 0,
            subjects_processed: 0,
            items_published: 0,
            high_confidence: 0,
            medium_confidence: 0,
            review_count: 0,
            unknown_count: 0,
            search: Vec::new(),
            llm: LlmStat {
                configured: self.llm.is_some(),
                ..LlmStat::default()
            },
            fetch: FetchStat::default(),
            errors: Vec::new(),
        };
        let previous = self.load_state();
        match self
            .bangumi
            .fetch_season_list(&season, max_subjects.saturating_mul(3).min(300))
            .await
        {
            Ok((subjects, source)) => {
                summary.subject_list_source = source.as_str().to_string();
                summary.subjects_fetched = subjects.len();
                log::info(format!(
                    "run {run_id}: {} subjects via {}",
                    subjects.len(),
                    source.as_str()
                ));
                let filtered: Vec<crate::bangumi::BangumiSubject> = subjects
                    .into_iter()
                    .filter(|subject| self.platform_allowed(subject.platform.as_deref()))
                    .collect();
                let allowed_count = filtered.len();
                let selected = self.select_subjects(filtered, &previous, max_subjects);
                log::info(format!(
                    "run {run_id}: processing {} of {} platform-allowed subjects",
                    selected.len(),
                    allowed_count
                ));
                let (outcomes, summary_updates) = self.process_all(selected, &season).await;
                summary.subjects_processed = outcomes.len();
                summary.errors.extend(summary_updates);
                self.complete_run(summary, previous, outcomes, &season, started_instant)
                    .await
            }
            Err(error) => {
                summary.errors.push(format!("bangumi: {error}"));
                log::error(format!("run {run_id}: season list failed: {error}"));
                self.finalize(summary, started_instant)
            }
        }
    }

    async fn process_all(
        self: &Arc<Self>,
        selected: Vec<crate::bangumi::BangumiSubject>,
        season: &SeasonRef,
    ) -> (Vec<SubjectOutcome>, Vec<String>) {
        let semaphore = Arc::new(Semaphore::new(self.cfg.fetch_concurrency.clamp(1, 4)));
        let budget = Arc::new(AtomicUsize::new(self.cfg.max_search_calls));
        let mut joinset: JoinSet<SubjectOutcome> = JoinSet::new();
        for subject in selected {
            let this = self.clone();
            let semaphore = semaphore.clone();
            let budget = budget.clone();
            let season = *season;
            joinset.spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .expect("subject semaphore stays open");
                this.process_subject(subject, season, budget).await
            });
        }
        let mut outcomes = Vec::new();
        let mut errors = Vec::new();
        while let Some(joined) = joinset.join_next().await {
            match joined {
                Ok(outcome) => outcomes.push(outcome),
                Err(error) => errors.push(format!("worker task failed: {error}")),
            }
        }
        (outcomes, errors)
    }

    async fn complete_run(
        self: &Arc<Self>,
        mut summary: RunSummary,
        previous: StateFile,
        mut outcomes: Vec<SubjectOutcome>,
        season: &SeasonRef,
        started_instant: Instant,
    ) -> RunSummary {
        if self.llm.is_some() {
            let stats = self.apply_llm(&mut outcomes).await;
            summary.llm = LlmStat {
                configured: true,
                calls: stats.calls,
                cached_hits: stats.cached_hits,
                results: stats.results,
                failures: stats.failures,
                last_error: stats.last_error,
            };
        }
        summary.search = aggregate_search(&outcomes);
        summary.fetch = aggregate_fetch(&outcomes);

        let mut state = previous;
        let now = Utc::now();
        for outcome in &outcomes {
            let key = outcome.subject.id.to_string();
            let existing = state.subjects.get(&key).cloned();
            let entry = self.merge_subject(outcome, existing, season, now);
            state.subjects.insert(key, entry);
        }
        let cutoff = season.rank() - self.cfg.state_retention_seasons as i64 + 1;
        state
            .subjects
            .retain(|_, subject| subject.season.rank() >= cutoff);
        state.updated_at = now;
        if let Err(error) = self.store.save_state(&state) {
            summary.errors.push(format!("state: {error}"));
        }

        let schedule = self.build_schedule(&state, season, now);
        if let Err(error) = self.store.publish(&schedule, self.cfg.history_keep) {
            summary.errors.push(format!("publish: {error}"));
        }
        let review = self.build_review(&state, season, now);
        if let Err(error) = self.store.save_reviews(&review) {
            summary.errors.push(format!("review: {error}"));
        }
        summary.items_published = schedule.items.len();
        summary.high_confidence = schedule
            .items
            .iter()
            .filter(|item| item.confidence == Confidence::High)
            .count();
        summary.medium_confidence = schedule
            .items
            .iter()
            .filter(|item| item.confidence == Confidence::Medium)
            .count();
        summary.review_count = review.conflicts;
        summary.unknown_count = review.missing;
        self.finalize(summary, started_instant)
    }

    fn finalize(&self, mut summary: RunSummary, started_instant: Instant) -> RunSummary {
        summary.finished_at = Utc::now();
        summary.duration_ms = started_instant.elapsed().as_millis() as u64;
        summary.ok = summary.errors.is_empty();
        let status = StatusFile {
            schema_version: SCHEMA_VERSION,
            updated_at: Utc::now(),
            last_run: Some(summary.clone()),
        };
        if let Err(error) = self.store.save_status(&status) {
            log::error(format!("status write failed: {error}"));
            summary.errors.push(format!("status: {error}"));
            summary.ok = false;
        }
        log::info(format!(
            "run {}: finished ok={} in {}ms (subjects: fetched={} processed={}, items: published={} high={} medium={}, review={}, unknown={})",
            summary.run_id,
            summary.ok,
            summary.duration_ms,
            summary.subjects_fetched,
            summary.subjects_processed,
            summary.items_published,
            summary.high_confidence,
            summary.medium_confidence,
            summary.review_count,
            summary.unknown_count
        ));
        for error in &summary.errors {
            log::warn(format!("run {}: {error}", summary.run_id));
        }
        summary
    }

    fn load_state(&self) -> StateFile {
        match self.store.load_state() {
            Ok(Some(state)) => state,
            Ok(None) => StateFile::empty(),
            Err(error) => {
                log::warn(format!("state load failed, starting fresh: {error}"));
                StateFile::empty()
            }
        }
    }

    fn platform_allowed(&self, platform: Option<&str>) -> bool {
        if self.cfg.include_platforms.is_empty() {
            return true;
        }
        match platform {
            Some(platform) => self
                .cfg
                .include_platforms
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(platform)),
            None => true,
        }
    }

    /// Rotate the processing window: least recently checked subjects first.
    fn select_subjects(
        &self,
        subjects: Vec<crate::bangumi::BangumiSubject>,
        previous: &StateFile,
        max: usize,
    ) -> Vec<crate::bangumi::BangumiSubject> {
        let mut scored: Vec<(i64, crate::bangumi::BangumiSubject)> = subjects
            .into_iter()
            .map(|subject| {
                let last_checked = previous
                    .subjects
                    .get(&subject.id.to_string())
                    .map(|state| state.last_checked_at.timestamp())
                    .unwrap_or(0);
                (last_checked, subject)
            })
            .collect();
        scored.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.air_date.cmp(&right.1.air_date))
                .then_with(|| left.1.id.cmp(&right.1.id))
        });
        scored
            .into_iter()
            .take(max)
            .map(|(_, subject)| subject)
            .collect()
    }

    fn build_queries(&self, subject: &crate::bangumi::BangumiSubject) -> Vec<String> {
        let mut queries = Vec::new();
        if !subject.name.trim().is_empty() {
            queries.push(format!("{} 放送時間 毎週", subject.name));
        }
        if let Some(name_cn) = &subject.name_cn {
            if !name_cn.trim().is_empty() && name_cn != &subject.name {
                queries.push(format!("{} 播出时间 每周", name_cn));
            }
        }
        if !subject.name.trim().is_empty() {
            queries.push(format!("{} 放送日 スケジュール", subject.name));
        }
        queries.truncate(self.cfg.search_queries_per_subject);
        queries
    }

    async fn process_subject(
        &self,
        subject: crate::bangumi::BangumiSubject,
        season: SeasonRef,
        search_budget: Arc<AtomicUsize>,
    ) -> SubjectOutcome {
        let _ = season;
        let names = subject_names(&subject);
        let mut candidates: Vec<TimeCandidate> = Vec::new();
        let now = Utc::now();
        if let Some(weekday) = subject.weekday_hint {
            candidates.push(TimeCandidate {
                weekday: Some(weekday),
                time: None,
                source_url: None,
                source_domain: "bangumi".to_string(),
                provider: "bangumi".to_string(),
                method: "infobox:weekday".to_string(),
                evidence: "Bangumi 放送星期".to_string(),
                observed_at: now,
            });
        }
        let mut attempts: Vec<SearchAttempt> = Vec::new();
        let mut llm_parts: Vec<String> = Vec::new();
        let mut fetch = FetchCounters::default();
        let mut fetched_urls: BTreeSet<String> = BTreeSet::new();

        // 1) Web search (snippets).
        let mut results: Vec<SearchResult> = Vec::new();
        for query in self.build_queries(&subject) {
            if !try_take(&search_budget) {
                log::debug("search budget exhausted for this run");
                break;
            }
            let outcome = self.search.search(&query).await;
            attempts.extend(outcome.attempts);
            results.extend(outcome.results);
        }
        let mut seen_urls = BTreeSet::new();
        results.retain(|result| is_http_url(&result.url) && seen_urls.insert(result.url.clone()));
        results.truncate(10);
        for result in &results {
            let domain = domain_of(&result.url);
            if !result.snippet.is_empty() {
                llm_parts.push(format!("{} {}", result.title, result.snippet));
                for slot in extract::extract_for_subject(&result.snippet, &names) {
                    add_candidate(
                        &mut candidates,
                        &slot,
                        &result.url,
                        &domain,
                        &result.provider,
                        "snippet",
                        &result.snippet,
                    );
                }
            }
        }

        // 2) Fetch candidate pages (bounded, domain-deduped).
        let mut used_domains: BTreeSet<String> = BTreeSet::new();
        for result in &results {
            if fetch.fetched + fetch.cached >= self.cfg.fetch_pages_per_subject {
                break;
            }
            let domain = domain_of(&result.url);
            if is_blocked_host(&domain) || !used_domains.insert(domain.clone()) {
                continue;
            }
            if !fetched_urls.insert(result.url.clone()) {
                continue;
            }
            fetch.requested += 1;
            match self.fetcher.get_text(&result.url).await {
                Ok(page) => {
                    if page.from_cache {
                        fetch.cached += 1;
                    } else {
                        fetch.fetched += 1;
                    }
                    llm_parts.push(format!(
                        "{} {}",
                        result.title,
                        truncate_chars(&page.text, 1500)
                    ));
                    for slot in extract::extract_for_subject(&page.text, &names) {
                        add_candidate(
                            &mut candidates,
                            &slot,
                            &result.url,
                            &domain,
                            &result.provider,
                            "page",
                            &page.text,
                        );
                    }
                }
                Err(error) => {
                    fetch.failed += 1;
                    log::debug(format!("fetch failed for {}: {error}", result.url));
                }
            }
        }

        // 3) Official website (dedicated to one show -> unfiltered extraction).
        if self.cfg.enable_official_site {
            if let Some(site) = subject.official_site.clone() {
                let domain = domain_of(&site);
                if is_http_url(&site)
                    && !is_blocked_host(&domain)
                    && fetched_urls.insert(site.clone())
                {
                    fetch.requested += 1;
                    match self.fetcher.get_text(&site).await {
                        Ok(page) => {
                            if page.from_cache {
                                fetch.cached += 1;
                            } else {
                                fetch.fetched += 1;
                            }
                            llm_parts.push(format!(
                                "{} {}",
                                subject.name,
                                truncate_chars(&page.text, 1500)
                            ));
                            // A dedicated official site describes one show, so
                            // schedule text may live under a heading far from the
                            // title; extract without the title-context filter.
                            let slots = extract::extract_slots(&page.text);
                            for slot in slots {
                                add_candidate(
                                    &mut candidates,
                                    &slot,
                                    &site,
                                    &domain,
                                    "official_site",
                                    "page",
                                    &page.text,
                                );
                            }
                        }
                        Err(error) => {
                            fetch.failed += 1;
                            log::debug(format!("official site fetch failed for {site}: {error}"));
                        }
                    }
                }
            }
        }

        candidates.truncate(self.cfg.max_candidates_per_subject);
        let mut verification = verify::verify(&candidates);
        apply_publish_policy(&mut verification, self.cfg.publish_medium);
        let llm_text = build_llm_text(&llm_parts, self.cfg.llm_max_input_chars);
        SubjectOutcome {
            subject,
            candidates,
            verification,
            search_attempts: attempts,
            fetch,
            llm_text,
        }
    }

    async fn apply_llm(self: &Arc<Self>, outcomes: &mut [SubjectOutcome]) -> LlmRunStats {
        let Some(llm) = &self.llm else {
            return LlmRunStats::default();
        };
        let mut requests: Vec<LlmRequest> = Vec::new();
        for outcome in outcomes.iter() {
            if outcome.verification.is_published() {
                continue;
            }
            if outcome.llm_text.chars().count() < 20 {
                continue;
            }
            requests.push(LlmRequest {
                subject_id: outcome.subject.id,
                title: outcome.subject.name.clone(),
                text: outcome.llm_text.clone(),
            });
        }
        if requests.is_empty() {
            return LlmRunStats {
                configured: true,
                ..LlmRunStats::default()
            };
        }
        log::info(format!("llm fallback: {} subjects", requests.len()));
        let (suggestions, stats) = llm.extract(&requests).await;
        if suggestions.is_empty() {
            log::info("llm fallback returned no suggestions");
            return stats;
        }
        let mut by_subject: BTreeMap<u64, Vec<LlmSuggestion>> = BTreeMap::new();
        for suggestion in suggestions {
            by_subject
                .entry(suggestion.subject_id)
                .or_default()
                .push(suggestion);
        }
        let now = Utc::now();
        for outcome in outcomes.iter_mut() {
            let Some(list) = by_subject.get(&outcome.subject.id) else {
                continue;
            };
            for suggestion in list {
                let Some(weekday) = suggestion.weekday else {
                    continue;
                };
                let candidate = TimeCandidate {
                    weekday: Some(weekday),
                    time: suggestion.time.clone(),
                    source_url: None,
                    source_domain: "llm".to_string(),
                    provider: "llm".to_string(),
                    method: "llm".to_string(),
                    evidence: format!("LLM: {}", truncate_chars(&suggestion.reason, 160)),
                    observed_at: now,
                };
                if !outcome.candidates.iter().any(|existing| {
                    existing.weekday == candidate.weekday
                        && existing.time == candidate.time
                        && existing.source_domain == candidate.source_domain
                }) {
                    outcome.candidates.push(candidate);
                }
            }
            outcome
                .candidates
                .truncate(self.cfg.max_candidates_per_subject);
            outcome.verification = verify::verify(&outcome.candidates);
            apply_publish_policy(&mut outcome.verification, self.cfg.publish_medium);
        }
        stats
    }

    fn merge_subject(
        &self,
        outcome: &SubjectOutcome,
        existing: Option<SubjectState>,
        season: &SeasonRef,
        now: DateTime<Utc>,
    ) -> SubjectState {
        let subject = &outcome.subject;
        let mut entry = SubjectState {
            subject_id: subject.id,
            name: subject.name.clone(),
            name_cn: subject.name_cn.clone(),
            season: *season,
            air_date: subject.air_date,
            image: subject.image.clone(),
            official_site: subject.official_site.clone(),
            platform: subject.platform.clone(),
            candidates: outcome.candidates.clone(),
            resolved: outcome.verification.resolved.clone(),
            confidence: outcome.verification.confidence,
            status: outcome.verification.status,
            review_reasons: outcome.verification.reasons.clone(),
            carried_over: false,
            first_seen_at: existing
                .as_ref()
                .map(|previous| previous.first_seen_at)
                .unwrap_or(now),
            last_checked_at: now,
            updated_at: now,
        };
        // Keep a previously resolved slot when a run finds no new evidence at
        // all (transient search/fetch failures must not erase published data).
        if entry.candidates.is_empty() && entry.resolved.is_none() {
            if let Some(previous) = &existing {
                if previous.resolved.is_some() {
                    entry.candidates = previous.candidates.clone();
                    entry.resolved = previous.resolved.clone();
                    entry.confidence = previous.confidence;
                    entry.status = previous.status;
                    entry.review_reasons = previous.review_reasons.clone();
                    entry.carried_over = true;
                    entry.updated_at = previous.updated_at;
                }
            }
        }
        if let (Some(resolved), Some(previous)) = (&entry.resolved, &existing) {
            if previous.resolved.as_ref() == Some(resolved) {
                entry.updated_at = previous.updated_at;
            }
        }
        entry
    }

    fn build_schedule(
        &self,
        state: &StateFile,
        season: &SeasonRef,
        generated_at: DateTime<Utc>,
    ) -> ScheduleFile {
        let mut items: Vec<ScheduleItem> = state
            .subjects
            .values()
            .filter(|subject| &subject.season == season && subject.is_published())
            .filter_map(|subject| {
                let resolved = subject.resolved.as_ref()?;
                Some(ScheduleItem {
                    subject_id: subject.subject_id,
                    name: subject.name.clone(),
                    name_cn: subject.name_cn.clone(),
                    air_date: subject.air_date,
                    image: subject.image.clone(),
                    weekday: resolved.weekday,
                    time: resolved.time.clone(),
                    timezone: SCHEDULE_TIMEZONE.to_string(),
                    confidence: subject.confidence,
                    sources: collect_sources(subject, resolved),
                    updated_at: subject.updated_at,
                })
            })
            .collect();
        items.sort_by(|left, right| {
            left.weekday
                .cmp(&right.weekday)
                .then_with(|| left.time.cmp(&right.time))
                .then_with(|| left.name.cmp(&right.name))
        });
        ScheduleFile {
            schema_version: SCHEMA_VERSION,
            generated_at,
            season: season.label(),
            season_start: season.start(),
            season_end: season.end_inclusive(),
            timezone: SCHEDULE_TIMEZONE.to_string(),
            item_count: items.len(),
            items,
        }
    }

    fn build_review(
        &self,
        state: &StateFile,
        season: &SeasonRef,
        generated_at: DateTime<Utc>,
    ) -> ReviewFile {
        let mut items: Vec<ReviewEntry> = state
            .subjects
            .values()
            .filter(|subject| &subject.season == season && !subject.is_published())
            .map(|subject| ReviewEntry {
                subject_id: subject.subject_id,
                name: subject.name.clone(),
                name_cn: subject.name_cn.clone(),
                status: subject.status,
                confidence: subject.confidence,
                reasons: subject.review_reasons.clone(),
                candidates: subject.candidates.clone(),
                last_checked_at: subject.last_checked_at,
            })
            .collect();
        items.sort_by(|left, right| {
            let left_rank = if left.status == ReviewStatus::Review {
                0
            } else {
                1
            };
            let right_rank = if right.status == ReviewStatus::Review {
                0
            } else {
                1
            };
            left_rank
                .cmp(&right_rank)
                .then_with(|| left.name.cmp(&right.name))
        });
        let conflicts = items
            .iter()
            .filter(|item| item.status == ReviewStatus::Review)
            .count();
        let missing = items.len().saturating_sub(conflicts);
        ReviewFile {
            schema_version: SCHEMA_VERSION,
            generated_at,
            season: season.label(),
            total: items.len(),
            conflicts,
            missing,
            items,
        }
    }
}

fn add_candidate(
    candidates: &mut Vec<TimeCandidate>,
    slot: &RawSlot,
    url: &str,
    domain: &str,
    provider: &str,
    kind: &str,
    source_text: &str,
) {
    let evidence = if source_text.is_empty() {
        slot.matched.clone()
    } else {
        format!("{} [{}]", slot.matched, truncate_chars(source_text, 120))
    };
    let candidate = TimeCandidate {
        weekday: Some(slot.weekday),
        time: slot.time.clone(),
        source_url: Some(url.to_string()),
        source_domain: domain.to_string(),
        provider: provider.to_string(),
        method: format!("{}|{}", slot.method, kind),
        evidence,
        observed_at: Utc::now(),
    };
    if candidates.iter().any(|existing| {
        existing.weekday == candidate.weekday
            && existing.time == candidate.time
            && existing.source_domain == candidate.source_domain
            && existing.source_url == candidate.source_url
    }) {
        return;
    }
    candidates.push(candidate);
}

fn build_llm_text(parts: &[String], max_chars: usize) -> String {
    let joined = collapse_ws(&parts.iter().cloned().collect::<Vec<_>>().join(" \n "));
    truncate_chars(&joined, max_chars).to_string()
}

fn collect_sources(
    subject: &SubjectState,
    resolved: &crate::model::ResolvedSlot,
) -> Vec<ScheduleSource> {
    let mut seen_domains = BTreeSet::new();
    let mut sources = Vec::new();
    for candidate in &subject.candidates {
        if candidate.slot().as_ref() != Some(resolved) {
            continue;
        }
        let Some(url) = &candidate.source_url else {
            continue;
        };
        if seen_domains.insert(candidate.source_domain.clone()) {
            sources.push(ScheduleSource {
                url: url.clone(),
                domain: candidate.source_domain.clone(),
                provider: candidate.provider.clone(),
            });
        }
    }
    sources
}

fn aggregate_search(outcomes: &[SubjectOutcome]) -> Vec<ProviderStat> {
    let mut map: BTreeMap<String, ProviderStat> = BTreeMap::new();
    for outcome in outcomes {
        for attempt in &outcome.search_attempts {
            let stat = map
                .entry(attempt.provider.clone())
                .or_insert_with(|| ProviderStat {
                    provider: attempt.provider.clone(),
                    ..ProviderStat::default()
                });
            stat.attempts += 1;
            stat.queries += 1;
            stat.elapsed_ms += attempt.elapsed_ms;
            if attempt.ok {
                stat.results += attempt.results;
            } else {
                stat.failures += 1;
                stat.last_error = attempt.error.clone();
            }
        }
    }
    map.into_values().collect()
}

fn aggregate_fetch(outcomes: &[SubjectOutcome]) -> FetchStat {
    let mut stat = FetchStat::default();
    for outcome in outcomes {
        stat.requested += outcome.fetch.requested;
        stat.fetched += outcome.fetch.fetched;
        stat.from_cache += outcome.fetch.cached;
        stat.failed += outcome.fetch.failed;
    }
    stat
}

/// Owns the single-run mutex and the pipeline instance.
pub struct RunCoordinator {
    pipeline: Arc<Pipeline>,
    busy: AtomicBool,
}

impl RunCoordinator {
    pub fn new(pipeline: Arc<Pipeline>) -> Arc<RunCoordinator> {
        Arc::new(RunCoordinator {
            pipeline,
            busy: AtomicBool::new(false),
        })
    }

    pub fn pipeline(&self) -> &Arc<Pipeline> {
        &self.pipeline
    }
}

struct BusyGuard(Arc<RunCoordinator>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::SeqCst);
    }
}

impl RefreshController for RunCoordinator {
    fn is_running(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    fn trigger(self: Arc<Self>, request: RefreshRequest) -> Result<TriggerAccepted, TriggerError> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(TriggerError::Busy);
        }
        let run_id = new_run_id();
        let accepted = TriggerAccepted {
            run_id: run_id.clone(),
            season: request.season,
        };
        let this = self.clone();
        let season = request.season;
        let trigger = request.trigger;
        tokio::spawn(async move {
            let _guard = BusyGuard(this.clone());
            let summary = this
                .pipeline
                .clone()
                .run(RunOptions {
                    trigger,
                    season,
                    max_subjects: None,
                    run_id: Some(run_id),
                })
                .await;
            log::info(format!(
                "background run finished: ok={} published={}",
                summary.ok, summary.items_published
            ));
        });
        Ok(accepted)
    }

    fn run_blocking(
        self: Arc<Self>,
        request: RefreshRequest,
    ) -> BoxFuture<'static, Option<RunSummary>> {
        Box::pin(async move {
            if self
                .busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return None;
            }
            let _guard = BusyGuard(self.clone());
            Some(
                self.pipeline
                    .clone()
                    .run(RunOptions {
                        trigger: request.trigger,
                        season: request.season,
                        max_subjects: None,
                        run_id: None,
                    })
                    .await,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_hosts() {
        assert!(is_blocked_host("x.com"));
        assert!(is_blocked_host("twitter.com"));
        assert!(is_blocked_host("mobile.twitter.com"));
        assert!(!is_blocked_host("example.com"));
        assert!(!is_blocked_host("notx.com"));
    }

    #[test]
    fn budget_is_consumed_once() {
        let counter = AtomicUsize::new(2);
        assert!(try_take(&counter));
        assert!(try_take(&counter));
        assert!(!try_take(&counter));
    }

    #[test]
    fn llm_text_respects_limit() {
        let parts = vec!["a".repeat(500), "b".repeat(500)];
        let text = build_llm_text(&parts, 100);
        assert!(text.chars().count() <= 100);
        assert!(text.contains('a'));
    }

    #[test]
    fn publish_policy_can_hold_medium_confidence() {
        let mut verification = Verification {
            resolved: Some(crate::model::ResolvedSlot::new(6, "23:30")),
            confidence: Confidence::Medium,
            status: ReviewStatus::Published,
            reasons: Vec::new(),
            domains: vec!["a.example".to_string()],
        };
        apply_publish_policy(&mut verification, false);
        assert_eq!(verification.status, ReviewStatus::Review);
        assert!(!verification.is_published());
    }
}
