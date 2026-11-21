//! Rule-first broadcast-time extraction.
//!
//! Japanese and Chinese schedule pages use many spellings for the same weekly
//! slot. Every extractor normalises to an ISO weekday (1=Mon..7=Sun) and HH:MM
//! in JST. Late-night notation is converted to the actual calendar day:
//! "土曜 25:15" and "土曜 深夜1:15" both become Sunday 01:15.

use std::sync::LazyLock;

use regex::Regex;

use crate::util::{context_around, format_time, next_weekday, truncate_chars};

/// Radius (in characters) around a match in which the subject title must occur.
pub const TITLE_CONTEXT_RADIUS: usize = 250;

/// Maximum slots returned by extract_slots.
pub const MAX_SLOTS: usize = 100;

/// One extraction result, before attribution to a subject/source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawSlot {
    /// ISO weekday in JST (1=Mon..7=Sun), already shifted for late-night shows.
    pub weekday: u8,
    /// HH:MM JST when known.
    pub time: Option<String>,
    pub method: &'static str,
    pub matched: String,
    /// Byte range of the match in the normalized text.
    pub start: usize,
    pub end: usize,
}

static JP_TIME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:毎週)?[\s]*(?P<w>[月火水木金土日])[\s]*曜日?[\s（(【\[]*(?P<deep>深夜|深)?[\s]*(?P<h>\d{1,2})[\s]*:[\s]*(?P<m>\d{2})",
    )
    .expect("valid JP colon regex")
});

static JP_HOUR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:毎週)?[\s]*(?P<w>[月火水木金土日])[\s]*曜日?[\s（(【\[]*(?P<deep>深夜|深)?[\s]*(?P<h>\d{1,2})[\s]*時(?:[\s]*(?P<m>\d{1,2})[\s]*分)?",
    )
    .expect("valid JP hour regex")
});

static JP_PAREN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"毎週[\s]*[（(](?P<w>[月火水木金土日])[）)][\s]*(?P<deep>深夜|深)?[\s]*(?P<h>\d{1,2})[\s]*:[\s]*(?P<m>\d{2})",
    )
    .expect("valid JP parenthesized regex")
});

static JP_LATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:毎週)?[\s]*(?P<w>[月火水木金土日])[\s]*曜日?[\s（(【\[]*(?:深夜|深)")
        .expect("valid JP late-night regex")
});

static JP_WEEKLY_ONLY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"毎週[\s]*(?P<w>[月火水木金土日])[\s]*曜日?").expect("valid JP weekly regex")
});

static ZH_TIME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:每[\s]*)?(?:周|週|星期|礼拜)[\s]*(?P<w>[一二三四五六日天])[\s]*(?P<mark>晚上|凌晨|深夜|晚)?[\s]*(?P<h>\d{1,2})[\s]*:[\s]*(?P<m>\d{2})",
    )
    .expect("valid ZH colon regex")
});

static ZH_LATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:每[\s]*)?(?:周|週|星期|礼拜)[\s]*(?P<w>[一二三四五六日天])[\s]*(?:深夜|晚上)")
        .expect("valid ZH late-night regex")
});

static ZH_WEEKDAY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"每[\s]*(?:周|週|星期|礼拜)[\s]*(?P<w>[一二三四五六日天])[\s]*(?:播出|更新|放送|上线|开播)",
    )
    .expect("valid ZH weekly regex")
});

static EN_TIME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:every[\s]+)?(?P<w>monday|tuesday|wednesday|thursday|friday|saturday|sunday|mon|tue|wed|thu|fri|sat|sun)[a-z]*[\s]*(?:at[\s]+)?(?P<h>\d{1,2}):(?P<m>\d{2})",
    )
    .expect("valid EN regex")
});

static SEASON_SUFFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)[\s]*(?:season[\s]*\d+|第[一二三四五六七八九十0-9]+季|\d+(?:st|nd|rd|th)[\s]*season|ii+|iv)[\s]*$",
    )
    .expect("valid season suffix regex")
});

/// Normalise full-width digits, colons and spaces before matching.
fn normalize_input(input: &str) -> String {
    input
        .chars()
        .map(|ch| match ch {
            '０'..='９' => char::from(b'0' + (ch as u32 - '０' as u32) as u8),
            '：' => ':',
            '　' => ' ',
            '～' | '〜' => '~',
            other => other,
        })
        .collect()
}

