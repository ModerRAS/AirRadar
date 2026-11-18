//! Season naming and date ranges.
//!
//! Season label format: YYYY-season, where season is winter (Jan-Mar),
//! spring (Apr-Jun), summer (Jul-Sep) or fall (Oct-Dec). Dates are JST.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Broadcast season (calendar quarter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Season {
    Winter,
    Spring,
    Summer,
    Fall,
}

impl Season {
    /// Season containing the given calendar month (1-12).
    pub fn from_month(month: u32) -> Season {
        match month {
            1..=3 => Season::Winter,
            4..=6 => Season::Spring,
            7..=9 => Season::Summer,
            _ => Season::Fall,
        }
    }

    pub fn first_month(&self) -> u32 {
        match self {
            Season::Winter => 1,
            Season::Spring => 4,
            Season::Summer => 7,
            Season::Fall => 10,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Season::Winter => "winter",
            Season::Spring => "spring",
            Season::Summer => "summer",
            Season::Fall => "fall",
        }
    }

    pub fn parse(value: &str) -> Option<Season> {
        match value.trim().to_ascii_lowercase().as_str() {
            "winter" => Some(Season::Winter),
            "spring" => Some(Season::Spring),
            "summer" => Some(Season::Summer),
            "fall" | "autumn" => Some(Season::Fall),
            _ => None,
        }
    }
}

impl fmt::Display for Season {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// JST fixed offset (Asia/Tokyo, UTC+9, no DST).
pub fn jst_offset() -> FixedOffset {
    FixedOffset::east_opt(9 * 3600).expect("valid JST offset")
}

/// A concrete season, e.g. 2026-fall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeasonRef {
    pub year: i32,
    pub season: Season,
}

impl SeasonRef {
    pub fn new(year: i32, season: Season) -> SeasonRef {
        SeasonRef { year, season }
    }

    /// Season containing a calendar date.
    pub fn from_date(date: NaiveDate) -> SeasonRef {
        SeasonRef::new(date.year(), Season::from_month(date.month()))
    }

    /// Season containing "now" in JST.
    pub fn current(now: DateTime<Utc>) -> SeasonRef {
        let local = now.with_timezone(&jst_offset());
        SeasonRef::from_date(local.date_naive())
    }

    /// Parse YYYY-season (case insensitive, accepts autumn alias).
    pub fn parse(value: &str) -> Option<SeasonRef> {
        let value = value.trim();
        let (year_text, season_text) = value.split_once('-')?;
        let year: i32 = year_text.parse().ok()?;
        if !(1900..=2200).contains(&year) {
            return None;
        }
        let season = Season::parse(season_text)?;
        Some(SeasonRef::new(year, season))
    }

    pub fn label(&self) -> String {
        format!("{}-{}", self.year, self.season.label())
    }

    /// First day of the season (inclusive).
    pub fn start(&self) -> NaiveDate {
        NaiveDate::from_ymd_opt(self.year, self.season.first_month(), 1)
            .expect("valid season start date")
    }

    /// First day of the next season (exclusive end of this season).
    pub fn end_exclusive(&self) -> NaiveDate {
        self.next().start()
    }

    /// Last day of the season (inclusive).
    pub fn end_inclusive(&self) -> NaiveDate {
        self.end_exclusive()
            .pred_opt()
            .expect("valid season end date")
    }

    pub fn next(&self) -> SeasonRef {
        match self.season {
            Season::Winter => SeasonRef::new(self.year, Season::Spring),
            Season::Spring => SeasonRef::new(self.year, Season::Summer),
            Season::Summer => SeasonRef::new(self.year, Season::Fall),
            Season::Fall => SeasonRef::new(self.year + 1, Season::Winter),
        }
    }

    pub fn previous(&self) -> SeasonRef {
        match self.season {
            Season::Winter => SeasonRef::new(self.year - 1, Season::Fall),
            Season::Spring => SeasonRef::new(self.year, Season::Winter),
            Season::Summer => SeasonRef::new(self.year, Season::Spring),
            Season::Fall => SeasonRef::new(self.year, Season::Summer),
        }
    }

    /// Monotonic season rank used for retention windows.
    pub fn rank(&self) -> i64 {
        let quarter = match self.season {
            Season::Winter => 0,
            Season::Spring => 1,
            Season::Summer => 2,
            Season::Fall => 3,
        };
        self.year as i64 * 4 + quarter
    }
}

