//! Persistent storage under DATA_DIR. Every JSON write is atomic.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::atomic::atomic_write;
use crate::model::{ReviewFile, ScheduleFile, StateFile, StatusFile, SCHEMA_VERSION};
use crate::season::SeasonRef;
use crate::util::{truncate_chars, weekday_en, weekday_zh};

/// Storage failure.
#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(error) => write!(f, "io error: {error}"),
            StoreError::Json(error) => write!(f, "json error: {error}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Io(error) => Some(error),
            StoreError::Json(error) => Some(error),
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> StoreError {
        StoreError::Io(error)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> StoreError {
        StoreError::Json(error)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// File layout under DATA_DIR:
/// - state.json, status.json, review.json, review.md, llm-cache.json
/// - llm-session-id (stable OpenCode Go session id, plain text)
/// - schedule/<season>.json, schedule/latest.json
/// - history/<season>-<timestamp>.json
/// - cache/ (owned by crate::cache::Cache)
pub struct Store {
    data_dir: PathBuf,
}

impl Store {
    pub fn new(data_dir: PathBuf) -> Store {
        Store { data_dir }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn state_path(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }

    pub fn status_path(&self) -> PathBuf {
        self.data_dir.join("status.json")
    }

    pub fn review_json_path(&self) -> PathBuf {
        self.data_dir.join("review.json")
    }

    pub fn review_md_path(&self) -> PathBuf {
        self.data_dir.join("review.md")
    }

    pub fn llm_cache_path(&self) -> PathBuf {
        self.data_dir.join("llm-cache.json")
    }

    /// Stable OpenCode Go session id used for the x-opencode-session header.
    pub fn llm_session_id_path(&self) -> PathBuf {
        self.data_dir.join("llm-session-id")
    }

    pub fn schedule_dir(&self) -> PathBuf {
        self.data_dir.join("schedule")
    }

    pub fn latest_path(&self) -> PathBuf {
        self.schedule_dir().join("latest.json")
    }

    pub fn schedule_path(&self, season: &SeasonRef) -> PathBuf {
        self.schedule_dir().join(format!("{season}.json"))
    }

    pub fn history_dir(&self) -> PathBuf {
        self.data_dir.join("history")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.data_dir)?;
        fs::create_dir_all(self.schedule_dir())?;
        fs::create_dir_all(self.history_dir())?;
        Ok(())
    }

    fn read_json<T: DeserializeOwned>(&self, path: &Path) -> Result<Option<T>> {
        if !path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(path)?;
        if content.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&content)?))
    }

    fn write_json<T: Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value)?;
        atomic_write(path, &bytes)?;
        Ok(())
    }

    pub fn load_state(&self) -> Result<Option<StateFile>> {
        self.read_json(&self.state_path())
    }

    pub fn save_state(&self, state: &StateFile) -> Result<()> {
        self.write_json(&self.state_path(), state)
    }

    pub fn load_latest_schedule(&self) -> Result<Option<ScheduleFile>> {
        self.read_json(&self.latest_path())
    }

    pub fn load_schedule(&self, season: &SeasonRef) -> Result<Option<ScheduleFile>> {
        self.read_json(&self.schedule_path(season))
    }

    /// Publish a schedule: season file + latest.json + one history version.
    pub fn publish(&self, schedule: &ScheduleFile, history_keep: usize) -> Result<()> {
        self.ensure_dirs()?;
        let bytes = serde_json::to_vec_pretty(schedule)?;
        let season_path = self
            .schedule_dir()
            .join(format!("{}.json", schedule.season));
        atomic_write(&season_path, &bytes)?;
        atomic_write(&self.latest_path(), &bytes)?;
        let history_name = format!(
            "{}-{}.json",
            schedule.season,
            schedule.generated_at.format("%Y%m%dT%H%M%SZ")
        );
        atomic_write(&self.history_dir().join(history_name), &bytes)?;
        self.prune_history(history_keep)?;
        Ok(())
    }

    /// History files, newest first.
    pub fn history_versions(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = match fs::read_dir(self.history_dir()) {
            Ok(entries) => entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
                .collect(),
            Err(_) => Vec::new(),
        };
        files.sort_by(|left, right| right.file_name().cmp(&left.file_name()));
        files
    }

    /// Keep at most keep history files; returns how many were removed.
    pub fn prune_history(&self, keep: usize) -> Result<usize> {
        let versions = self.history_versions();
        let mut removed = 0usize;
        for path in versions.iter().skip(keep) {
            match fs::remove_file(path) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(StoreError::Io(error)),
            }
        }
        Ok(removed)
    }

    /// Persist review.json (machine readable) and review.md (human readable).
    pub fn save_reviews(&self, review: &ReviewFile) -> Result<()> {
        self.ensure_dirs()?;
        self.write_json(&self.review_json_path(), review)?;
        let markdown = render_review_markdown(review);
        atomic_write(&self.review_md_path(), markdown.as_bytes())?;
        Ok(())
    }

    pub fn load_status(&self) -> Result<Option<StatusFile>> {
        self.read_json(&self.status_path())
    }

    pub fn save_status(&self, status: &StatusFile) -> Result<()> {
        self.write_json(&self.status_path(), status)
    }

    /// LLM result cache is stored as a flat JSON map keyed by subject + text hash.
    pub fn load_llm_cache(&self) -> Result<HashMap<String, Value>> {
        Ok(self.read_json(&self.llm_cache_path())?.unwrap_or_default())
    }

    pub fn save_llm_cache(&self, cache: &HashMap<String, Value>) -> Result<()> {
        self.write_json(&self.llm_cache_path(), cache)
    }

    /// Load the persisted OpenCode Go session id, if any.
    pub fn load_llm_session_id(&self) -> Result<Option<String>> {
        let path = self.llm_session_id_path();
        if !path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&path)?;
        let trimmed = content.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_string()))
        }
    }

    /// Persist the OpenCode Go session id atomically. Empty ids are ignored so a
    /// blank value can never wipe the stable id.
    pub fn save_llm_session_id(&self, session_id: &str) -> Result<()> {
        let trimmed = session_id.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        atomic_write(&self.llm_session_id_path(), trimmed.as_bytes())?;
        Ok(())
    }
}