fn jp_weekday(ch: char) -> Option<u8> {
    match ch {
        '月' => Some(1),
        '火' => Some(2),
        '水' => Some(3),
        '木' => Some(4),
        '金' => Some(5),
        '土' => Some(6),
        '日' => Some(7),
        _ => None,
    }
}

fn zh_weekday(ch: char) -> Option<u8> {
    match ch {
        '一' => Some(1),
        '二' => Some(2),
        '三' => Some(3),
        '四' => Some(4),
        '五' => Some(5),
        '六' => Some(6),
        '日' | '天' => Some(7),
        _ => None,
    }
}

fn en_weekday(text: &str) -> Option<u8> {
    match text.to_ascii_lowercase().as_str() {
        "monday" | "mon" => Some(1),
        "tuesday" | "tue" => Some(2),
        "wednesday" | "wed" => Some(3),
        "thursday" | "thu" => Some(4),
        "friday" | "fri" => Some(5),
        "saturday" | "sat" => Some(6),
        "sunday" | "sun" => Some(7),
        _ => None,
    }
}

/// Build a normalized slot from a raw weekday / hour / minute observation.
fn build_slot(
    weekday: u8,
    late_night: bool,
    hour: u32,
    minute: u32,
    method: &'static str,
    matched: String,
    start: usize,
    end: usize,
) -> Option<RawSlot> {
    if minute > 59 || !(1..=7).contains(&weekday) {
        return None;
    }
    let mut day = weekday;
    let mut clock_hour = hour;
    if hour >= 24 {
        // 24:30, 25:15, ... are next-day clock times.
        if hour > 29 {
            return None;
        }
        clock_hour = hour - 24;
        day = next_weekday(day);
    } else if late_night && hour < 6 {
        // "土曜 深夜1:15" is the night that follows Saturday.
        day = next_weekday(day);
    }
    Some(RawSlot {
        weekday: day,
        time: Some(format_time(clock_hour, minute)),
        method,
        matched: truncate_chars(&matched, 80).to_string(),
        start,
        end,
    })
}

fn capture_u32(caps: &regex::Captures<'_>, name: &str) -> Option<u32> {
    caps.name(name)?.as_str().parse::<u32>().ok()
}

fn capture_u32_or(caps: &regex::Captures<'_>, name: &str, default: u32) -> u32 {
    capture_u32(caps, name).unwrap_or(default)
}

