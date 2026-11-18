//! Official Bangumi API client (https://api.bgm.tv).
//!
//! The current-season list is fetched with POST /v0/search/subjects filtered by
//! air_date (type=2). GET /calendar is used as a fallback when search fails.
//! Only id/name/name_cn/air_date/images/platform/infobox are consumed.

use std::fmt;
use std::time::Duration;

use chrono::NaiveDate;
use reqwest::header::{ACCEPT, USER_AGENT};
use serde_json::{json, Value};

use crate::config::Config;
use crate::log;
use crate::season::SeasonRef;
use crate::util::truncate_chars;

const DEFAULT_BASE: &str = "https://api.bgm.tv";
const PAGE_LIMIT: usize = 100;
const MAX_PAGES: usize = 4;
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// Bangumi API failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BangumiError {
    Http(String),
    Status(u16),
    Parse(String),
}

impl fmt::Display for BangumiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BangumiError::Http(message) => write!(f, "http error: {message}"),
            BangumiError::Status(status) => write!(f, "http status {status}"),
            BangumiError::Parse(message) => write!(f, "parse error: {message}"),
        }
    }
}

impl std::error::Error for BangumiError {}

/// Minimal Bangumi subject data required by the pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BangumiSubject {
    pub id: u64,
    pub name: String,
    pub name_cn: Option<String>,
    pub air_date: Option<NaiveDate>,
    pub platform: Option<String>,
    pub image: Option<String>,
    pub official_site: Option<String>,
    /// Weekday hint from the Bangumi infobox or calendar (1=Mon..7=Sun).
    pub weekday_hint: Option<u8>,
}

/// Where the season list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectListSource {
    Search,
    Calendar,
}

impl SubjectListSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            SubjectListSource::Search => "bangumi-search",
            SubjectListSource::Calendar => "bangumi-calendar",
        }
    }
}

/// Bangumi API client. Cheap to clone for concurrent use? Keep one per run.
pub struct BangumiClient {
    client: reqwest::Client,
    base_url: String,
    user_agent: String,
}

impl BangumiClient {
    pub fn new(cfg: &Config) -> Result<BangumiClient, BangumiError> {
        BangumiClient::with_base(cfg, DEFAULT_BASE)
    }

    pub fn with_base(cfg: &Config, base_url: &str) -> Result<BangumiClient, BangumiError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .map_err(|error| BangumiError::Http(format!("build http client: {error}")))?;
        Ok(BangumiClient {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            user_agent: cfg.bangumi_ua.clone(),
        })
    }

    /// Fetch the season subject list, falling back to /calendar on failure.
    pub async fn fetch_season_list(
        &self,
        season: &SeasonRef,
        max: usize,
    ) -> Result<(Vec<BangumiSubject>, SubjectListSource), BangumiError> {
        match self.search_season(season, max).await {
            Ok(subjects) if !subjects.is_empty() => {
                return Ok((subjects, SubjectListSource::Search));
            }
            Ok(_) => {
                log::warn("bangumi search returned no subjects; falling back to /calendar");
            }
            Err(error) => {
                log::warn(format!(
                    "bangumi search failed ({error}); falling back to /calendar"
                ));
            }
        }
        let mut subjects = self.calendar().await?;
        subjects = filter_by_season(subjects, season);
        Ok((subjects, SubjectListSource::Calendar))
    }

    /// POST /v0/search/subjects with an empty keyword and an air_date filter.
    pub async fn search_season(
        &self,
        season: &SeasonRef,
        max: usize,
    ) -> Result<Vec<BangumiSubject>, BangumiError> {
        let start = season.start();
        let end = season.end_exclusive();
        let page_limit = max.clamp(1, PAGE_LIMIT);
        let mut collected: Vec<BangumiSubject> = Vec::new();
        let mut offset = 0usize;
        for _page in 0..MAX_PAGES {
            let body = json!({
                "keyword": "",
                "sort": "match",
                "filter": {
                    "type": [2],
                    "air_date": [format!(">={start}"), format!("<{end}")]
                }
            });
            let url = format!(
                "{}/v0/search/subjects?limit={page_limit}&offset={offset}",
                self.base_url
            );
            let value = self.send_json("POST", &url, Some(body)).await?;
            let data = value
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if data.is_empty() {
                break;
            }
            for item in &data {
                if let Some(subject) = parse_subject(item) {
                    collected.push(subject);
                }
            }
            offset += data.len();
            let total = value.get("total").and_then(Value::as_u64).unwrap_or(0);
            if collected.len() >= max || offset as u64 >= total {
                break;
            }
        }
        let mut collected = dedup_subjects(collected);
        collected.truncate(max);
        Ok(collected)
    }

    /// GET /calendar (currently airing list, used as a fallback).
    pub async fn calendar(&self) -> Result<Vec<BangumiSubject>, BangumiError> {
        let url = format!("{}/calendar", self.base_url);
        let value = self.send_json("GET", &url, None).await?;
        Ok(dedup_subjects(parse_calendar_response(&value)))
    }

    async fn send_json(
        &self,
        method: &str,
        url: &str,
        body: Option<Value>,
    ) -> Result<Value, BangumiError> {
        let mut last_error: Option<BangumiError> = None;
        for attempt in 0..2 {
            let mut request = if method == "POST" {
                self.client.post(url)
            } else {
                self.client.get(url)
            };
            request = request
                .header(ACCEPT, "application/json")
                .header(USER_AGENT, &self.user_agent);
            if let Some(body) = &body {
                request = request.json(body);
            }
            match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    let text = response.text().await.map_err(|error| {
                        BangumiError::Http(format!("{url}: read body: {error}"))
                    })?;
                    if status.is_success() {
                        return serde_json::from_str(&text).map_err(|error| {
                            BangumiError::Parse(format!(
                                "{url}: {error}: {}",
                                truncate_chars(&text, 200)
                            ))
                        });
                    }
                    last_error = Some(BangumiError::Status(status.as_u16()));
                    if attempt == 0 && (status.is_server_error() || status.as_u16() == 429) {
                        tokio::time::sleep(RETRY_DELAY).await;
                        continue;
                    }
                    return Err(last_error.expect("status error set"));
                }
                Err(error) => {
                    last_error = Some(BangumiError::Http(format!("{url}: {error}")));
                    if attempt == 0 {
                        tokio::time::sleep(RETRY_DELAY).await;
                        continue;
                    }
                    return Err(last_error.expect("http error set"));
                }
            }
        }
        Err(last_error.unwrap_or_else(|| BangumiError::Http("request failed".to_string())))
    }
}

