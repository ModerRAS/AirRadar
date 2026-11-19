//! Runtime configuration loaded from environment variables.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use crate::search::ProviderKind;
use crate::season::SeasonRef;
use crate::util::parse_bool;
use crate::DEFAULT_UA;

/// Configuration error with a human readable explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub message: String,
}

impl ConfigError {
    pub fn new(message: impl Into<String>) -> ConfigError {
        ConfigError {
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

fn raw<'a>(map: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    map.get(name)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
}

fn text(map: &HashMap<String, String>, name: &str) -> Option<String> {
    raw(map, name).map(str::to_string)
}

fn parse_or<T>(map: &HashMap<String, String>, name: &str, default: T) -> Result<T, ConfigError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    match raw(map, name) {
        Some(value) => value
            .parse::<T>()
            .map_err(|error| ConfigError::new(format!("{name}: invalid value {value:?}: {error}"))),
        None => Ok(default),
    }
}

fn parse_bool_or(
    map: &HashMap<String, String>,
    name: &str,
    default: bool,
) -> Result<bool, ConfigError> {
    match raw(map, name) {
        Some(value) => parse_bool(value)
            .ok_or_else(|| ConfigError::new(format!("{name}: invalid boolean {value:?}"))),
        None => Ok(default),
    }
}

/// Read NAME or NAME_FILE (docker secret style); whitespace-only files mean unset.
fn secret(map: &HashMap<String, String>, name: &str) -> Result<Option<String>, ConfigError> {
    if let Some(value) = text(map, name) {
        return Ok(Some(value));
    }
    let file_name = format!("{name}_FILE");
    let Some(path) = text(map, &file_name) else {
        return Ok(None);
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Err(error) => Err(ConfigError::new(format!(
            "{file_name}: cannot read {path}: {error}"
        ))),
    }
}

fn clamp<T: PartialOrd>(value: T, low: T, high: T) -> T {
    if value < low {
        low
    } else if value > high {
        high
    } else {
        value
    }
}

/// Fully resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub log_level: String,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub admin_token: Option<String>,
    pub bangumi_ua: String,
    pub fetch_ua: String,
    pub search_order: Vec<ProviderKind>,
    pub search_fallback_on_empty: bool,
    pub search_timeout_secs: u64,
    pub brave_api_key: Option<String>,
    pub tavily_api_key: Option<String>,
    pub deepseek_api_key: Option<String>,
    pub deepseek_search_url: String,
    pub deepseek_search_model: String,
    pub llm_base_url: Option<String>,
    pub llm_api_key: Option<String>,
    pub llm_model: Option<String>,
    /// Optional OpenCode Go session id; when unset a stable id is generated and
    /// persisted under DATA_DIR/llm-session-id.
    pub llm_session_id: Option<String>,
    pub llm_batch_size: usize,
    pub llm_max_input_chars: usize,
    pub max_llm_calls: usize,
    pub search_queries_per_subject: usize,
    pub max_search_calls: usize,
    pub refresh_hours: f64,
    pub max_subjects_per_run: usize,
    pub season_override: Option<SeasonRef>,
    /// Empty vector means all platforms are accepted.
    pub include_platforms: Vec<String>,
    pub enable_official_site: bool,
    pub fetch_timeout_secs: u64,
    pub fetch_concurrency: usize,
    pub fetch_min_interval_ms: u64,
    pub fetch_max_bytes: usize,
    pub fetch_pages_per_subject: usize,
    pub cache_max_bytes: u64,
    pub cache_ttl_hours: f64,
    pub history_keep: usize,
    pub state_retention_seasons: usize,
    pub publish_medium: bool,
    pub max_candidates_per_subject: usize,
}

impl Config {
    /// Load configuration from the process environment.
    pub fn from_env() -> Result<Config, ConfigError> {
        let pairs: Vec<(String, String)> = std::env::vars().collect();
        Config::from_pairs(&pairs)
    }

