//! Page cache under /data/cache with a byte budget and LRU eviction.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::atomic::atomic_write;
use crate::util::hash_hex;

/// One cached page (extracted text, not raw HTML, to keep the disk budget).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedPage {
    pub url: String,
    pub key: String,
    pub fetched_at: DateTime<Utc>,
    pub status: u16,
    pub content_type: String,
    /// Extracted plain text (post HTML-to-text).
    pub text: String,
    /// Downloaded byte count before extraction/truncation.
    pub raw_bytes: usize,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
}

/// Cache counters reported by /api/status.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CacheStats {
    pub entries: usize,
    pub bytes: u64,
    pub max_bytes: u64,
}

/// Summary of one LRU cleanup pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub removed: usize,
    pub remaining_entries: usize,
    pub remaining_bytes: u64,
}

/// Disk-backed page cache. Access times for LRU live in memory; on restart the
/// file modification time is used as a fallback, which is good enough for a
/// bounded cache.
pub struct Cache {
    dir: PathBuf,
    max_bytes: u64,
    ttl: Option<Duration>,
    access: Mutex<HashMap<String, SystemTime>>,
}

impl Cache {
    pub fn new(dir: PathBuf, max_bytes: u64, ttl: Option<Duration>) -> Cache {
        Cache {
            dir,
            max_bytes,
            ttl,
            access: Mutex::new(HashMap::new()),
        }
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    fn key_for(url: &str) -> String {
        hash_hex(url)
    }

    fn path_for_key(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    /// Look up a page. Expired entries are treated as misses but not deleted.
    pub fn get(&self, url: &str) -> Option<CachedPage> {
        let key = Self::key_for(url);
        let path = self.path_for_key(&key);
        let content = fs::read_to_string(&path).ok()?;
        let page: CachedPage = serde_json::from_str(&content).ok()?;
        if page.url != url {
            return None;
        }
        if let Some(ttl) = self.ttl {
            let age = Utc::now().signed_duration_since(page.fetched_at);
            match age.to_std() {
                Ok(age) if age <= ttl => {}
                _ => return None,
            }
        }
        self.access
            .lock()
            .expect("cache access mutex poisoned")
            .insert(key, SystemTime::now());
        Some(page)
    }

    /// Store a page and run LRU cleanup.
    pub fn put(&self, page: CachedPage) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let key = Self::key_for(&page.url);
        let path = self.path_for_key(&key);
        let bytes = serde_json::to_vec(&page)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        atomic_write(&path, &bytes)?;
        self.access
            .lock()
            .expect("cache access mutex poisoned")
            .insert(key, SystemTime::now());
        self.cleanup();
        Ok(())
    }

    /// Delete least recently used entries until the byte budget is respected.
    pub fn cleanup(&self) -> CleanupReport {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(_) => return CleanupReport::default(),
        };
        let mut files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
        let mut total = 0u64;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let key = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("")
                .to_string();
            let accessed = self
                .access
                .lock()
                .expect("cache access mutex poisoned")
                .get(&key)
                .copied()
                .or_else(|| metadata.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            total += metadata.len();
            files.push((path, metadata.len(), accessed));
        }
        files.sort_by(|left, right| left.2.cmp(&right.2));
        let mut removed = 0usize;
        for (path, length, _) in &files {
            if total <= self.max_bytes {
                break;
            }
            if fs::remove_file(path).is_ok() {
                total = total.saturating_sub(*length);
                removed += 1;
                if let Some(key) = path.file_stem().and_then(|stem| stem.to_str()) {
                    self.access
                        .lock()
                        .expect("cache access mutex poisoned")
                        .remove(key);
                }
            }
        }
        CleanupReport {
            removed,
            remaining_entries: files.len().saturating_sub(removed),
            remaining_bytes: total,
        }
    }

    /// Current cache size on disk.
    pub fn stats(&self) -> CacheStats {
        let mut stats = CacheStats {
            max_bytes: self.max_bytes,
            ..CacheStats::default()
        };
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                if let Ok(metadata) = entry.metadata() {
                    if metadata.is_file() {
                        stats.entries += 1;
                        stats.bytes += metadata.len();
                    }
                }
            }
        }
        stats
    }
}

#[cfg(test)]
fn sample_page(url: &str, text: &str) -> CachedPage {
    CachedPage {
        url: url.to_string(),
        key: hash_hex(url),
        fetched_at: Utc::now(),
        status: 200,
        content_type: "text/html".to_string(),
        text: text.to_string(),
        raw_bytes: text.len(),
        truncated: false,
        final_url: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::truncate_chars;

    #[test]
    fn put_and_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().join("cache"), 1_000_000, None);
        cache
            .put(sample_page("https://example.com/a", "毎週土曜 23:30"))
            .unwrap();
        let page = cache.get("https://example.com/a").unwrap();
        assert_eq!(page.text, "毎週土曜 23:30");
        assert!(cache.get("https://example.com/other").is_none());
    }

    #[test]
    fn expired_entries_are_misses() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(
            dir.path().join("cache"),
            1_000_000,
            Some(Duration::from_millis(30)),
        );
        cache
            .put(sample_page("https://example.com/a", "x"))
            .unwrap();
        std::thread::sleep(Duration::from_millis(60));
        assert!(cache.get("https://example.com/a").is_none());
    }

    #[test]
    fn lru_evicts_oldest_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().join("cache"), 1_200, None);
        cache
            .put(sample_page("https://example.com/one", &"a".repeat(300)))
            .unwrap();
        std::thread::sleep(Duration::from_millis(10));
        cache
            .put(sample_page("https://example.com/two", &"b".repeat(300)))
            .unwrap();
        std::thread::sleep(Duration::from_millis(10));
        cache
            .put(sample_page("https://example.com/three", &"c".repeat(300)))
            .unwrap();
        let stats = cache.stats();
        assert!(stats.bytes <= 1_200, "cache exceeded budget: {stats:?}");
        assert!(cache.get("https://example.com/one").is_none());
        assert!(cache.get("https://example.com/three").is_some());
    }

    #[test]
    fn cleanup_reports_counts() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().join("cache"), 0, None);
        std::fs::create_dir_all(cache.dir()).unwrap();
        std::fs::write(cache.dir().join("deadbeef.json"), b"{}").unwrap();
        let report = cache.cleanup();
        assert_eq!(report.removed, 1);
        assert_eq!(report.remaining_entries, 0);
        assert_eq!(report.remaining_bytes, 0);
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn collision_guard_rejects_mismatched_url() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().join("cache"), 1_000_000, None);
        cache
            .put(sample_page("https://example.com/a", "x"))
            .unwrap();
        let key = hash_hex("https://example.com/a");
        let path = dir.path().join("cache").join(format!("{key}.json"));
        let mut page: CachedPage =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        page.url = "https://example.com/other".to_string();
        atomic_write(&path, &serde_json::to_vec(&page).unwrap()).unwrap();
        assert!(cache.get("https://example.com/a").is_none());
    }

    #[test]
    fn truncation_helper_is_char_safe() {
        assert_eq!(truncate_chars("日本語テキスト", 3), "日本語");
    }
}