/// True when two byte ranges overlap.
fn overlaps(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

fn record_timed(timed: &mut Vec<RawSlot>, ranges: &mut Vec<(usize, usize)>, slot: Option<RawSlot>) {
    let Some(slot) = slot else {
        return;
    };
    // Track every parsed match, including duplicates, so weekday-only hints
    // that overlap an already parsed match are suppressed.
    ranges.push((slot.start, slot.end));
    if timed
        .iter()
        .any(|existing| existing.weekday == slot.weekday && existing.time == slot.time)
    {
        return;
    }
    timed.push(slot);
}

/// Extract all broadcast slots from a text blob (no title attribution).
pub fn extract_slots(text: &str) -> Vec<RawSlot> {
    let normalized = normalize_input(text);
    let mut timed: Vec<RawSlot> = Vec::new();
    let mut timed_ranges: Vec<(usize, usize)> = Vec::new();

    for caps in JP_TIME_RE.captures_iter(&normalized) {
        let Some(matched) = caps.get(0) else { continue };
        let weekday = caps
            .name("w")
            .and_then(|m| jp_weekday(m.as_str().chars().next()?));
        let deep = caps.name("deep").is_some();
        let hour = capture_u32(&caps, "h");
        let minute = capture_u32(&caps, "m");
        if let (Some(weekday), Some(hour), Some(minute)) = (weekday, hour, minute) {
            record_timed(
                &mut timed,
                &mut timed_ranges,
                build_slot(
                    weekday,
                    deep,
                    hour,
                    minute,
                    "jp:weekly-colon",
                    matched.as_str().to_string(),
                    matched.start(),
                    matched.end(),
                ),
            );
        }
    }

    for caps in JP_HOUR_RE.captures_iter(&normalized) {
        let Some(matched) = caps.get(0) else { continue };
        let weekday = caps
            .name("w")
            .and_then(|m| jp_weekday(m.as_str().chars().next()?));
        let deep = caps.name("deep").is_some();
        let hour = capture_u32(&caps, "h");
        let minute = capture_u32_or(&caps, "m", 0);
        if let (Some(weekday), Some(hour)) = (weekday, hour) {
            record_timed(
                &mut timed,
                &mut timed_ranges,
                build_slot(
                    weekday,
                    deep,
                    hour,
                    minute,
                    "jp:weekly-hour",
                    matched.as_str().to_string(),
                    matched.start(),
                    matched.end(),
                ),
            );
        }
    }

    for caps in JP_PAREN_RE.captures_iter(&normalized) {
        let Some(matched) = caps.get(0) else { continue };
        let weekday = caps
            .name("w")
            .and_then(|m| jp_weekday(m.as_str().chars().next()?));
        let deep = caps.name("deep").is_some();
        let hour = capture_u32(&caps, "h");
        let minute = capture_u32(&caps, "m");
        if let (Some(weekday), Some(hour), Some(minute)) = (weekday, hour, minute) {
            record_timed(
                &mut timed,
                &mut timed_ranges,
                build_slot(
                    weekday,
                    deep,
                    hour,
                    minute,
                    "jp:weekly-parenthesized",
                    matched.as_str().to_string(),
                    matched.start(),
                    matched.end(),
                ),
            );
        }
    }

    for caps in ZH_TIME_RE.captures_iter(&normalized) {
        let Some(matched) = caps.get(0) else { continue };
        let weekday = caps
            .name("w")
            .and_then(|m| zh_weekday(m.as_str().chars().next()?));
        let marker = caps.name("mark").map(|m| m.as_str()).unwrap_or("");
        let deep = marker.contains('深');
        let hour = capture_u32(&caps, "h");
        let minute = capture_u32(&caps, "m");
        if let (Some(weekday), Some(hour), Some(minute)) = (weekday, hour, minute) {
            record_timed(
                &mut timed,
                &mut timed_ranges,
                build_slot(
                    weekday,
                    deep,
                    hour,
                    minute,
                    "zh:weekly-colon",
                    matched.as_str().to_string(),
                    matched.start(),
                    matched.end(),
                ),
            );
        }
    }

    for caps in EN_TIME_RE.captures_iter(&normalized) {
        let Some(matched) = caps.get(0) else { continue };
        let weekday = caps.name("w").and_then(|m| en_weekday(m.as_str()));
        let hour = capture_u32(&caps, "h");
        let minute = capture_u32(&caps, "m");
        if let (Some(weekday), Some(hour), Some(minute)) = (weekday, hour, minute) {
            record_timed(
                &mut timed,
                &mut timed_ranges,
                build_slot(
                    weekday,
                    false,
                    hour,
                    minute,
                    "en:weekly",
                    matched.as_str().to_string(),
                    matched.start(),
                    matched.end(),
                ),
            );
        }
    }

    let mut hints: Vec<RawSlot> = Vec::new();

    let mut push_hint =
        |weekday: u8, method: &'static str, matched: String, start: usize, end: usize| {
            if timed_ranges
                .iter()
                .any(|range| overlaps(*range, (start, end)))
            {
                return;
            }
            if hints.iter().any(|hint| hint.weekday == weekday) {
                return;
            }
            hints.push(RawSlot {
                weekday,
                time: None,
                method,
                matched: truncate_chars(&matched, 80).to_string(),
                start,
                end,
            });
        };

    for caps in JP_LATE_RE.captures_iter(&normalized) {
        let Some(matched) = caps.get(0) else { continue };
        let Some(weekday) = caps
            .name("w")
            .and_then(|m| jp_weekday(m.as_str().chars().next()?))
        else {
            continue;
        };
        push_hint(
            weekday,
            "jp:late-night-only",
            matched.as_str().to_string(),
            matched.start(),
            matched.end(),
        );
    }

    for caps in JP_WEEKLY_ONLY_RE.captures_iter(&normalized) {
        let Some(matched) = caps.get(0) else { continue };
        let Some(weekday) = caps
            .name("w")
            .and_then(|m| jp_weekday(m.as_str().chars().next()?))
        else {
            continue;
        };
        push_hint(
            weekday,
            "jp:weekday-only",
            matched.as_str().to_string(),
            matched.start(),
            matched.end(),
        );
    }

    for caps in ZH_LATE_RE.captures_iter(&normalized) {
        let Some(matched) = caps.get(0) else { continue };
        let Some(weekday) = caps
            .name("w")
            .and_then(|m| zh_weekday(m.as_str().chars().next()?))
        else {
            continue;
        };
        push_hint(
            weekday,
            "zh:late-night-only",
            matched.as_str().to_string(),
            matched.start(),
            matched.end(),
        );
    }

    for caps in ZH_WEEKDAY_RE.captures_iter(&normalized) {
        let Some(matched) = caps.get(0) else { continue };
        let Some(weekday) = caps
            .name("w")
            .and_then(|m| zh_weekday(m.as_str().chars().next()?))
        else {
            continue;
        };
        push_hint(
            weekday,
            "zh:weekday-only",
            matched.as_str().to_string(),
            matched.start(),
            matched.end(),
        );
    }

    timed.extend(hints);
    timed.sort_by_key(|slot| slot.start);
    timed.truncate(MAX_SLOTS);
    timed
}

