//! HTTP API integration tests (axum + tower oneshot, no network).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use airradar::app::{
    AppState, BoxFuture, RefreshController, RefreshRequest, TriggerAccepted, TriggerError,
};
use airradar::cache::Cache;
use airradar::config::Config;
use airradar::model::{RunSummary, ScheduleFile};
use airradar::store::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::Utc;
use serde_json::{json, Value};
use tower::ServiceExt;

struct MockController {
    busy: AtomicBool,
    triggers: AtomicUsize,
}

impl MockController {
    fn new() -> Arc<MockController> {
        Arc::new(MockController {
            busy: AtomicBool::new(false),
            triggers: AtomicUsize::new(0),
        })
    }
}

impl RefreshController for MockController {
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
        self.triggers.fetch_add(1, Ordering::SeqCst);
        Ok(TriggerAccepted {
            run_id: "mock-run".to_string(),
            season: request.season,
        })
    }

    fn run_blocking(
        self: Arc<Self>,
        _request: RefreshRequest,
    ) -> BoxFuture<'static, Option<RunSummary>> {
        Box::pin(async move { None })
    }
}

fn state_with(dir: &std::path::Path, controller: Arc<MockController>) -> Arc<AppState> {
    let cfg = Config::for_tests(dir.to_path_buf());
    let store = Arc::new(Store::new(dir.to_path_buf()));
    let cache = Arc::new(Cache::new(dir.join("cache"), 1024 * 1024, None));
    Arc::new(AppState::new(Arc::new(cfg), store, cache, controller))
}

fn sample_schedule() -> ScheduleFile {
    serde_json::from_value(json!({
        "schemaVersion": 1,
        "generatedAt": "2026-11-18T05:00:00Z",
        "season": "2026-fall",
        "seasonStart": "2026-10-01",
        "seasonEnd": "2026-12-31",
        "timezone": "Asia/Tokyo",
        "itemCount": 1,
        "items": [{
            "subjectId": 393010,
            "name": "探偵はもう、死んでいる。 Season 2",
            "nameCn": "侦探已经死了。 第二季",
            "airDate": "2026-10-07",
            "weekday": 3,
            "time": "23:00",
            "timezone": "Asia/Tokyo",
            "confidence": "high",
            "sources": [{
                "url": "https://example.com/schedule",
                "domain": "example.com",
                "provider": "brave"
            }],
            "updatedAt": "2026-11-18T04:00:00Z"
        }]
    }))
    .expect("valid sample schedule")
}

async fn call(router: Router, method: &str, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = builder.body(Body::empty()).expect("valid request");
    let response = router.oneshot(request).await.expect("router call");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body");
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn health_reports_version_and_uptime() {
    let dir = tempfile::tempdir().unwrap();
    let router = airradar::api::router(state_with(dir.path(), MockController::new()));
    let (status, body) = call(router, "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "airradar");
    assert_eq!(body["version"], airradar::VERSION);
    assert!(body["uptimeSeconds"].is_number());
    assert_eq!(body["schemaVersion"], 1);
}

#[tokio::test]
async fn latest_is_404_before_first_publish() {
    let dir = tempfile::tempdir().unwrap();
    let router = airradar::api::router(state_with(dir.path(), MockController::new()));
    let (status, body) = call(router, "GET", "/api/schedule/latest", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no_schedule_data");
}

#[tokio::test]
async fn latest_returns_published_schedule() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), MockController::new());
    state.store.publish(&sample_schedule(), 5).unwrap();
    let router = airradar::api::router(state);
    let (status, body) = call(router, "GET", "/api/schedule/latest", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["schemaVersion"], 1);
    assert_eq!(body["season"], "2026-fall");
    assert_eq!(body["itemCount"], 1);
    assert_eq!(body["items"][0]["weekday"], 3);
    assert_eq!(body["items"][0]["time"], "23:00");
    // Old payloads without timeKnown are treated as timed rows.
    assert_eq!(body["items"][0]["timeKnown"], true);
    assert_eq!(body["items"][0]["disputed"], false);
    assert_eq!(body["items"][0]["timezone"], "Asia/Tokyo");
    assert_eq!(body["items"][0]["confidence"], "high");
}

#[tokio::test]
async fn disputed_rows_serialise_flag_and_low_confidence() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), MockController::new());
    let mut schedule = sample_schedule();
    schedule.items[0].disputed = true;
    schedule.items[0].confidence = airradar::model::Confidence::Low;
    state.store.publish(&schedule, 5).unwrap();
    let router = airradar::api::router(state);
    let (status, body) = call(router, "GET", "/api/schedule/latest", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"][0]["disputed"], true);
    assert_eq!(body["items"][0]["confidence"], "low");
}

#[tokio::test]
async fn weekday_only_rows_serialise_null_time_and_time_known_false() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), MockController::new());
    let mut schedule = sample_schedule();
    schedule.items[0].time = None;
    schedule.items[0].time_known = false;
    schedule.items[0].confidence = airradar::model::Confidence::Unknown;
    state.store.publish(&schedule, 5).unwrap();
    let router = airradar::api::router(state);
    let (status, body) = call(router, "GET", "/api/schedule/latest", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["items"][0]["time"].is_null());
    assert_eq!(body["items"][0]["timeKnown"], false);
    assert_eq!(body["items"][0]["confidence"], "unknown");
}