fn candidate_time_label(candidate: &crate::model::TimeCandidate) -> String {
    match (candidate.weekday, candidate.time.as_deref()) {
        (Some(weekday), Some(time)) => {
            format!("{} {} ({})", weekday_en(weekday), time, weekday_zh(weekday))
        }
        (Some(weekday), None) => format!("{} (time unknown)", weekday_zh(weekday)),
        _ => "time unknown".to_string(),
    }
}

/// Render the human review file.
pub fn render_review_markdown(review: &ReviewFile) -> String {
    let mut out = String::new();
    out.push_str("# AirRadar review queue\n\n");
    out.push_str(&format!("- Season: {}\n", review.season));
    out.push_str(&format!(
        "- Generated: {}\n",
        review
            .generated_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    ));
    out.push_str(&format!(
        "- Entries: {} (conflicts: {}, missing: {})\n",
        review.total, review.conflicts, review.missing
    ));
    out.push_str("\nEntries are excluded from the published schedule until reviewed.\n");
    for entry in &review.items {
        out.push_str(&format!("\n## {} ({})\n", entry.name, entry.subject_id));
        if let Some(name_cn) = &entry.name_cn {
            if name_cn != &entry.name {
                out.push_str(&format!("- Chinese title: {name_cn}\n"));
            }
        }
        out.push_str(&format!(
            "- Status: {:?}, confidence: {}\n",
            entry.status,
            entry.confidence.as_str()
        ));
        out.push_str(&format!(
            "- Last checked: {}\n",
            entry
                .last_checked_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ));
        for reason in &entry.reasons {
            out.push_str(&format!("- Reason: {reason}\n"));
        }
        if !entry.candidates.is_empty() {
            out.push_str("- Candidates:\n");
            for candidate in &entry.candidates {
                let evidence = truncate_chars(&candidate.evidence, 200);
                out.push_str(&format!(
                    "  - {} | {} ({}) | {}\n",
                    candidate_time_label(candidate),
                    candidate.provider,
                    candidate.source_domain,
                    evidence.replace('\n', " ")
                ));
            }
        }
    }
    out
}