/// Strip a trailing season marker (Season 2, 第二季, 2nd Season, II).
pub fn strip_season_suffix(title: &str) -> String {
    SEASON_SUFFIX_RE
        .replace(title.trim(), "")
        .trim()
        .to_string()
}

fn normalize_name(input: &str) -> String {
    input
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if !value.is_empty() && !list.contains(&value) {
        list.push(value);
    }
}

/// Title variants used for context matching: full name plus season-stripped base.
pub fn title_variants(names: &[String]) -> Vec<String> {
    let mut variants = Vec::new();
    for name in names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        push_unique(&mut variants, trimmed.to_string());
        let base = strip_season_suffix(trimmed);
        if !base.is_empty() {
            push_unique(&mut variants, base);
        }
    }
    variants
}

fn line_bounds(text: &str, start: usize, end: usize) -> (usize, usize) {
    let line_start = text[..start]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    let line_end = text[end..]
        .find('\n')
        .map(|index| end + index)
        .unwrap_or(text.len());
    (line_start, line_end)
}

/// True when any title variant occurs anywhere in the text (normalised,
/// case-insensitive). Used as a lenient context signal for search snippets.
pub fn text_mentions_title(text: &str, names: &[String]) -> bool {
    let normalized_text = normalize_name(text);
    title_variants(names)
        .iter()
        .map(|name| normalize_name(name))
        .filter(|name| name.chars().count() >= 2)
        .any(|name| normalized_text.contains(name.as_str()))
}