fn parse_date(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()
}

fn pick_image(images: &Value) -> Option<String> {
    ["large", "common", "medium", "small"]
        .iter()
        .find_map(|key| {
            images
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        })
}

fn value_to_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => items.iter().find_map(value_to_text),
        Value::Object(map) => map
            .get("v")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                map.values()
                    .find_map(|item| item.as_str())
                    .map(str::to_string)
            }),
        _ => None,
    }
}

fn infobox_value(items: &[Value], key: &str) -> Option<String> {
    let item = items
        .iter()
        .find(|item| item.get("key").and_then(Value::as_str) == Some(key))?;
    value_to_text(item.get("value")?)
}

fn normalize_site_url(text: &str) -> Option<String> {
    let first = text.split_whitespace().next().unwrap_or("").trim();
    if first.starts_with("http://") || first.starts_with("https://") {
        Some(first.to_string())
    } else {
        None
    }
}

fn parse_weekday_hint(text: &str) -> Option<u8> {
    for ch in text.chars() {
        let weekday = match ch {
            '一' => 1,
            '二' => 2,
            '三' => 3,
            '四' => 4,
            '五' => 5,
            '六' => 6,
            '日' | '天' => 7,
            '月' => 1,
            '火' => 2,
            '水' => 3,
            '木' => 4,
            '金' => 5,
            '土' => 6,
            _ => continue,
        };
        return Some(weekday);
    }
    None
}

/// Parse one subject object from a search or calendar payload.
pub fn parse_subject(value: &Value) -> Option<BangumiSubject> {
    let id = value.get("id").and_then(Value::as_u64)?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        return None;
    }
    let name_cn = value
        .get("name_cn")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let date_text = value
        .get("date")
        .and_then(Value::as_str)
        .or_else(|| value.get("air_date").and_then(Value::as_str));
    let air_date = date_text.and_then(parse_date);
    let platform = value
        .get("platform")
        .and_then(Value::as_str)
        .map(str::to_string);
    let image = value.get("images").and_then(pick_image);
    let infobox = value.get("infobox").and_then(Value::as_array);
    let official_site = infobox
        .and_then(|items| infobox_value(items, "官方网站"))
        .and_then(|text| normalize_site_url(&text));
    let weekday_hint = infobox
        .and_then(|items| infobox_value(items, "放送星期"))
        .and_then(|text| parse_weekday_hint(&text))
        .or_else(|| {
            value
                .get("air_weekday")
                .and_then(Value::as_u64)
                .and_then(|number| u8::try_from(number).ok())
                .filter(|number| (1..=7).contains(number))
        });
    Some(BangumiSubject {
        id,
        name,
        name_cn,
        air_date,
        platform,
        image,
        official_site,
        weekday_hint,
    })
}

/// Parse the data array of POST /v0/search/subjects.
pub fn parse_search_response(value: &Value) -> Vec<BangumiSubject> {
    value
        .get("data")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_subject).collect())
        .unwrap_or_default()
}

/// Parse GET /calendar (array of weekday objects with items).
pub fn parse_calendar_response(value: &Value) -> Vec<BangumiSubject> {
    let mut subjects = Vec::new();
    let Some(days) = value.as_array() else {
        return subjects;
    };
    for day in days {
        if let Some(items) = day.get("items").and_then(Value::as_array) {
            for item in items {
                if let Some(subject) = parse_subject(item) {
                    subjects.push(subject);
                }
            }
        }
    }
    subjects
}