    /// Load configuration from explicit key/value pairs (testable).
    pub fn from_pairs(pairs: &[(String, String)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();

        let data_dir =
            PathBuf::from(text(&map, "DATA_DIR").unwrap_or_else(|| "./data".to_string()));
        let cache_dir = match text(&map, "CACHE_DIR") {
            Some(value) => PathBuf::from(value),
            None => data_dir.join("cache"),
        };

        let bangumi_ua = text(&map, "BANGUMI_UA").unwrap_or_else(|| DEFAULT_UA.to_string());
        let fetch_ua = text(&map, "FETCH_UA").unwrap_or_else(|| bangumi_ua.clone());

        let search_order = match text(&map, "SEARCH_PROVIDER_ORDER") {
            None => ProviderKind::ALL.to_vec(),
            Some(value) => {
                let mut order = Vec::new();
                for item in value.split(',') {
                    let item = item.trim();
                    if item.is_empty() {
                        continue;
                    }
                    let kind = ProviderKind::parse(item).ok_or_else(|| {
                        ConfigError::new(format!(
                            "SEARCH_PROVIDER_ORDER: unknown provider {item:?} (expected brave, tavily, deepseek)"
                        ))
                    })?;
                    if !order.contains(&kind) {
                        order.push(kind);
                    }
                }
                if order.is_empty() {
                    ProviderKind::ALL.to_vec()
                } else {
                    order
                }
            }
        };

        let include_platforms = match map.get("INCLUDE_PLATFORMS").map(|value| value.trim()) {
            None => vec!["TV".to_string(), "WEB".to_string()],
            Some("") | Some("ALL") | Some("all") => Vec::new(),
            Some(value) => value
                .split(',')
                .map(|item| item.trim().to_ascii_uppercase())
                .filter(|item| !item.is_empty())
                .collect(),
        };

        let refresh_hours: f64 = parse_or(&map, "REFRESH_HOURS", 6.0)?;
        if !refresh_hours.is_finite() || refresh_hours <= 0.0 {
            return Err(ConfigError::new(format!(
                "REFRESH_HOURS must be a positive number, got {refresh_hours}"
            )));
        }

        let season_override = match text(&map, "SEASON_OVERRIDE") {
            None => None,
            Some(value) => Some(SeasonRef::parse(&value).ok_or_else(|| {
                ConfigError::new(format!(
                    "SEASON_OVERRIDE: invalid season {value:?}; expected YYYY-season (e.g. 2026-fall)"
                ))
            })?),
        };

        let cache_ttl_hours: f64 = parse_or(&map, "CACHE_TTL_HOURS", 72.0)?;
        if !cache_ttl_hours.is_finite() || cache_ttl_hours < 0.0 {
            return Err(ConfigError::new(format!(
                "CACHE_TTL_HOURS must be zero or a positive number, got {cache_ttl_hours}"
            )));
        }

        let max_subjects_per_run = clamp(parse_or(&map, "MAX_SUBJECTS_PER_RUN", 30usize)?, 1, 200);
        let fetch_concurrency = clamp(parse_or(&map, "FETCH_CONCURRENCY", 4usize)?, 1, 8);
        let history_keep = clamp(parse_or(&map, "HISTORY_KEEP", 10usize)?, 1, 10);
        let max_llm_calls = clamp(parse_or(&map, "MAX_LLM_CALLS", 10usize)?, 0, 50);
        let max_search_calls = clamp(parse_or(&map, "MAX_SEARCH_CALLS", 60usize)?, 0, 1000);
        let search_queries_per_subject =
            clamp(parse_or(&map, "SEARCH_QUERIES_PER_SUBJECT", 2usize)?, 1, 5);
        let fetch_pages_per_subject =
            clamp(parse_or(&map, "FETCH_PAGES_PER_SUBJECT", 3usize)?, 0, 5);
        let fetch_timeout_secs = clamp(parse_or(&map, "FETCH_TIMEOUT_SECS", 15u64)?, 1, 120);
        let fetch_min_interval_ms =
            clamp(parse_or(&map, "FETCH_MIN_INTERVAL_MS", 300u64)?, 0, 10_000);
        let fetch_max_bytes = clamp(
            parse_or(&map, "FETCH_MAX_BYTES", 204_800usize)?,
            16_384,
            4 * 1024 * 1024,
        );
        let cache_max_bytes = clamp(
            parse_or(&map, "CACHE_MAX_BYTES", 268_435_456u64)?,
            1024 * 1024,
            4 * 1024 * 1024 * 1024,
        );
        let state_retention_seasons =
            clamp(parse_or(&map, "STATE_RETENTION_SEASONS", 4usize)?, 1, 12);
        let llm_batch_size = clamp(parse_or(&map, "LLM_BATCH_SIZE", 5usize)?, 1, 20);
        let llm_max_input_chars =
            clamp(parse_or(&map, "LLM_MAX_INPUT_CHARS", 1200usize)?, 200, 8000);
        let max_candidates_per_subject = clamp(
            parse_or(&map, "MAX_CANDIDATES_PER_SUBJECT", 40usize)?,
            4,
            200,
        );
        let search_timeout_secs = clamp(parse_or(&map, "SEARCH_TIMEOUT_SECS", 20u64)?, 1, 120);

        let llm_base_url = text(&map, "LLM_BASE_URL");
        let llm_api_key = secret(&map, "LLM_API_KEY")?;
        let llm_model = text(&map, "LLM_MODEL");
        let llm_session_id = text(&map, "LLM_SESSION_ID");

        Ok(Config {
            port: parse_or(&map, "PORT", 8080u16)?,
            log_level: text(&map, "LOG_LEVEL").unwrap_or_else(|| "info".to_string()),
            data_dir,
            cache_dir,
            admin_token: secret(&map, "ADMIN_TOKEN")?,
            bangumi_ua,
            fetch_ua,
            search_order,
            search_fallback_on_empty: parse_bool_or(&map, "SEARCH_FALLBACK_ON_EMPTY", false)?,
            search_timeout_secs,
            brave_api_key: secret(&map, "BRAVE_API_KEY")?,
            tavily_api_key: secret(&map, "TAVILY_API_KEY")?,
            deepseek_api_key: secret(&map, "DEEPSEEK_API_KEY")?,
            deepseek_search_url: text(&map, "DEEPSEEK_SEARCH_URL")
                .unwrap_or_else(|| "https://api.deepseek.com/v1/chat/completions".to_string()),
            deepseek_search_model: text(&map, "DEEPSEEK_SEARCH_MODEL")
                .unwrap_or_else(|| "deepseek-chat".to_string()),
            llm_base_url,
            llm_api_key,
            llm_model,
            llm_session_id,
            llm_batch_size,
            llm_max_input_chars,
            max_llm_calls,
            search_queries_per_subject,
            max_search_calls,
            refresh_hours,
            max_subjects_per_run,
            season_override,
            include_platforms,
            enable_official_site: parse_bool_or(&map, "ENABLE_OFFICIAL_SITE", true)?,
            fetch_timeout_secs,
            fetch_concurrency,
            fetch_min_interval_ms,
            fetch_max_bytes,
            fetch_pages_per_subject,
            cache_max_bytes,
            cache_ttl_hours,
            history_keep,
            state_retention_seasons,
            publish_medium: parse_bool_or(&map, "PUBLISH_MEDIUM", true)?,
            max_candidates_per_subject,
        })
    }

    /// Deterministic configuration for unit/integration tests.
    pub fn for_tests(data_dir: PathBuf) -> Config {
        Config {
            port: 0,
            log_level: "error".to_string(),
            cache_dir: data_dir.join("cache"),
            data_dir,
            admin_token: Some("test-token".to_string()),
            bangumi_ua: DEFAULT_UA.to_string(),
            fetch_ua: DEFAULT_UA.to_string(),
            search_order: Vec::new(),
            search_fallback_on_empty: false,
            search_timeout_secs: 5,
            brave_api_key: None,
            tavily_api_key: None,
            deepseek_api_key: None,
            deepseek_search_url: "https://api.deepseek.com/v1/chat/completions".to_string(),
            deepseek_search_model: "deepseek-chat".to_string(),
            llm_base_url: None,
            llm_api_key: None,
            llm_model: None,
            llm_session_id: None,
            llm_batch_size: 5,
            llm_max_input_chars: 1200,
            max_llm_calls: 0,
            search_queries_per_subject: 1,
            max_search_calls: 0,
            refresh_hours: 6.0,
            max_subjects_per_run: 2,
            season_override: None,
            include_platforms: vec!["TV".to_string()],
            enable_official_site: false,
            fetch_timeout_secs: 5,
            fetch_concurrency: 2,
            fetch_min_interval_ms: 0,
            fetch_max_bytes: 204_800,
            fetch_pages_per_subject: 0,
            cache_max_bytes: 1024 * 1024,
            cache_ttl_hours: 72.0,
            history_keep: 3,
            state_retention_seasons: 4,
            publish_medium: true,
            max_candidates_per_subject: 40,
        }
    }

    /// True when all three LLM settings are present.
    pub fn has_llm(&self) -> bool {
        self.llm_base_url.is_some() && self.llm_api_key.is_some() && self.llm_model.is_some()
    }

    pub fn admin_enabled(&self) -> bool {
        self.admin_token
            .as_deref()
            .map(|token| !token.trim().is_empty())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn defaults() {
        let config = Config::from_pairs(&[]).unwrap();
        assert_eq!(config.port, 8080);
        assert_eq!(config.refresh_hours, 6.0);
        assert_eq!(config.max_subjects_per_run, 30);
        assert_eq!(config.history_keep, 10);
        assert_eq!(config.cache_dir, PathBuf::from("./data").join("cache"));
        assert_eq!(config.search_order, ProviderKind::ALL.to_vec());
        assert!(config.brave_api_key.is_none());
        assert!(!config.has_llm());
        assert_eq!(config.include_platforms, vec!["TV", "WEB"]);
        assert!(config.admin_enabled() == false);
    }

    #[test]
    fn overrides_and_clamping() {
        let config = Config::from_pairs(&pairs(&[
            ("PORT", "18080"),
            ("DATA_DIR", "D:/airradar-data"),
            ("REFRESH_HOURS", "1.5"),
            ("MAX_SUBJECTS_PER_RUN", "500"),
            ("HISTORY_KEEP", "99"),
            ("SEARCH_PROVIDER_ORDER", "tavily,brave,tavily"),
            ("INCLUDE_PLATFORMS", "TV, WEB , ON A"),
            ("PUBLISH_MEDIUM", "off"),
            ("SEASON_OVERRIDE", "2026-fall"),
        ]))
        .unwrap();
        assert_eq!(config.port, 18080);
        assert_eq!(config.data_dir, PathBuf::from("D:/airradar-data"));
        assert_eq!(
            config.cache_dir,
            PathBuf::from("D:/airradar-data").join("cache")
        );
        assert_eq!(config.refresh_hours, 1.5);
        assert_eq!(config.max_subjects_per_run, 200);
        assert_eq!(config.history_keep, 10);
        assert_eq!(
            config.search_order,
            vec![ProviderKind::Tavily, ProviderKind::Brave]
        );
        assert_eq!(config.include_platforms, vec!["TV", "WEB", "ON A"]);
        assert!(!config.publish_medium);
        assert_eq!(
            config.season_override,
            Some(SeasonRef::new(2026, crate::season::Season::Fall))
        );
    }

    #[test]
    fn invalid_values_are_rejected() {
        let error = Config::from_pairs(&pairs(&[("PORT", "http")])).unwrap_err();
        assert!(error.message.contains("PORT"));
        let error = Config::from_pairs(&pairs(&[("REFRESH_HOURS", "0")])).unwrap_err();
        assert!(error.message.contains("REFRESH_HOURS"));
        let error = Config::from_pairs(&pairs(&[("SEARCH_PROVIDER_ORDER", "google")])).unwrap_err();
        assert!(error.message.contains("SEARCH_PROVIDER_ORDER"));
        let error = Config::from_pairs(&pairs(&[("SEASON_OVERRIDE", "2026-Q4")])).unwrap_err();
        assert!(error.message.contains("SEASON_OVERRIDE"));
        let error = Config::from_pairs(&pairs(&[("PUBLISH_MEDIUM", "maybe")])).unwrap_err();
        assert!(error.message.contains("PUBLISH_MEDIUM"));
    }

    #[test]
    fn all_platforms_sentinel() {
        let config = Config::from_pairs(&pairs(&[("INCLUDE_PLATFORMS", "ALL")])).unwrap();
        assert!(config.include_platforms.is_empty());
    }

    #[test]
    fn reads_secret_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("token");
        std::fs::write(&token_path, "  file-token\n").unwrap();
        let config = Config::from_pairs(&pairs(&[(
            "ADMIN_TOKEN_FILE",
            token_path.to_str().unwrap(),
        )]))
        .unwrap();
        assert_eq!(config.admin_token.as_deref(), Some("file-token"));
        assert!(config.admin_enabled());

        let empty_path = dir.path().join("empty");
        std::fs::write(&empty_path, "   \n").unwrap();
        let config = Config::from_pairs(&pairs(&[(
            "ADMIN_TOKEN_FILE",
            empty_path.to_str().unwrap(),
        )]))
        .unwrap();
        assert!(config.admin_token.is_none());
        assert!(!config.admin_enabled());
    }

    #[test]
    fn llm_session_id_is_optional() {
        let config = Config::from_pairs(&[]).unwrap();
        assert!(config.llm_session_id.is_none());

        let config = Config::from_pairs(&pairs(&[("LLM_SESSION_ID", "  session-abc  ")])).unwrap();
        assert_eq!(config.llm_session_id.as_deref(), Some("session-abc"));

        // Blank means unset -> a stable id will be generated and persisted.
        let config = Config::from_pairs(&pairs(&[("LLM_SESSION_ID", "   ")])).unwrap();
        assert!(config.llm_session_id.is_none());
    }

    #[test]
    fn llm_requires_all_three_settings() {
        let partial = Config::from_pairs(&pairs(&[
            ("LLM_BASE_URL", "http://127.0.0.1:9/v1"),
            ("LLM_MODEL", "m"),
        ]))
        .unwrap();
        assert!(!partial.has_llm());
        let complete = Config::from_pairs(&pairs(&[
            ("LLM_BASE_URL", "http://127.0.0.1:9/v1"),
            ("LLM_MODEL", "m"),
            ("LLM_API_KEY", "k"),
        ]))
        .unwrap();
        assert!(complete.has_llm());
        assert_eq!(complete.llm_api_key.as_deref(), Some("k"));
    }
}