/// Extract slots and keep only those attributable to the subject.
///
/// On table rows (converted to one line per row by the HTML extractor) the
/// title must occur on the same row, which prevents a season-wide schedule
/// table from assigning every other show's time to this subject. In prose
/// pages the title may be a heading one or two lines above the match, so a
/// TITLE_CONTEXT_RADIUS character window is also accepted.
/// With an empty names list all slots are returned (used by probe tooling and
/// dedicated official sites that only describe one show).
pub fn extract_for_subject(text: &str, names: &[String]) -> Vec<RawSlot> {
    let slots = extract_slots(text);
    if names.is_empty() {
        return slots;
    }
    let normalized_text = normalize_input(text);
    let wanted: Vec<String> = title_variants(names)
        .iter()
        .map(|name| normalize_name(name))
        .filter(|name| name.chars().count() >= 2)
        .collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    slots
        .into_iter()
        .filter(|slot| {
            let (line_start, line_end) = line_bounds(&normalized_text, slot.start, slot.end);
            let line = &normalized_text[line_start..line_end];
            let normalized_line = normalize_name(line);
            if wanted.iter().any(|name| normalized_line.contains(name)) {
                return true;
            }
            // Table rows are authoritative: no cross-row guessing.
            if line.contains('|') {
                return false;
            }
            let window =
                context_around(&normalized_text, slot.start, slot.end, TITLE_CONTEXT_RADIUS);
            let normalized_window = normalize_name(&window);
            wanted.iter().any(|name| normalized_window.contains(name))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn times(slots: &[RawSlot]) -> Vec<(u8, Option<&str>)> {
        slots
            .iter()
            .map(|slot| (slot.weekday, slot.time.as_deref()))
            .collect()
    }

    fn find(slots: &[RawSlot], weekday: u8, time: &str) -> bool {
        slots
            .iter()
            .any(|slot| slot.weekday == weekday && slot.time.as_deref() == Some(time))
    }

    #[test]
    fn japanese_weekly_colon() {
        let slots = extract_slots("毎週土曜日 23:30 放送");
        assert_eq!(times(&slots), vec![(6, Some("23:30"))]);
        let slots = extract_slots("毎週 月曜 25:15");
        assert!(find(&slots, 2, "01:15"));
        let slots = extract_slots("毎週金曜日23:00〜");
        assert!(find(&slots, 5, "23:00"));
    }

    #[test]
    fn japanese_hour_notation() {
        let slots = extract_slots("毎週水曜日 22時30分より放送");
        assert!(find(&slots, 3, "22:30"));
        let slots = extract_slots("毎週木曜 25時15分");
        assert!(find(&slots, 5, "01:15"));
        let slots = extract_slots("毎週金曜 深夜1時");
        assert!(find(&slots, 6, "01:00"));
    }

    #[test]
    fn japanese_late_night_shifts_day() {
        let slots = extract_slots("毎週土曜日 深25:15");
        assert_eq!(times(&slots), vec![(7, Some("01:15"))]);
        let slots = extract_slots("毎週土曜 深夜1:15");
        assert_eq!(times(&slots), vec![(7, Some("01:15"))]);
        let slots = extract_slots("毎週日曜 深夜0:30");
        assert_eq!(times(&slots), vec![(1, Some("00:30"))]);
        let slots = extract_slots("毎週火曜 24:00");
        assert_eq!(times(&slots), vec![(3, Some("00:00"))]);
        // Late-night marker with a normal evening hour stays on the same day.
        let slots = extract_slots("毎週金曜 深23:30");
        assert_eq!(times(&slots), vec![(5, Some("23:30"))]);
    }

    #[test]
    fn japanese_parenthesized_weekday() {
        let slots = extract_slots("毎週(木) 21:00");
        assert_eq!(times(&slots), vec![(4, Some("21:00"))]);
        let slots = extract_slots("毎週（日）24:30");
        assert_eq!(times(&slots), vec![(1, Some("00:30"))]);
    }

    #[test]
    fn chinese_weekly_forms() {
        let slots = extract_slots("每周六 23:30 播出");
        assert!(find(&slots, 6, "23:30"));
        let slots = extract_slots("周六 23:30");
        assert!(find(&slots, 6, "23:30"));
        let slots = extract_slots("星期天 01:30");
        assert!(find(&slots, 7, "01:30"));
        let slots = extract_slots("每周日 24:30");
        assert!(find(&slots, 1, "00:30"));
        let slots = extract_slots("每周六晚 22:00");
        assert!(find(&slots, 6, "22:00"));
    }

    #[test]
    fn chinese_hints() {
        let slots = extract_slots("每周日 深夜 播出");
        assert_eq!(times(&slots), vec![(7, None)]);
        let slots = extract_slots("每周二 更新");
        assert_eq!(times(&slots), vec![(2, None)]);
    }

    #[test]
    fn english_weekly() {
        let slots = extract_slots("Every Saturday at 23:30 JST");
        assert!(find(&slots, 6, "23:30"));
        let slots = extract_slots("Sundays 01:15 (JST)");
        assert!(find(&slots, 7, "01:15"));
        let slots = extract_slots("Every Sunday 25:30");
        assert!(find(&slots, 1, "01:30"));
    }

    #[test]
    fn full_width_digits_and_colons() {
        let slots = extract_slots("毎週土曜日　２３：３０");
        assert!(find(&slots, 6, "23:30"));
        let slots = extract_slots("每周六 ２３：３０");
        assert!(find(&slots, 6, "23:30"));
    }

    #[test]
    fn invalid_input_is_ignored() {
        assert!(extract_slots("").is_empty());
        let slots = extract_slots("毎週土曜 25:99");
        assert!(
            slots.iter().all(|slot| slot.time.is_none()),
            "no timed slot expected"
        );
        assert_eq!(
            times(&slots),
            vec![(6, None)],
            "weekday hint is still useful"
        );
        assert!(extract_slots("毎週土曜 30:00")
            .iter()
            .all(|slot| slot.time.is_none()));
        assert!(extract_slots("毎週 23:30").is_empty());
        assert!(extract_slots("weekly 25:30").is_empty());
        assert_eq!(extract_slots("毎週土曜 23:30").len(), 1);
    }

    #[test]
    fn duplicate_slots_are_deduplicated() {
        let slots = extract_slots("毎週土曜 23:30 / 毎週土曜日 23:30");
        assert_eq!(times(&slots), vec![(6, Some("23:30"))]);
    }

    #[test]
    fn late_night_hint_is_suppressed_by_timed_match() {
        let slots = extract_slots("毎週土曜 深夜1:15");
        assert_eq!(times(&slots), vec![(7, Some("01:15"))]);
    }

    #[test]
    fn weekday_only_hint() {
        let slots = extract_slots("毎週金曜日 放送");
        assert_eq!(times(&slots), vec![(5, None)]);
        assert_eq!(slots[0].method, "jp:weekday-only");
    }

    #[test]
    fn title_mentions_title_anywhere() {
        let text = format!("作品A {} 毎週土曜 23:30", "x".repeat(400));
        assert!(text_mentions_title(&text, &["作品A".to_string()]));
        assert!(!text_mentions_title(&text, &["作品B".to_string()]));
        // Short/empty titles never match.
        assert!(!text_mentions_title(&text, &["A".to_string()]));
    }

    #[test]
    fn season_suffix_stripping() {
        assert_eq!(
            strip_season_suffix("探偵はもう、死んでいる。 Season 2"),
            "探偵はもう、死んでいる。"
        );
        assert_eq!(
            strip_season_suffix("侦探已经死了。 第二季"),
            "侦探已经死了。"
        );
        assert_eq!(strip_season_suffix("Show 2nd Season"), "Show");
        assert_eq!(strip_season_suffix("Plain Title"), "Plain Title");
        let variants = title_variants(&["Show Season 2".to_string()]);
        assert_eq!(variants, vec!["Show Season 2", "Show"]);
    }

    #[test]
    fn title_context_filter_keeps_only_matching_table_row() {
        let text = "作品A | 毎週土曜 23:30\n作品B | 毎週日曜 01:00\n作品C | 毎週月曜 22:00";
        let slots = extract_for_subject(text, &["作品B".to_string()]);
        assert_eq!(times(&slots), vec![(7, Some("01:00"))]);
        // Titles in neighbouring rows must not leak.
        let slots = extract_for_subject(text, &["作品A".to_string()]);
        assert_eq!(times(&slots), vec![(6, Some("23:30"))]);
    }

    #[test]
    fn title_context_filter_supports_heading_style_pages() {
        let text = "『作品B』\n放送情報\n毎週日曜 01:00 より放送開始";
        let slots = extract_for_subject(text, &["作品B".to_string()]);
        assert_eq!(times(&slots), vec![(7, Some("01:00"))]);
    }

    #[test]
    fn title_context_filter_handles_season_suffix() {
        let text = "侦探已经死了。 第二季\n每周三 23:00 播出";
        let slots = extract_for_subject(text, &["侦探已经死了。 第二季".to_string()]);
        assert_eq!(times(&slots), vec![(3, Some("23:00"))]);
    }

    #[test]
    fn title_context_filter_rejects_unknown_show() {
        let text = "作品A | 毎週土曜 23:30\n作品B | 毎週日曜 01:00";
        let slots = extract_for_subject(text, &["完全不同".to_string()]);
        assert!(slots.is_empty());
    }

    #[test]
    fn duplicate_slots_keep_all_match_ranges_for_hint_suppression() {
        let slots = extract_slots("毎週土曜 23:30 / 毎週土曜日 23:30");
        assert_eq!(times(&slots), vec![(6, Some("23:30"))]);
    }

    #[test]
    fn empty_names_return_all_slots() {
        let text = "作品A 毎週土曜 23:30 作品B 毎週日曜 01:00";
        let slots = extract_for_subject(text, &[]);
        assert_eq!(slots.len(), 2);
    }
}
