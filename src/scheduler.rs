//! Interval scheduler: periodic refresh with jitter, plus an immediate
//! startup refresh when no data has ever been published.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::watch;

use crate::app::{AppState, RefreshRequest};
use crate::log;
use crate::model::RunTrigger;
use crate::util::{iso, random_f64};

/// Delay before the very first (or catch-up) refresh.
pub const STARTUP_REFRESH_DELAY_SECS: i64 = 10;

/// Minimum time before a catch-up run is scheduled.
pub const MIN_RESCHEDULE_DELAY_SECS: i64 = 5;

/// Compute the next run time.
///
/// interval_secs is REFRESH_HOURS * 3600. jitter is a multiplier in [-1, 1];
/// actual jitter applied is 10% of the interval.
pub fn next_run_at(
    now: DateTime<Utc>,
    last_run_finished: Option<DateTime<Utc>>,
    interval_secs: f64,
    has_data: bool,
    jitter: f64,
) -> DateTime<Utc> {
    if !has_data || interval_secs <= 0.0 {
        return now + chrono::Duration::seconds(STARTUP_REFRESH_DELAY_SECS);
    }
    let base = last_run_finished.unwrap_or(now);
    let jitter = jitter.clamp(-1.0, 1.0);
    let effective = (interval_secs * (1.0 + 0.1 * jitter)).max(1.0);
    let target = base + chrono::Duration::milliseconds((effective * 1000.0) as i64);
    if target <= now {
        now + chrono::Duration::seconds(MIN_RESCHEDULE_DELAY_SECS)
    } else {
        target
    }
}

/// Background scheduler loop. Exits when the shutdown watch flips to true.
pub async fn run(state: Arc<AppState>, mut shutdown: watch::Receiver<bool>) {
    loop {
        let now = Utc::now();
        let has_data = state.store.load_latest_schedule().ok().flatten().is_some();
        let last_finished = state.last_run_summary().map(|summary| summary.finished_at);
        let interval_secs = state.cfg.refresh_hours * 3600.0;
        let jitter = random_f64() * 2.0 - 1.0;
        let next = next_run_at(now, last_finished, interval_secs, has_data, jitter);
        state.set_next_run_at(Some(next));
        let wait = (next - now)
            .to_std()
            .unwrap_or(Duration::from_secs(1))
            .max(Duration::from_secs(1));
        log::info(format!(
            "next refresh scheduled at {} (in {}s)",
            iso(&next),
            wait.as_secs()
        ));
        tokio::select! {
            _ = tokio::time::sleep(wait) => {
                log::info("scheduled refresh starting");
                let summary = state
                    .controller
                    .clone()
                    .run_blocking(RefreshRequest::new(
                        RunTrigger::Schedule,
                        state.cfg.season_override,
                    ))
                    .await;
                match summary {
                    Some(summary) => log::info(format!(
                        "scheduled refresh done: ok={} published={}",
                        summary.ok, summary.items_published
                    )),
                    None => log::warn("scheduled refresh skipped: a run is already active"),
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    log::info("scheduler stopping");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_data_schedules_startup_refresh() {
        let now = Utc::now();
        let next = next_run_at(now, None, 6.0 * 3600.0, false, 0.0);
        assert_eq!(
            next.signed_duration_since(now).num_seconds(),
            STARTUP_REFRESH_DELAY_SECS
        );
    }

    #[test]
    fn next_run_respects_interval() {
        let now = Utc::now();
        let last = now - chrono::Duration::hours(1);
        let next = next_run_at(now, Some(last), 6.0 * 3600.0, true, 0.0);
        let delta = next.signed_duration_since(now).num_seconds();
        assert!(delta > 4 * 3600 && delta <= 5 * 3600 + 5, "delta={delta}");
    }

    #[test]
    fn overdue_run_schedules_soon() {
        let now = Utc::now();
        let last = now - chrono::Duration::hours(7);
        let next = next_run_at(now, Some(last), 6.0 * 3600.0, true, 0.0);
        let delta = next.signed_duration_since(now).num_seconds();
        assert!(
            delta >= MIN_RESCHEDULE_DELAY_SECS && delta <= 15,
            "delta={delta}"
        );
    }

    #[test]
    fn jitter_stays_within_ten_percent() {
        let now = Utc::now();
        let last = now;
        let interval = 6.0 * 3600.0;
        let early = next_run_at(now, Some(last), interval, true, -1.0)
            .signed_duration_since(now)
            .num_seconds();
        let late = next_run_at(now, Some(last), interval, true, 1.0)
            .signed_duration_since(now)
            .num_seconds();
        assert!(early >= (interval as i64) - (interval as i64 / 10) - 5);
        assert!(late <= (interval as i64) + (interval as i64 / 10) + 5);
        assert!(early < late);
    }
}
