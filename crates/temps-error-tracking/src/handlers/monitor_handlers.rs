//! Admin API for cron monitors (Sentry-compatible check-ins).

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use temps_auth::{permission_guard, project_scope_guard, RequireAuth};
use temps_core::problemdetails::{self, Problem, ProblemDetails};
use temps_entities::{monitor_check_ins, monitors};
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::services::monitor_service::{MonitorError, MonitorService, MonitorUpdate};

#[derive(OpenApi)]
#[openapi(
    paths(list_monitors, get_monitor, update_monitor, list_monitor_check_ins),
    components(schemas(
        MonitorResponse,
        MonitorListResponse,
        UpdateMonitorRequest,
        MonitorCheckInResponse,
        MonitorCheckInListResponse,
    )),
    tags((name = "monitors", description = "Cron monitor check-in management"))
)]
pub struct MonitorApiDoc;

#[derive(Clone)]
pub struct MonitorAppState {
    pub monitor_service: Arc<MonitorService>,
    pub audit_service: Arc<dyn temps_core::AuditLogger>,
}

pub fn configure_monitor_routes() -> Router<Arc<MonitorAppState>> {
    Router::new()
        .route("/projects/{project_id}/monitors", get(list_monitors))
        .route(
            "/projects/{project_id}/monitors/{monitor_id}",
            get(get_monitor).patch(update_monitor),
        )
        .route(
            "/projects/{project_id}/monitors/{monitor_id}/check-ins",
            get(list_monitor_check_ins),
        )
}

// ===== Pagination =====

#[derive(Debug, Deserialize, IntoParams)]
pub struct PaginationQuery {
    pub page: Option<u64>,
    pub page_size: Option<u64>,
}

// ===== Response/Request DTOs =====