/// Convenience constructor for an empty review file.
pub fn empty_review(season: String) -> ReviewFile {
    ReviewFile {
        schema_version: SCHEMA_VERSION,
        generated_at: Utc::now(),
        season,
        total: 0,
        conflicts: 0,
        missing: 0,
        items: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Confidence, ReviewEntry, ReviewStatus, ScheduleItem, ScheduleSource, SCHEMA_VERSION,
    };
    use crate::season::{Season, SeasonRef};
    use chrono::{Duration, Utc};

    fn sample_schedule(generated_at: chrono::DateTime<Utc>) -> ScheduleFile {
        let item = ScheduleItem {
            subject_id: 393010,
            name: "探偵はもう、死んでいる。 Season 2".to_string(),
            name_cn: Some("侦探已经死了。 第二季".to_string()),
            air_date: None,
            image: None,
            weekday: 3,
            time: "23:00".to_string(),
            timezone: "Asia/Tokyo".to_string(),
            confidence: Confidence::High,
            sources: vec![ScheduleSource {
                url: "https://example.com/schedule".to_string(),
                domain: "example.com".to_string(),
                provider: "brave".to_string(),
            }],
            updated_at: Utc::now(),
        };
        ScheduleFile {
            schema_version: SCHEMA_VERSION,
            generated_at,
            season: "2026-fall".to_string(),
            season_start: chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            season_end: chrono::NaiveDate::from_ymd_opt(2026, 12, 31).unwrap(),
            timezone: "Asia/Tokyo".to_string(),
            item_count: 1,
            items: vec![item],
        }
    }

    #[test]
    fn state_roundtrip_and_corrupt_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        assert!(store.load_state().unwrap().is_none());
        let mut state = StateFile::empty();
        state.updated_at = Utc::now();
        store.save_state(&state).unwrap();
        let loaded = store.load_state().unwrap().unwrap();
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert!(loaded.subjects.is_empty());

        fs::write(store.state_path(), "{not json").unwrap();
        assert!(store.load_state().is_err());
    }

    #[test]
    fn publish_writes_latest_and_history_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let season = SeasonRef::new(2026, Season::Fall);
        let base = Utc::now();
        for index in 0..4 {
            let schedule = sample_schedule(base + Duration::seconds(index));
            store.publish(&schedule, 2).unwrap();
        }
        let latest = store.load_latest_schedule().unwrap().unwrap();
        assert_eq!(latest.season, "2026-fall");
        assert_eq!(latest.items.len(), 1);
        let per_season = store.load_schedule(&season).unwrap().unwrap();
        assert_eq!(per_season.items[0].subject_id, 393010);
        assert_eq!(store.history_versions().len(), 2);
        let names: Vec<String> = store
            .history_versions()
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(
            names[0] > names[1],
            "history must be newest first: {names:?}"
        );
    }

    #[test]
    fn no_tmp_files_after_publish() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        store.publish(&sample_schedule(Utc::now()), 5).unwrap();
        let leftovers: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }

    #[test]
    fn review_files_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let review = ReviewFile {
            schema_version: SCHEMA_VERSION,
            generated_at: Utc::now(),
            season: "2026-fall".to_string(),
            total: 1,
            conflicts: 1,
            missing: 0,
            items: vec![ReviewEntry {
                subject_id: 1,
                name: "作品A".to_string(),
                name_cn: Some("作品甲".to_string()),
                status: ReviewStatus::Review,
                confidence: Confidence::Low,
                reasons: vec![
                    "conflict: Sat 23:30 <- a.example; Sun 01:15 <- b.example".to_string()
                ],
                candidates: vec![crate::model::TimeCandidate {
                    weekday: Some(6),
                    time: Some("23:30".to_string()),
                    source_url: None,
                    source_domain: "a.example".to_string(),
                    provider: "brave".to_string(),
                    method: "jp:weekly-colon".to_string(),
                    evidence: "毎週土曜 23:30".to_string(),
                    observed_at: Utc::now(),
                }],
                last_checked_at: Utc::now(),
            }],
        };
        store.save_reviews(&review).unwrap();
        assert!(store.review_json_path().exists());
        let markdown = fs::read_to_string(store.review_md_path()).unwrap();
        assert!(markdown.contains("作品A"));
        assert!(markdown.contains("作品甲"));
        assert!(markdown.contains("conflict"));
        assert!(markdown.contains("Sat 23:30"));
    }

    #[test]
    fn status_and_llm_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        assert!(store.load_status().unwrap().is_none());
        let status = StatusFile::empty();
        store.save_status(&status).unwrap();
        assert!(store.load_status().unwrap().is_some());

        let mut cache = HashMap::new();
        cache.insert(
            "subject:1".to_string(),
            serde_json::json!({"weekday": 6, "time": "23:30"}),
        );
        store.save_llm_cache(&cache).unwrap();
        let loaded = store.load_llm_cache().unwrap();
        assert_eq!(loaded.get("subject:1").unwrap()["weekday"], 6);
    }

    #[test]
    fn llm_session_id_is_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        assert!(store.load_llm_session_id().unwrap().is_none());

        store.save_llm_session_id("  airradar-abc123  ").unwrap();
        assert_eq!(
            store.load_llm_session_id().unwrap().as_deref(),
            Some("airradar-abc123")
        );
        assert!(store.llm_session_id_path().exists());

        // Survives a restart (a new Store instance on the same data dir).
        let reopened = Store::new(dir.path().to_path_buf());
        assert_eq!(
            reopened.load_llm_session_id().unwrap().as_deref(),
            Some("airradar-abc123")
        );

        // An empty value must not wipe the stable id.
        store.save_llm_session_id("   ").unwrap();
        assert_eq!(
            store.load_llm_session_id().unwrap().as_deref(),
            Some("airradar-abc123")
        );
    }

    #[test]
    fn empty_review_is_rendered() {
        let markdown = render_review_markdown(&empty_review("2026-fall".to_string()));
        assert!(markdown.contains("Season: 2026-fall"));
        assert!(markdown.contains("Entries: 0"));
    }
}
