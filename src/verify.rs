//! Cross-source verification and confidence assignment.
//!
//! Independence is defined by source domain, not by the number of pages: a
//! snippet and a fetched page from the same domain count once.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{Confidence, ResolvedSlot, ReviewStatus, TimeCandidate};
use crate::util::{weekday_en, weekday_zh};

/// Result of cross-checking all candidates for one subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    pub resolved: Option<ResolvedSlot>,
    pub confidence: Confidence,
    pub status: ReviewStatus,
    pub reasons: Vec<String>,
    /// Independent source domains behind the winning slot (sorted).
    pub domains: Vec<String>,
}

impl Verification {
    pub fn is_published(&self) -> bool {
        self.status == ReviewStatus::Published && self.resolved.is_some()
    }

    fn unknown(reasons: Vec<String>) -> Verification {
        Verification {
            resolved: None,
            confidence: Confidence::Unknown,
            status: ReviewStatus::Unknown,
            reasons,
            domains: Vec::new(),
        }
    }
}

fn domain_id(candidate: &TimeCandidate) -> String {
    let domain = candidate.source_domain.trim().to_ascii_lowercase();
    let domain = domain.strip_prefix("www.").unwrap_or(&domain).to_string();
    if domain.is_empty() || domain == "unknown" {
        let provider = candidate.provider.trim();
        if provider.is_empty() {
            "unknown".to_string()
        } else {
            format!("provider:{provider}")
        }
    } else {
        domain
    }
}

fn slot_label(weekday: u8, time: &str) -> String {
    format!(
        "{} {} ({} UTC+9)",
        weekday_en(weekday),
        time,
        weekday_zh(weekday)
    )
}

fn conflict_reason(
    slots: &BTreeMap<(u8, String), BTreeSet<String>>,
    winner: Option<&(u8, String)>,
) -> String {
    let mut parts = Vec::new();
    for ((weekday, time), domains) in slots {
        let label = slot_label(*weekday, time);
        let sources = domains.iter().cloned().collect::<Vec<_>>().join(", ");
        if winner
            .map(|winner| winner == &(*weekday, time.clone()))
            .unwrap_or(false)
        {
            parts.push(format!("{label} <- {sources}"));
        } else {
            parts.push(format!("{label} <- {sources}"));
        }
    }
    format!("conflict: {}", parts.join("; "))
}

