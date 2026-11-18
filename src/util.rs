use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};

/// Current UTC timestamp.
pub fn now() -> DateTime<Utc> {
    Utc::now()
}

/// RFC3339 UTC timestamp with second precision.
pub fn iso(ts: &DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Lower-cased host part of an absolute http(s) URL, with a leading www. removed.
pub fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    let host = if host_port.starts_with('[') {
        format!("{}]", host_port.split(']').next()?)
    } else {
        host_port.split(':').next()?.to_string()
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Source domain used for cross-check independence; falls back to "unknown".
pub fn domain_of(url: &str) -> String {
    url_host(url).unwrap_or_else(|| "unknown".to_string())
}

/// True for absolute http/https URLs.
pub fn is_http_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Stable 64-bit hash rendered as 16 hex chars (cache keys, dedupe).
pub fn hash_hex(input: &str) -> String {
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Minimal percent-encoding for URL query values.
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{:02X}", other)),
        }
    }
    out
}

/// Truncate to at most max_chars characters without splitting a UTF-8 char.
pub fn truncate_chars(input: &str, max_chars: usize) -> &str {
    match input.char_indices().nth(max_chars) {
        Some((idx, _)) => &input[..idx],
        None => input,
    }
}

/// Owned variant of truncate_chars.
pub fn truncate_chars_owned(input: &str, max_chars: usize) -> String {
    truncate_chars(input, max_chars).to_string()
}

/// Collapse all whitespace runs into single spaces and trim.
pub fn collapse_ws(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clamp_boundary(input: &str, mut index: usize) -> usize {
    index = index.min(input.len());
    while index > 0 && !input.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Text window around a byte range, collapsed to one line.
pub fn context_around(input: &str, start: usize, end: usize, radius_chars: usize) -> String {
    let start = clamp_boundary(input, start);
    let end = clamp_boundary(input, end).max(start);
    let mut left = start;
    let mut seen = 0usize;
    for (idx, _) in input[..start].char_indices().rev() {
        if seen >= radius_chars {
            break;
        }
        left = idx;
        seen += 1;
    }
    let mut right = end;
    seen = 0;
    for (idx, ch) in input[end..].char_indices() {
        if seen >= radius_chars {
            break;
        }
        right = end + idx + ch.len_utf8();
        seen += 1;
    }
    collapse_ws(&input[left..right])
}

static RNG_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Cheap pseudo-random u64 (splitmix64 seeded by clock, pid and counter).
pub fn random_u64() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = RNG_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut x = nanos
        ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (std::process::id() as u64).rotate_left(32);
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Uniform f64 in [0, 1).
pub fn random_f64() -> f64 {
    (random_u64() >> 11) as f64 / ((1u64 << 53) as f64)
}

/// Human friendly run id like 20261118T051500Z-1a2b3c4d.
pub fn new_run_id() -> String {
    format!(
        "{}-{:08x}",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        (random_u64() & 0xffff_ffff) as u32
    )
}

/// Parse common truthy/falsy spellings; None when not recognized.
pub fn parse_bool(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Format a 24h "HH:MM" clock time.
pub fn format_time(hour: u32, minute: u32) -> String {
    format!("{:02}:{:02}", hour, minute)
}

/// Validate a canonical "HH:MM" string (00:00..=23:59).
pub fn is_valid_time(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return false;
    }
    let hour = text[0..2].parse::<u32>().ok();
    let minute = text[3..5].parse::<u32>().ok();
    matches!((hour, minute), (Some(h), Some(m)) if h < 24 && m < 60)
}

/// Next ISO weekday (1=Mon..7=Sun), wrapping to Monday.
pub fn next_weekday(weekday: u8) -> u8 {
    if weekday >= 7 {
        1
    } else {
        weekday + 1
    }
}

/// Chinese label for an ISO weekday.
pub fn weekday_zh(weekday: u8) -> &'static str {
    match weekday {
        1 => "周一",
        2 => "周二",
        3 => "周三",
        4 => "周四",
        5 => "周五",
        6 => "周六",
        _ => "周日",
    }
}

/// Japanese label for an ISO weekday.
pub fn weekday_ja(weekday: u8) -> &'static str {
    match weekday {
        1 => "月",
        2 => "火",
        3 => "水",
        4 => "木",
        5 => "金",
        6 => "土",
        _ => "日",
    }
}

/// English short label for an ISO weekday.
pub fn weekday_en(weekday: u8) -> &'static str {
    match weekday {
        1 => "Mon",
        2 => "Tue",
        3 => "Wed",
        4 => "Thu",
        5 => "Fri",
        6 => "Sat",
        _ => "Sun",
    }
}
