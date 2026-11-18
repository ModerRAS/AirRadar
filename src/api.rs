//! HTTP API (axum): health, schedule reads, status and admin refresh.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::{AppState, RefreshRequest, TriggerError};
use crate::log;
use crate::model::{RunTrigger, ScheduleFile, SCHEMA_VERSION};
use crate::season::SeasonRef;
use crate::util::iso;
use crate::VERSION;

/// Build the axum router for the given application state.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/schedule/latest", get(latest_schedule))
        .route("/api/schedule", get(schedule_by_season))
        .route("/api/status", get(status))
        .route("/admin/refresh", post(admin_refresh))
        .fallback(not_found)
        .with_state(state)
}

/// JSON error payload.
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }

    fn bad_request(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, code, message)
    }

    fn not_found(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::NOT_FOUND, code, message)
    }

    fn unauthorized(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    fn conflict(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::CONFLICT, code, message)
    }

    fn unavailable(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, code, message)
    }

    fn internal(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "error": self.code, "message": self.message })),
        )
            .into_response()
    }
}

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "service": "airradar",
        "version": VERSION,
        "schemaVersion": SCHEMA_VERSION,
        "uptimeSeconds": state.uptime_seconds(),
        "time": iso(&Utc::now()),
    }))
}

async fn latest_schedule(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ScheduleFile>, ApiError> {
    match state.store.load_latest_schedule() {
        Ok(Some(schedule)) => Ok(Json(schedule)),
        Ok(None) => Err(ApiError::not_found(
            "no_schedule_data",
            "no schedule has been published yet",
        )),
        Err(error) => {
            log::error(format!("latest schedule read failed: {error}"));
            Err(ApiError::internal("failed to read schedule data"))
        }
    }
}

#[derive(Debug, Deserialize)]
struct SeasonQuery {
    season: Option<String>,
}

async fn schedule_by_season(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SeasonQuery>,
) -> Result<Json<ScheduleFile>, ApiError> {
    let Some(season_text) = query.season else {
        return Err(ApiError::bad_request(
            "missing_season",
            "season query parameter is required, for example /api/schedule?season=2026-fall",
        ));
    };
    let season = SeasonRef::parse(&season_text).ok_or_else(|| {
        ApiError::bad_request(
            "invalid_season",
            format!("invalid season {season_text:?}; expected YYYY-season (for example 2026-fall)"),
        )
    })?;
    match state.store.load_schedule(&season) {
        Ok(Some(schedule)) => Ok(Json(schedule)),
        Ok(None) => Err(ApiError::not_found(
            "season_not_found",
            format!("no schedule published for {season}"),
        )),
        Err(error) => {
            log::error(format!("schedule read failed for {season}: {error}"));
            Err(ApiError::internal("failed to read schedule data"))
        }
    }
}

async fn status(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let last_run = state.last_run_summary();
    let latest = match state.store.load_latest_schedule() {
        Ok(latest) => latest,
        Err(error) => {
            log::warn(format!("status: latest schedule read failed: {error}"));
            None
        }
    };
    let data = match latest {
        Some(schedule) => json!({
            "latestSeason": schedule.season,
            "latestGeneratedAt": iso(&schedule.generated_at),
            "latestItemCount": schedule.item_count,
            "historyVersions": state.store.history_versions().len(),
        }),
        None => json!({
            "latestSeason": Value::Null,
            "latestGeneratedAt": Value::Null,
            "latestItemCount": 0,
            "historyVersions": state.store.history_versions().len(),
        }),
    };
    let cache = state.cache_stats();
    let search_order: Vec<&str> = state
        .cfg
        .search_order
        .iter()
        .map(|kind| kind.as_str())
        .collect();
    Ok(Json(json!({
        "service": "airradar",
        "version": VERSION,
        "schemaVersion": SCHEMA_VERSION,
        "uptimeSeconds": state.uptime_seconds(),
        "time": iso(&Utc::now()),
        "running": state.controller.is_running(),
        "refreshHours": state.cfg.refresh_hours,
        "seasonOverride": state.cfg.season_override.map(|season| season.label()),
        "nextScheduledAt": state.next_run_at().map(|next| iso(&next)),
        "data": data,
        "lastRun": last_run,
        "search": {
            "providers": state.search_providers,
            "configuredOrder": search_order,
        },
        "llm": { "configured": state.llm_configured },
        "cache": {
            "entries": cache.entries,
            "bytes": cache.bytes,
            "maxBytes": cache.max_bytes,
        },
        "limits": {
            "maxSubjectsPerRun": state.cfg.max_subjects_per_run,
            "maxLlmCalls": state.cfg.max_llm_calls,
            "fetchConcurrency": state.cfg.fetch_concurrency,
            "historyKeep": state.cfg.history_keep,
            "cacheMaxBytes": state.cfg.cache_max_bytes,
        },
    })))
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

async fn admin_refresh(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<SeasonQuery>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Some(expected) = state.cfg.admin_token.as_deref() else {
        return Err(ApiError::unavailable(
            "admin_disabled",
            "ADMIN_TOKEN is not configured on the server",
        ));
    };
    if expected.trim().is_empty() {
        return Err(ApiError::unavailable(
            "admin_disabled",
            "ADMIN_TOKEN is not configured on the server",
        ));
    }
    let provided = bearer_token(&headers)
        .ok_or_else(|| ApiError::unauthorized("missing Authorization: Bearer token"))?;
    if !constant_time_eq(provided.as_bytes(), expected.trim().as_bytes()) {
        return Err(ApiError::unauthorized("invalid admin token"));
    }
    let season = match query.season {
        Some(text) => Some(SeasonRef::parse(&text).ok_or_else(|| {
            ApiError::bad_request(
                "invalid_season",
                format!("invalid season {text:?}; expected YYYY-season"),
            )
        })?),
        None => state.cfg.season_override,
    };
    match state
        .controller
        .clone()
        .trigger(RefreshRequest::new(RunTrigger::Manual, season))
    {
        Ok(accepted) => Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "accepted": true,
                "runId": accepted.run_id,
                "season": accepted.season.map(|season| season.label()),
                "trigger": "manual",
            })),
        )),
        Err(TriggerError::Busy) => Err(ApiError::conflict(
            "refresh_in_progress",
            "a refresh is already running",
        )),
    }
}

async fn not_found() -> ApiError {
    ApiError::not_found("not_found", "route not found")
}