#[derive(Debug, Serialize, ToSchema)]
pub struct MonitorResponse {
    pub id: i32,
    pub project_id: i32,
    pub environment_id: Option<i32>,
    pub slug: String,
    pub name: Option<String>,
    pub schedule: Option<String>,
    pub schedule_type: String,
    pub schedule_unit: Option<String>,
    pub checkin_margin_minutes: Option<i32>,
    pub max_runtime_minutes: Option<i32>,
    pub timezone: Option<String>,
    pub status: String,
    pub last_checkin_at: Option<String>,
    pub next_checkin_expected_at: Option<String>,
    pub checkin_retention_days: i32,
    pub muted: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl From<monitors::Model> for MonitorResponse {
    fn from(m: monitors::Model) -> Self {
        Self {
            id: m.id,
            project_id: m.project_id,
            environment_id: m.environment_id,
            slug: m.slug,
            name: m.name,
            schedule: m.schedule,
            schedule_type: m.schedule_type,
            schedule_unit: m.schedule_unit,
            checkin_margin_minutes: m.checkin_margin_minutes,
            max_runtime_minutes: m.max_runtime_minutes,
            timezone: m.timezone,
            status: m.status,
            last_checkin_at: m.last_checkin_at.map(|d| d.to_rfc3339()),
            next_checkin_expected_at: m.next_checkin_expected_at.map(|d| d.to_rfc3339()),
            checkin_retention_days: m.checkin_retention_days,
            muted: m.muted,
            created_at: m.created_at.to_rfc3339(),
            updated_at: m.updated_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MonitorListResponse {
    pub items: Vec<MonitorResponse>,
    pub total: u64,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateMonitorRequest {
    pub name: Option<String>,
    pub muted: Option<bool>,
    /// Per-monitor retention for check-in rows (minimum 1 day).
    pub checkin_retention_days: Option<i32>,
    /// Disable the monitor to suppress missed/timeout detection.
    pub disabled: Option<bool>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MonitorCheckInResponse {
    pub id: i64,
    pub monitor_id: i32,
    pub check_in_id: Option<String>,
    pub status: String,
    pub duration_ms: Option<i64>,
    pub environment: Option<String>,
    pub release: Option<String>,
    pub created_at: String,
}

impl From<monitor_check_ins::Model> for MonitorCheckInResponse {
    fn from(c: monitor_check_ins::Model) -> Self {
        Self {
            id: c.id,
            monitor_id: c.monitor_id,
            check_in_id: c.check_in_id,
            status: c.status,
            duration_ms: c.duration_ms,
            environment: c.environment,
            release: c.release,
            created_at: c.created_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MonitorCheckInListResponse {
    pub items: Vec<MonitorCheckInResponse>,
    pub total: u64,
}

// ===== Handlers =====

#[utoipa::path(
    get,
    path = "/projects/{project_id}/monitors",
    params(("project_id" = i32, Path, description = "Project ID"), PaginationQuery),
    responses(
        (status = 200, description = "List of monitors", body = MonitorListResponse),
        (status = 401, description = "Unauthorized", body = ProblemDetails),
        (status = 403, description = "Insufficient permissions", body = ProblemDetails),
    ),
    security(("bearer_auth" = [])),
    tag = "monitors"
)]
pub async fn list_monitors(
    State(state): State<Arc<MonitorAppState>>,
    RequireAuth(auth): RequireAuth,
    Path(project_id): Path<i32>,
    Query(pagination): Query<PaginationQuery>,
) -> Result<Json<MonitorListResponse>, Problem> {
    permission_guard!(auth, ErrorTrackingRead);
    project_scope_guard!(auth, project_id);

    let (items, total) = state
        .monitor_service
        .list_monitors(project_id, pagination.page, pagination.page_size)
        .await?;

    Ok(Json(MonitorListResponse {
        items: items.into_iter().map(MonitorResponse::from).collect(),
        total,
    }))
}

#[utoipa::path(
    get,
    path = "/projects/{project_id}/monitors/{monitor_id}",
    params(
        ("project_id" = i32, Path, description = "Project ID"),
        ("monitor_id" = i32, Path, description = "Monitor ID")
    ),
    responses(
        (status = 200, description = "Monitor", body = MonitorResponse),
        (status = 404, description = "Not found", body = ProblemDetails),
    ),
    security(("bearer_auth" = [])),
    tag = "monitors"
)]
pub async fn get_monitor(
    State(state): State<Arc<MonitorAppState>>,
    RequireAuth(auth): RequireAuth,
    Path((project_id, monitor_id)): Path<(i32, i32)>,
) -> Result<Json<MonitorResponse>, Problem> {
    permission_guard!(auth, ErrorTrackingRead);
    project_scope_guard!(auth, project_id);

    let monitor = state
        .monitor_service
        .get_monitor(project_id, monitor_id)
        .await?;
    Ok(Json(MonitorResponse::from(monitor)))
}

#[utoipa::path(
    patch,
    path = "/projects/{project_id}/monitors/{monitor_id}",
    params(
        ("project_id" = i32, Path, description = "Project ID"),
        ("monitor_id" = i32, Path, description = "Monitor ID")
    ),
    request_body = UpdateMonitorRequest,
    responses(
        (status = 200, description = "Updated monitor", body = MonitorResponse),
        (status = 404, description = "Not found", body = ProblemDetails),
    ),
    security(("bearer_auth" = [])),
    tag = "monitors"
)]
pub async fn update_monitor(
    State(state): State<Arc<MonitorAppState>>,
    RequireAuth(auth): RequireAuth,
    Path((project_id, monitor_id)): Path<(i32, i32)>,
    Json(req): Json<UpdateMonitorRequest>,
) -> Result<Json<MonitorResponse>, Problem> {
    permission_guard!(auth, ErrorTrackingWrite);
    project_scope_guard!(auth, project_id);

    let monitor = state
        .monitor_service
        .update_monitor(
            project_id,
            monitor_id,
            MonitorUpdate {
                name: req.name,
                muted: req.muted,
                checkin_retention_days: req.checkin_retention_days,
                disabled: req.disabled,
            },
        )
        .await?;

    Ok(Json(MonitorResponse::from(monitor)))
}

#[utoipa::path(
    get,
    path = "/projects/{project_id}/monitors/{monitor_id}/check-ins",
    params(
        ("project_id" = i32, Path, description = "Project ID"),
        ("monitor_id" = i32, Path, description = "Monitor ID"),
        PaginationQuery
    ),
    responses(
        (status = 200, description = "List of check-ins", body = MonitorCheckInListResponse),
        (status = 404, description = "Not found", body = ProblemDetails),
    ),
    security(("bearer_auth" = [])),
    tag = "monitors"
)]
pub async fn list_monitor_check_ins(
    State(state): State<Arc<MonitorAppState>>,
    RequireAuth(auth): RequireAuth,
    Path((project_id, monitor_id)): Path<(i32, i32)>,
    Query(pagination): Query<PaginationQuery>,
) -> Result<Json<MonitorCheckInListResponse>, Problem> {
    permission_guard!(auth, ErrorTrackingRead);
    project_scope_guard!(auth, project_id);

    let (items, total) = state
        .monitor_service
        .list_check_ins(
            project_id,
            monitor_id,
            pagination.page,
            pagination.page_size,
        )
        .await?;

    Ok(Json(MonitorCheckInListResponse {
        items: items
            .into_iter()
            .map(MonitorCheckInResponse::from)
            .collect(),
        total,
    }))
}

// ===== Error mapping =====

impl From<MonitorError> for Problem {
    fn from(error: MonitorError) -> Self {
        match error {
            MonitorError::NotFound { .. } => problemdetails::new(StatusCode::NOT_FOUND)
                .with_title("Monitor Not Found")
                .with_detail(error.to_string()),
            MonitorError::MissingSlug { .. }
            | MonitorError::InvalidStatus { .. }
            | MonitorError::InvalidSchedule { .. } => problemdetails::new(StatusCode::BAD_REQUEST)
                .with_title("Invalid Monitor Check-In")
                .with_detail(error.to_string()),
            MonitorError::Database(_) => problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                .with_title("Internal Server Error")
                .with_detail(error.to_string()),
        }
    }
}