#[tokio::test]
async fn schedule_by_season_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), MockController::new());
    state.store.publish(&sample_schedule(), 5).unwrap();
    let router = airradar::api::router(state);

    let (status, body) = call(
        router.clone(),
        "GET",
        "/api/schedule?season=2026-fall",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["season"], "2026-fall");

    let (status, body) = call(router.clone(), "GET", "/api/schedule", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "missing_season");

    let (status, body) = call(router.clone(), "GET", "/api/schedule?season=2026-Q4", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_season");

    let (status, body) = call(router, "GET", "/api/schedule?season=2020-winter", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "season_not_found");
}

#[tokio::test]
async fn status_reports_data_and_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), MockController::new());
    state.store.publish(&sample_schedule(), 5).unwrap();
    let router = airradar::api::router(state);
    let (status, body) = call(router, "GET", "/api/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["service"], "airradar");
    assert_eq!(body["running"], false);
    assert_eq!(body["data"]["latestSeason"], "2026-fall");
    assert_eq!(body["data"]["latestItemCount"], 1);
    assert_eq!(body["data"]["historyVersions"], 1);
    assert_eq!(body["llm"]["configured"], false);
    assert!(body["lastRun"].is_null());
    assert_eq!(body["cache"]["maxBytes"], 1024 * 1024);
    assert!(body["nextScheduledAt"].is_null());
    assert_eq!(body["limits"]["maxSubjectsPerRun"], 2);
}

#[tokio::test]
async fn admin_refresh_requires_token_and_handles_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let controller = MockController::new();
    let state = state_with(dir.path(), controller.clone());
    let router = airradar::api::router(state);

    let (status, body) = call(router.clone(), "POST", "/admin/refresh", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "unauthorized");

    let (status, body) = call(router.clone(), "POST", "/admin/refresh", Some("wrong")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "unauthorized");

    let (status, body) = call(
        router.clone(),
        "POST",
        "/admin/refresh?season=not-a-season",
        Some("test-token"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_season");

    let (status, body) = call(
        router.clone(),
        "POST",
        "/admin/refresh?season=2026-fall",
        Some("test-token"),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["accepted"], true);
    assert_eq!(body["runId"], "mock-run");
    assert_eq!(body["season"], "2026-fall");
    assert_eq!(body["trigger"], "manual");
    assert_eq!(controller.triggers.load(Ordering::SeqCst), 1);

    let (status, body) = call(router.clone(), "POST", "/admin/refresh", Some("test-token")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "refresh_in_progress");

    let (status, body) = call(router, "GET", "/api/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["running"], true);
}

#[tokio::test]
async fn admin_is_disabled_without_token() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests(dir.path().to_path_buf());
    cfg.admin_token = None;
    let store = Arc::new(Store::new(dir.path().to_path_buf()));
    let cache = Arc::new(Cache::new(dir.path().join("cache"), 1024, None));
    let state = Arc::new(AppState::new(
        Arc::new(cfg),
        store,
        cache,
        MockController::new(),
    ));
    let router = airradar::api::router(state);
    let (status, body) = call(router, "POST", "/admin/refresh", Some("anything")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "admin_disabled");
}

#[tokio::test]
async fn unknown_route_returns_404_json() {
    let dir = tempfile::tempdir().unwrap();
    let router = airradar::api::router(state_with(dir.path(), MockController::new()));
    let (status, body) = call(router, "GET", "/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "not_found");
}

#[tokio::test]
async fn review_state_last_run_is_surfaced() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), MockController::new());
    let mut status_file = airradar::model::StatusFile::empty();
    status_file.updated_at = Utc::now();
    state.store.save_status(&status_file).unwrap();
    let router = airradar::api::router(state.clone());
    let (status, body) = call(router, "GET", "/api/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["lastRun"].is_null());

    let mut status_file = airradar::model::StatusFile::empty();
    status_file.last_run = Some(
        serde_json::from_value(json!({
            "runId": "20261118T050000Z-deadbeef",
            "trigger": "manual",
            "season": "2026-fall",
            "startedAt": "2026-11-18T05:00:00Z",
            "finishedAt": "2026-11-18T05:00:10Z",
            "durationMs": 10000,
            "ok": true,
            "subjectListSource": "bangumi-search",
            "subjectsFetched": 10,
            "subjectsProcessed": 5,
            "itemsPublished": 3,
            "highConfidence": 1,
            "mediumConfidence": 2,
            "reviewCount": 1,
            "unknownCount": 1,
            "search": [],
            "llm": {"configured": false, "calls": 0, "cachedHits": 0, "results": 0, "failures": 0},
            "fetch": {"requested": 2, "fetched": 2, "fromCache": 0, "failed": 0}
        }))
        .expect("valid run summary"),
    );
    status_file.updated_at = Utc::now();
    state.store.save_status(&status_file).unwrap();
    let router = airradar::api::router(state.clone());
    let (status, body) = call(router, "GET", "/api/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["lastRun"]["runId"], "20261118T050000Z-deadbeef");
    assert_eq!(body["lastRun"]["itemsPublished"], 3);
}