/// Keep only subjects whose air_date falls inside the season.
pub fn filter_by_season(subjects: Vec<BangumiSubject>, season: &SeasonRef) -> Vec<BangumiSubject> {
    let start = season.start();
    let end = season.end_exclusive();
    subjects
        .into_iter()
        .filter(|subject| {
            subject
                .air_date
                .map(|date| date >= start && date < end)
                .unwrap_or(false)
        })
        .collect()
}

fn dedup_subjects(subjects: Vec<BangumiSubject>) -> Vec<BangumiSubject> {
    let mut seen = std::collections::BTreeSet::new();
    let mut unique: Vec<BangumiSubject> = Vec::new();
    for subject in subjects {
        if seen.insert(subject.id) {
            unique.push(subject);
        }
    }
    unique.sort_by(|left, right| {
        left.air_date
            .cmp(&right.air_date)
            .then_with(|| left.id.cmp(&right.id))
    });
    unique
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::season::{Season, SeasonRef};

    fn search_fixture() -> Value {
        serde_json::from_str(include_str!("../fixtures/bangumi_search.json")).unwrap()
    }

    fn calendar_fixture() -> Value {
        serde_json::from_str(include_str!("../fixtures/bangumi_calendar.json")).unwrap()
    }

    #[test]
    fn parses_search_payload() {
        let subjects = parse_search_response(&search_fixture());
        assert_eq!(subjects.len(), 3);
        let first = &subjects[0];
        assert_eq!(first.id, 393010);
        assert_eq!(first.name, "探偵はもう、死んでいる。 Season 2");
        assert_eq!(first.name_cn.as_deref(), Some("侦探已经死了。 第二季"));
        assert_eq!(first.air_date, NaiveDate::from_ymd_opt(2026, 10, 7));
        assert_eq!(first.platform.as_deref(), Some("TV"));
        assert!(first.image.as_deref().unwrap().contains("lain.bgm.tv"));
        assert_eq!(
            first.official_site.as_deref(),
            Some("https://tanmoshi-anime.jp/Season2/")
        );
        assert_eq!(first.weekday_hint, Some(3));
        let movie = &subjects[2];
        assert_eq!(movie.platform.as_deref(), Some("剧场版"));
        assert!(movie.official_site.is_none());
        assert_eq!(
            movie.image.as_deref(),
            Some("https://lain.bgm.tv/pic/cover/c/zz.jpg")
        );
    }

    #[test]
    fn parses_infobox_array_url() {
        let subjects = parse_search_response(&search_fixture());
        let web = &subjects[1];
        assert_eq!(
            web.official_site.as_deref(),
            Some("https://example.com/lienv")
        );
        assert!(web.weekday_hint.is_none());
    }

    #[test]
    fn parses_calendar_payload() {
        let subjects = parse_calendar_response(&calendar_fixture());
        assert_eq!(subjects.len(), 2);
        assert_eq!(subjects[0].id, 393010);
        assert_eq!(subjects[0].air_date, NaiveDate::from_ymd_opt(2026, 10, 7));
        assert_eq!(subjects[0].weekday_hint, Some(3));
    }

    #[test]
    fn filters_calendar_to_season() {
        let subjects = parse_calendar_response(&calendar_fixture());
        let filtered = filter_by_season(subjects, &SeasonRef::new(2026, Season::Fall));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, 393010);
    }

    #[test]
    fn parses_weekday_hints() {
        assert_eq!(parse_weekday_hint("星期三"), Some(3));
        assert_eq!(parse_weekday_hint("周六"), Some(6));
        assert_eq!(parse_weekday_hint("水曜日"), Some(3));
        assert_eq!(parse_weekday_hint("土曜日"), Some(6));
        assert_eq!(parse_weekday_hint("星期日"), Some(7));
        assert_eq!(parse_weekday_hint("unknown"), None);
    }

    #[test]
    fn normalizes_official_site() {
        assert_eq!(
            normalize_site_url("https://a.example/x"),
            Some("https://a.example/x".to_string())
        );
        assert_eq!(
            normalize_site_url("http://a.example"),
            Some("http://a.example".to_string())
        );
        assert_eq!(normalize_site_url("a.example"), None);
        assert_eq!(normalize_site_url(""), None);
    }

    #[test]
    fn invalid_payloads_are_empty() {
        assert!(parse_search_response(&serde_json::json!({"data": "nope"})).is_empty());
        assert!(parse_search_response(&serde_json::json!({})).is_empty());
        assert!(parse_calendar_response(&serde_json::json!({"not": "array"})).is_empty());
        assert!(parse_subject(&serde_json::json!({"name": "no id"})).is_none());
        assert!(parse_subject(&serde_json::json!({"id": 1, "name": "  "})).is_none());
    }

    #[test]
    fn dedup_keeps_first_and_sorts_by_date() {
        let mut raw = parse_search_response(&search_fixture());
        raw.push(raw[0].clone());
        let unique = dedup_subjects(raw);
        assert_eq!(unique.len(), 3);
        let dates: Vec<_> = unique.iter().map(|subject| subject.air_date).collect();
        let mut sorted = dates.clone();
        sorted.sort();
        assert_eq!(dates, sorted);
    }
}