impl fmt::Display for SeasonRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

impl FromStr for SeasonRef {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        SeasonRef::parse(value).ok_or_else(|| {
            format!("invalid season {value:?}; expected YYYY-season (e.g. 2026-fall)")
        })
    }
}

impl Serialize for SeasonRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.label())
    }
}

impl<'de> Deserialize<'de> for SeasonRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        SeasonRef::parse(&text)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid season {text:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn month_mapping() {
        assert_eq!(Season::from_month(1), Season::Winter);
        assert_eq!(Season::from_month(3), Season::Winter);
        assert_eq!(Season::from_month(4), Season::Spring);
        assert_eq!(Season::from_month(7), Season::Summer);
        assert_eq!(Season::from_month(10), Season::Fall);
        assert_eq!(Season::from_month(12), Season::Fall);
    }

    #[test]
    fn parse_and_label() {
        assert_eq!(
            SeasonRef::parse("2026-fall"),
            Some(SeasonRef::new(2026, Season::Fall))
        );
        assert_eq!(
            SeasonRef::parse("2026-FALL"),
            Some(SeasonRef::new(2026, Season::Fall))
        );
        assert_eq!(
            SeasonRef::parse("2026-autumn"),
            Some(SeasonRef::new(2026, Season::Fall))
        );
        assert_eq!(
            SeasonRef::parse("2026-winter").unwrap().label(),
            "2026-winter"
        );
        assert_eq!(SeasonRef::parse("2026"), None);
        assert_eq!(SeasonRef::parse("2026-summer-x"), None);
        assert_eq!(SeasonRef::parse("26-fall"), None);
        assert_eq!(SeasonRef::parse("2026-springtime"), None);
        assert_eq!("2026-fall".parse::<SeasonRef>().unwrap().year, 2026);
    }

    #[test]
    fn date_ranges() {
        let fall = SeasonRef::new(2026, Season::Fall);
        assert_eq!(fall.start(), date(2026, 10, 1));
        assert_eq!(fall.end_inclusive(), date(2026, 12, 31));
        assert_eq!(fall.end_exclusive(), date(2027, 1, 1));
        assert_eq!(fall.next(), SeasonRef::new(2027, Season::Winter));
        assert_eq!(fall.previous(), SeasonRef::new(2026, Season::Summer));
        let winter = SeasonRef::new(2026, Season::Winter);
        assert_eq!(winter.end_inclusive(), date(2026, 3, 31));
        assert_eq!(winter.previous(), SeasonRef::new(2025, Season::Fall));
    }

    #[test]
    fn current_season_uses_jst() {
        // 2026-11-18T13:09:53+08:00 == 2026-11-18T14:09:53Z == 2026-11-18 23:09 JST.
        let ts = DateTime::parse_from_rfc3339("2026-11-18T14:09:53Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(SeasonRef::current(ts), SeasonRef::new(2026, Season::Fall));
        // 2026-12-31T15:30Z is 2027-01-01 00:30 JST -> winter 2027.
        let new_year = DateTime::parse_from_rfc3339("2026-12-31T15:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            SeasonRef::current(new_year),
            SeasonRef::new(2027, Season::Winter)
        );
        // 2026-03-31T14:59Z is 2026-03-31 23:59 JST -> winter 2026.
        let ts2 = DateTime::parse_from_rfc3339("2026-03-31T14:59:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            SeasonRef::current(ts2),
            SeasonRef::new(2026, Season::Winter)
        );
    }

    #[test]
    fn rank_is_monotonic() {
        let seasons = [
            SeasonRef::new(2025, Season::Fall),
            SeasonRef::new(2026, Season::Winter),
            SeasonRef::new(2026, Season::Fall),
            SeasonRef::new(2027, Season::Winter),
        ];
        for pair in seasons.windows(2) {
            assert!(pair[0].rank() < pair[1].rank());
        }
    }

    #[test]
    fn serde_round_trip() {
        let season = SeasonRef::new(2027, Season::Winter);
        let json = serde_json::to_string(&season).unwrap();
        assert_eq!(json, "\"2027-winter\"");
        let back: SeasonRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, season);
    }
}