/// Cross-check candidates and decide publication.
pub fn verify(candidates: &[TimeCandidate]) -> Verification {
    let mut slots: BTreeMap<(u8, String), BTreeSet<String>> = BTreeMap::new();
    for candidate in candidates {
        let Some(slot) = candidate.slot() else {
            continue;
        };
        slots
            .entry((slot.weekday, slot.time))
            .or_default()
            .insert(domain_id(candidate));
    }

    if slots.is_empty() {
        let hint = candidates.iter().find_map(|candidate| {
            candidate
                .weekday
                .filter(|weekday| (1..=7).contains(weekday))
        });
        let mut reasons = vec!["no_time_found".to_string()];
        if let Some(weekday) = hint {
            reasons.push(format!(
                "weekday_hint: only weekday {} known, time missing",
                weekday_zh(weekday)
            ));
        }
        return Verification::unknown(reasons);
    }

    let best = slots.values().map(BTreeSet::len).max().unwrap_or(1);
    let top_slots: Vec<&(u8, String)> = slots
        .iter()
        .filter(|(_, domains)| domains.len() == best)
        .map(|(slot, _)| slot)
        .collect();

    if top_slots.len() > 1 {
        return Verification {
            resolved: None,
            confidence: Confidence::Low,
            status: ReviewStatus::Review,
            reasons: vec![conflict_reason(&slots, None)],
            domains: Vec::new(),
        };
    }

    let winning = top_slots[0];
    let winning_domains = &slots[winning];
    let other_conflicts: Vec<&(u8, String)> = slots
        .iter()
        .filter(|(slot, domains)| {
            *slot != winning
                && domains
                    .iter()
                    .any(|domain| !winning_domains.contains(domain))
        })
        .map(|(slot, _)| slot)
        .collect();

    if !other_conflicts.is_empty() {
        return Verification {
            resolved: None,
            confidence: Confidence::Low,
            status: ReviewStatus::Review,
            reasons: vec![conflict_reason(&slots, Some(winning))],
            domains: Vec::new(),
        };
    }

    let domains: Vec<String> = winning_domains.iter().cloned().collect();
    if best >= 2 {
        Verification {
            resolved: Some(ResolvedSlot::new(winning.0, winning.1.clone())),
            confidence: Confidence::High,
            status: ReviewStatus::Published,
            reasons: Vec::new(),
            domains,
        }
    } else if slots.len() == 1 {
        Verification {
            resolved: Some(ResolvedSlot::new(winning.0, winning.1.clone())),
            confidence: Confidence::Medium,
            status: ReviewStatus::Published,
            reasons: vec!["single_source".to_string()],
            domains,
        }
    } else {
        Verification {
            resolved: None,
            confidence: Confidence::Low,
            status: ReviewStatus::Review,
            reasons: vec![conflict_reason(&slots, Some(winning))],
            domains: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TimeCandidate;
    use chrono::Utc;

    fn candidate(weekday: u8, time: &str, domain: &str) -> TimeCandidate {
        TimeCandidate {
            weekday: Some(weekday),
            time: Some(time.to_string()),
            source_url: Some(format!("https://{domain}/page")),
            source_domain: domain.to_string(),
            provider: "mock".to_string(),
            method: "test".to_string(),
            evidence: format!("{domain} says {weekday} {time}"),
            observed_at: Utc::now(),
        }
    }

    fn hint(weekday: u8) -> TimeCandidate {
        TimeCandidate {
            weekday: Some(weekday),
            time: None,
            source_url: None,
            source_domain: "hint.example".to_string(),
            provider: "mock".to_string(),
            method: "test".to_string(),
            evidence: String::new(),
            observed_at: Utc::now(),
        }
    }

    #[test]
    fn two_independent_domains_agree_high() {
        let verification = verify(&[
            candidate(7, "01:15", "a.example"),
            candidate(7, "01:15", "b.example"),
        ]);
        assert_eq!(verification.confidence, Confidence::High);
        assert_eq!(verification.status, ReviewStatus::Published);
        assert_eq!(verification.resolved, Some(ResolvedSlot::new(7, "01:15")));
        assert_eq!(verification.domains, vec!["a.example", "b.example"]);
        assert!(verification.reasons.is_empty());
    }

    #[test]
    fn same_domain_does_not_count_twice() {
        let verification = verify(&[
            candidate(6, "23:30", "a.example"),
            candidate(6, "23:30", "a.example"),
            candidate(6, "23:30", "www.a.example"),
        ]);
        assert_eq!(verification.confidence, Confidence::Medium);
        assert_eq!(verification.domains, vec!["a.example"]);
    }

    #[test]
    fn single_source_is_medium() {
        let verification = verify(&[candidate(3, "22:00", "a.example")]);
        assert_eq!(verification.confidence, Confidence::Medium);
        assert_eq!(verification.status, ReviewStatus::Published);
        assert_eq!(verification.reasons, vec!["single_source"]);
    }

    #[test]
    fn conflicting_sources_go_to_review() {
        let verification = verify(&[
            candidate(6, "23:30", "a.example"),
            candidate(6, "23:30", "b.example"),
            candidate(7, "01:15", "c.example"),
        ]);
        assert_eq!(verification.status, ReviewStatus::Review);
        assert_eq!(verification.confidence, Confidence::Low);
        assert!(verification.resolved.is_none());
        assert!(verification.reasons[0].contains("conflict"));
        assert!(verification.reasons[0].contains("c.example"));
    }

    #[test]
    fn two_equally_strong_slots_conflict() {
        let verification = verify(&[
            candidate(6, "23:30", "a.example"),
            candidate(6, "23:30", "b.example"),
            candidate(7, "01:15", "c.example"),
            candidate(7, "01:15", "d.example"),
        ]);
        assert_eq!(verification.status, ReviewStatus::Review);
        assert!(verification.resolved.is_none());
    }

    #[test]
    fn duplicate_slot_from_same_domain_conflicts_with_other_slot() {
        // One source lists two different times (for example a rerun); without a
        // second independent source the service refuses to guess.
        let verification = verify(&[
            candidate(6, "23:30", "a.example"),
            candidate(7, "01:15", "a.example"),
        ]);
        assert_eq!(verification.status, ReviewStatus::Review);
        assert!(verification.resolved.is_none());
    }

    #[test]
    fn weekday_agreement_between_single_domains_still_conflicts() {
        let verification = verify(&[
            candidate(2, "22:00", "a.example"),
            candidate(3, "22:00", "b.example"),
        ]);
        assert_eq!(verification.status, ReviewStatus::Review);
    }

    #[test]
    fn no_time_goes_to_unknown() {
        let verification = verify(&[]);
        assert_eq!(verification.status, ReviewStatus::Unknown);
        assert_eq!(verification.confidence, Confidence::Unknown);
        assert_eq!(verification.reasons, vec!["no_time_found"]);
    }

    #[test]
    fn weekday_only_hint_is_reported() {
        let verification = verify(&[hint(5)]);
        assert_eq!(verification.status, ReviewStatus::Unknown);
        assert!(verification
            .reasons
            .iter()
            .any(|reason| reason.contains("weekday_hint")));
    }

    #[test]
    fn invalid_time_is_ignored() {
        let mut invalid = candidate(6, "25:99", "a.example");
        invalid.weekday = Some(6);
        let verification = verify(&[invalid]);
        assert_eq!(verification.status, ReviewStatus::Unknown);
    }

    #[test]
    fn unknown_domains_fall_back_to_provider_independence() {
        let mut first = candidate(7, "01:15", "");
        first.provider = "brave".to_string();
        let mut second = candidate(7, "01:15", "unknown");
        second.provider = "tavily".to_string();
        let verification = verify(&[first, second]);
        assert_eq!(verification.confidence, Confidence::High);
    }
}
