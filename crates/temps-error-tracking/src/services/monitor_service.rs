//! Sentry-compatible cron monitor service.
//!
//! Records check-ins reported by deployed applications, auto-creating monitors on
//! first sight, and detects monitors that fail to check in on schedule (`missed`)
//! or run longer than their configured `max_runtime` (`timeout`).
//!
//! ## Lifecycle of a run
//!
//! A run reports either a single terminal check-in (`ok`/`error`) or a pair:
//! `in_progress` at start and `ok`/`error` at finish, correlated by `check_in_id`.
//! When the terminal report arrives we update the existing `in_progress` row in
//! place (computing duration from the elapsed time when not supplied), keeping one
//! row per run.

use std::str::FromStr;
use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures::future::BoxFuture;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, Set,
};
use thiserror::Error;

use crate::sentry::envelope::CheckIn;
use temps_entities::{monitor_check_ins, monitors};

/// Default grace period (minutes) applied when a monitor does not specify its own
/// `checkin_margin`. Keeps the detector from flapping at the exact expected minute.
const DEFAULT_CHECKIN_MARGIN_MINUTES: i64 = 1;

/// Default per-monitor retention for check-in rows.
const DEFAULT_RETENTION_DAYS: i32 = 30;

#[derive(Error, Debug)]
pub enum MonitorError {
    #[error("Check-in for project {project_id} is missing a monitor_slug")]
    MissingSlug { project_id: i32 },

    #[error("Invalid check-in status '{status}' for monitor '{slug}' (project {project_id})")]
    InvalidStatus {
        project_id: i32,
        slug: String,
        status: String,
    },

    #[error("Invalid schedule '{schedule}' for monitor '{slug}': {reason}")]
    InvalidSchedule {
        slug: String,
        schedule: String,
        reason: String,
    },

    #[error("Monitor {monitor_id} not found in project {project_id}")]
    NotFound { monitor_id: i32, project_id: i32 },

    #[error("Database error: {0}")]
    Database(#[from] sea_orm::DbErr),
}

/// Notification payload emitted when a monitor's health changes for the worse.
#[derive(Clone, Debug)]
pub struct MonitorAlert {
    pub monitor_id: i32,
    pub project_id: i32,
    pub environment_id: Option<i32>,
    pub slug: String,
    pub name: Option<String>,
    /// `error`, `missed`, or `timeout`.
    pub status: String,
    pub message: String,
}

/// Callback used to forward [`MonitorAlert`]s to the notification system.
pub type MonitorNotificationCallback =
    Arc<dyn Fn(MonitorAlert) -> BoxFuture<'static, ()> + Send + Sync>;

/// Admin-editable monitor fields. Each `None` field is left unchanged.
#[derive(Clone, Debug, Default)]
pub struct MonitorUpdate {
    pub name: Option<String>,
    pub muted: Option<bool>,
    pub checkin_retention_days: Option<i32>,
    /// `Some(true)` disables the monitor (suppresses detection); `Some(false)`
    /// re-activates it.
    pub disabled: Option<bool>,
}

/// Normalized terminal/transient status of a check-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckInStatus {
    InProgress,
    Ok,
    Error,
}

impl CheckInStatus {
    fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "in_progress" => Some(Self::InProgress),
            "ok" => Some(Self::Ok),
            "error" | "failed" => Some(Self::Error),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }

    fn is_terminal(&self) -> bool {
        !matches!(self, Self::InProgress)
    }
}

pub struct MonitorService {
    db: Arc<DatabaseConnection>,
    notification_callback: tokio::sync::OnceCell<MonitorNotificationCallback>,
}

impl MonitorService {
    pub fn new(db: Arc<DatabaseConnection>) -> Self {
        Self {
            db,
            notification_callback: tokio::sync::OnceCell::new(),
        }
    }

    /// Wire up the notification callback (idempotent — first call wins).
    pub fn set_notification_callback(&self, callback: MonitorNotificationCallback) {
        let _ = self.notification_callback.set(callback);
    }

    async fn notify(&self, alert: MonitorAlert) {
        if let Some(callback) = self.notification_callback.get() {
            callback(alert).await;
        }
    }

    // ===================== Ingestion =====================

    /// Record a single check-in, upserting its monitor.
    pub async fn record_check_in(
        &self,
        project_id: i32,
        environment_id: Option<i32>,
        check_in: &CheckIn,
    ) -> Result<(), MonitorError> {
        let slug = check_in
            .monitor_slug
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or(MonitorError::MissingSlug { project_id })?
            .to_string();

        let status =
            CheckInStatus::parse(&check_in.status).ok_or_else(|| MonitorError::InvalidStatus {
                project_id,
                slug: slug.clone(),
                status: check_in.status.clone(),
            })?;

        let now = Utc::now();

        // Upsert the monitor row.
        let monitor = self
            .upsert_monitor(project_id, environment_id, &slug, check_in, status, now)
            .await?;

        // Persist the check-in row (updating an in_progress row in place on finish).
        let duration_ms = check_in
            .duration
            .map(|secs| (secs * 1000.0).round() as i64)
            .filter(|ms| *ms >= 0);

        self.persist_check_in(&monitor, check_in, status, duration_ms, now)
            .await?;

        // A failed run is the headline signal — alert immediately (unless muted).
        if status == CheckInStatus::Error && !monitor.muted {
            self.notify(MonitorAlert {
                monitor_id: monitor.id,
                project_id: monitor.project_id,
                environment_id: monitor.environment_id,
                slug: monitor.slug.clone(),
                name: monitor.name.clone(),
                status: "error".to_string(),
                message: format!(
                    "Monitor '{}' reported a failed check-in",
                    monitor.name.as_deref().unwrap_or(&monitor.slug)
                ),
            })
            .await;
        }

        Ok(())
    }

    async fn upsert_monitor(
        &self,
        project_id: i32,
        environment_id: Option<i32>,
        slug: &str,
        check_in: &CheckIn,
        status: CheckInStatus,
        now: DateTime<Utc>,
    ) -> Result<monitors::Model, MonitorError> {
        let existing = monitors::Entity::find()
            .filter(monitors::Column::ProjectId.eq(project_id))
            .filter(monitors::Column::Slug.eq(slug))
            .one(self.db.as_ref())
            .await?;

        // Health status: terminal check-ins set ok/error; in_progress never
        // downgrades a known-good/bad status.
        let health = match status {
            CheckInStatus::Ok => Some("ok".to_string()),
            CheckInStatus::Error => Some("error".to_string()),
            CheckInStatus::InProgress => None,
        };

        match existing {
            Some(model) => {
                let mut active: monitors::ActiveModel = model.clone().into();
                active.last_checkin_at = Set(Some(now));
                if let Some(env_id) = environment_id {
                    active.environment_id = Set(Some(env_id));
                }
                if let Some(h) = health {
                    active.status = Set(h);
                }
                self.apply_monitor_config(&mut active, check_in);

                // Recompute the next expected check-in from the (possibly updated) schedule.
                let schedule = active_str(&active.schedule, &model.schedule);
                let schedule_type = active_string(&active.schedule_type, &model.schedule_type);
                let schedule_unit = active_str(&active.schedule_unit, &model.schedule_unit);
                active.next_checkin_expected_at = Set(self.compute_next_expected(
                    &schedule_type,
                    schedule.as_deref(),
                    schedule_unit.as_deref(),
                    now,
                    slug,
                )?);
                active.updated_at = Set(now);

                Ok(active.update(self.db.as_ref()).await?)
            }
            None => {
                let mut active = monitors::ActiveModel {
                    project_id: Set(project_id),
                    environment_id: Set(environment_id),
                    slug: Set(slug.to_string()),
                    name: Set(Some(slug.to_string())),
                    schedule: Set(None),
                    schedule_type: Set("crontab".to_string()),
                    schedule_unit: Set(None),
                    checkin_margin_minutes: Set(None),
                    max_runtime_minutes: Set(None),
                    timezone: Set(None),
                    status: Set(health.unwrap_or_else(|| "active".to_string())),
                    last_checkin_at: Set(Some(now)),
                    next_checkin_expected_at: Set(None),
                    checkin_retention_days: Set(DEFAULT_RETENTION_DAYS),
                    muted: Set(false),
                    created_at: Set(now),
                    updated_at: Set(now),
                    ..Default::default()
                };
                self.apply_monitor_config(&mut active, check_in);

                let schedule = active_str(&active.schedule, &None);
                let schedule_type = active_string(&active.schedule_type, "crontab");
                let schedule_unit = active_str(&active.schedule_unit, &None);
                active.next_checkin_expected_at = Set(self.compute_next_expected(
                    &schedule_type,
                    schedule.as_deref(),
                    schedule_unit.as_deref(),
                    now,
                    slug,
                )?);

                Ok(active.insert(self.db.as_ref()).await?)
            }
        }
    }

    /// Apply a check-in's `monitor_config` to a monitor ActiveModel, if present.
    fn apply_monitor_config(&self, active: &mut monitors::ActiveModel, check_in: &CheckIn) {
        let Some(cfg) = &check_in.monitor_config else {
            return;
        };

        if let Some(schedule) = &cfg.schedule {
            match schedule.schedule_type.as_str() {
                "interval" => {
                    active.schedule_type = Set("interval".to_string());
                    // For interval schedules `value` is a number of units.
                    let count = schedule
                        .value
                        .as_i64()
                        .or_else(|| schedule.value.as_str().and_then(|s| s.parse::<i64>().ok()));
                    active.schedule = Set(count.map(|c| c.to_string()));
                    active.schedule_unit = Set(schedule.unit.clone());
                }
                _ => {
                    // Default/crontab: `value` is a cron expression string.
                    active.schedule_type = Set("crontab".to_string());
                    active.schedule = Set(schedule
                        .value
                        .as_str()
                        .map(|s| s.to_string())
                        .or_else(|| Some(schedule.value.to_string())));
                    active.schedule_unit = Set(None);
                }
            }
        }

        if let Some(margin) = cfg.checkin_margin {
            active.checkin_margin_minutes = Set(Some(margin as i32));
        }
        if let Some(max_runtime) = cfg.max_runtime {
            active.max_runtime_minutes = Set(Some(max_runtime as i32));
        }
        if let Some(tz) = &cfg.timezone {
            active.timezone = Set(Some(tz.clone()));
        }
    }

    async fn persist_check_in(
        &self,
        monitor: &monitors::Model,
        check_in: &CheckIn,
        status: CheckInStatus,
        duration_ms: Option<i64>,
        now: DateTime<Utc>,
    ) -> Result<(), MonitorError> {
        let environment = check_in.environment.clone();
        let release = check_in.release.clone();

        // On a terminal report with a known check_in_id, resolve the matching
        // in_progress row in place (one row per run) instead of inserting a new one.
        if status.is_terminal() {
            if let Some(cid) = check_in.check_in_id.as_deref().filter(|c| !c.is_empty()) {
                if let Some(open) = monitor_check_ins::Entity::find()
                    .filter(monitor_check_ins::Column::MonitorId.eq(monitor.id))
                    .filter(monitor_check_ins::Column::CheckInId.eq(cid))
                    .filter(monitor_check_ins::Column::Status.eq("in_progress"))
                    .order_by_desc(monitor_check_ins::Column::CreatedAt)
                    .one(self.db.as_ref())
                    .await?
                {
                    let started = open.created_at;
                    let mut active: monitor_check_ins::ActiveModel = open.into();
                    active.status = Set(status.as_str().to_string());
                    let resolved_duration = duration_ms
                        .or_else(|| (now - started).num_milliseconds().into())
                        .filter(|ms| *ms >= 0);
                    active.duration_ms = Set(resolved_duration);
                    if environment.is_some() {
                        active.environment = Set(environment);
                    }
                    if release.is_some() {
                        active.release = Set(release);
                    }
                    active.update(self.db.as_ref()).await?;
                    return Ok(());
                }
            }
        }

        let row = monitor_check_ins::ActiveModel {
            monitor_id: Set(monitor.id),
            check_in_id: Set(check_in.check_in_id.clone()),
            status: Set(status.as_str().to_string()),
            duration_ms: Set(duration_ms),
            environment: Set(environment),
            release: Set(release),
            created_at: Set(now),
            ..Default::default()
        };
        row.insert(self.db.as_ref()).await?;
        Ok(())
    }

    // ===================== Detection =====================

    /// Detect monitors that missed their expected check-in or whose in-progress run
    /// exceeded `max_runtime`. Intended to be called once per minute. Returns the
    /// number of monitors transitioned to an unhealthy state.
    pub async fn detect_unhealthy(&self) -> Result<usize, MonitorError> {
        let now = Utc::now();
        let mut transitioned = 0;

        let candidates = monitors::Entity::find()
            .filter(monitors::Column::Status.ne("disabled"))
            .all(self.db.as_ref())
            .await?;

        for monitor in candidates {
            // ---- Missed check-in ----
            if let Some(expected) = monitor.next_checkin_expected_at {
                let margin = monitor
                    .checkin_margin_minutes
                    .map(|m| m as i64)
                    .unwrap_or(DEFAULT_CHECKIN_MARGIN_MINUTES);
                let deadline = expected + ChronoDuration::minutes(margin);

                if now > deadline {
                    let first_time = monitor.status != "missed";
                    if first_time && !monitor.muted {
                        self.insert_synthetic_check_in(monitor.id, "missed", now)
                            .await?;
                        self.notify(MonitorAlert {
                            monitor_id: monitor.id,
                            project_id: monitor.project_id,
                            environment_id: monitor.environment_id,
                            slug: monitor.slug.clone(),
                            name: monitor.name.clone(),
                            status: "missed".to_string(),
                            message: format!(
                                "Monitor '{}' missed its expected check-in at {}",
                                monitor.name.as_deref().unwrap_or(&monitor.slug),
                                expected.to_rfc3339()
                            ),
                        })
                        .await;
                        transitioned += 1;
                    }

                    // Advance the expectation to the next occurrence so we don't
                    // re-alert every minute.
                    let next = self.compute_next_expected(
                        &monitor.schedule_type,
                        monitor.schedule.as_deref(),
                        monitor.schedule_unit.as_deref(),
                        now,
                        &monitor.slug,
                    )?;
                    let mut active: monitors::ActiveModel = monitor.clone().into();
                    active.status = Set("missed".to_string());
                    active.next_checkin_expected_at = Set(next);
                    active.updated_at = Set(now);
                    active.update(self.db.as_ref()).await?;
                    continue;
                }
            }

            // ---- Overrun / timeout ----
            if let Some(max_runtime) = monitor.max_runtime_minutes {
                let threshold = now - ChronoDuration::minutes(max_runtime as i64);
                let stale = monitor_check_ins::Entity::find()
                    .filter(monitor_check_ins::Column::MonitorId.eq(monitor.id))
                    .filter(monitor_check_ins::Column::Status.eq("in_progress"))
                    .filter(monitor_check_ins::Column::CreatedAt.lt(threshold))
                    .all(self.db.as_ref())
                    .await?;

                if !stale.is_empty() {
                    for open in stale {
                        let started = open.created_at;
                        let mut active: monitor_check_ins::ActiveModel = open.into();
                        active.status = Set("timeout".to_string());
                        active.duration_ms = Set((now - started).num_milliseconds().into());
                        active.update(self.db.as_ref()).await?;
                    }

                    if monitor.status != "timeout" && !monitor.muted {
                        self.notify(MonitorAlert {
                            monitor_id: monitor.id,
                            project_id: monitor.project_id,
                            environment_id: monitor.environment_id,
                            slug: monitor.slug.clone(),
                            name: monitor.name.clone(),
                            status: "timeout".to_string(),
                            message: format!(
                                "Monitor '{}' exceeded its maximum runtime of {} minute(s)",
                                monitor.name.as_deref().unwrap_or(&monitor.slug),
                                max_runtime
                            ),
                        })
                        .await;
                        transitioned += 1;
                    }

                    let mut active: monitors::ActiveModel = monitor.clone().into();
                    active.status = Set("timeout".to_string());
                    active.updated_at = Set(now);
                    active.update(self.db.as_ref()).await?;
                }
            }
        }

        Ok(transitioned)
    }

    async fn insert_synthetic_check_in(
        &self,
        monitor_id: i32,
        status: &str,
        now: DateTime<Utc>,
    ) -> Result<(), MonitorError> {
        let row = monitor_check_ins::ActiveModel {
            monitor_id: Set(monitor_id),
            check_in_id: Set(None),
            status: Set(status.to_string()),
            duration_ms: Set(None),
            environment: Set(None),
            release: Set(None),
            created_at: Set(now),
            ..Default::default()
        };
        row.insert(self.db.as_ref()).await?;
        Ok(())
    }

    // ===================== Retention =====================

    /// Delete check-in rows older than each monitor's `checkin_retention_days`.
    /// Returns the total number of rows deleted. Intended to run daily; the
    /// table-level TimescaleDB retention policy is a coarser backstop.
    pub async fn cleanup_expired_check_ins(&self) -> Result<u64, MonitorError> {
        let now = Utc::now();
        let mut deleted = 0u64;

        let all = monitors::Entity::find().all(self.db.as_ref()).await?;
        for monitor in all {
            let retention = monitor.checkin_retention_days.max(1) as i64;
            let cutoff = now - ChronoDuration::days(retention);

            let res = monitor_check_ins::Entity::delete_many()
                .filter(monitor_check_ins::Column::MonitorId.eq(monitor.id))
                .filter(monitor_check_ins::Column::CreatedAt.lt(cutoff))
                .exec(self.db.as_ref())
                .await?;
            deleted += res.rows_affected;
        }

        Ok(deleted)
    }

    // ===================== Read API =====================

    pub async fn list_monitors(
        &self,
        project_id: i32,
        page: Option<u64>,
        page_size: Option<u64>,
    ) -> Result<(Vec<monitors::Model>, u64), MonitorError> {
        let page = page.unwrap_or(1).max(1);
        let page_size = page_size.unwrap_or(20).clamp(1, 100);

        let paginator = monitors::Entity::find()
            .filter(monitors::Column::ProjectId.eq(project_id))
            .order_by_desc(monitors::Column::LastCheckinAt)
            .paginate(self.db.as_ref(), page_size);

        let total = paginator.num_items().await?;
        let items = paginator.fetch_page(page - 1).await?;
        Ok((items, total))
    }

    pub async fn get_monitor(
        &self,
        project_id: i32,
        monitor_id: i32,
    ) -> Result<monitors::Model, MonitorError> {
        monitors::Entity::find_by_id(monitor_id)
            .filter(monitors::Column::ProjectId.eq(project_id))
            .one(self.db.as_ref())
            .await?
            .ok_or(MonitorError::NotFound {
                monitor_id,
                project_id,
            })
    }

    /// Apply admin-editable fields to a monitor. Only `Some(_)` fields are changed.
    pub async fn update_monitor(
        &self,
        project_id: i32,
        monitor_id: i32,
        update: MonitorUpdate,
    ) -> Result<monitors::Model, MonitorError> {
        let monitor = self.get_monitor(project_id, monitor_id).await?;
        let mut active: monitors::ActiveModel = monitor.into();

        if let Some(name) = update.name {
            active.name = Set(Some(name));
        }
        if let Some(muted) = update.muted {
            active.muted = Set(muted);
        }
        if let Some(days) = update.checkin_retention_days {
            active.checkin_retention_days = Set(days.max(1));
        }
        if let Some(disabled) = update.disabled {
            active.status = Set(if disabled { "disabled" } else { "active" }.to_string());
        }
        active.updated_at = Set(Utc::now());

        Ok(active.update(self.db.as_ref()).await?)
    }

    pub async fn list_check_ins(
        &self,
        project_id: i32,
        monitor_id: i32,
        page: Option<u64>,
        page_size: Option<u64>,
    ) -> Result<(Vec<monitor_check_ins::Model>, u64), MonitorError> {
        // Ensure the monitor belongs to the project before exposing its check-ins.
        self.get_monitor(project_id, monitor_id).await?;

        let page = page.unwrap_or(1).max(1);
        let page_size = page_size.unwrap_or(20).clamp(1, 100);

        let paginator = monitor_check_ins::Entity::find()
            .filter(monitor_check_ins::Column::MonitorId.eq(monitor_id))
            .order_by_desc(monitor_check_ins::Column::CreatedAt)
            .paginate(self.db.as_ref(), page_size);

        let total = paginator.num_items().await?;
        let items = paginator.fetch_page(page - 1).await?;
        Ok((items, total))
    }

    // ===================== Schedule helpers =====================

    /// Compute the next expected check-in time after `from`, or `None` when the
    /// monitor has no usable schedule.
    fn compute_next_expected(
        &self,
        schedule_type: &str,
        schedule: Option<&str>,
        schedule_unit: Option<&str>,
        from: DateTime<Utc>,
        slug: &str,
    ) -> Result<Option<DateTime<Utc>>, MonitorError> {
        let Some(schedule) = schedule.filter(|s| !s.is_empty()) else {
            return Ok(None);
        };

        match schedule_type {
            "interval" => {
                let count: i64 = schedule
                    .parse()
                    .map_err(|_| MonitorError::InvalidSchedule {
                        slug: slug.to_string(),
                        schedule: schedule.to_string(),
                        reason: "interval value must be an integer".to_string(),
                    })?;
                let unit = schedule_unit.unwrap_or("minute");
                let delta = interval_to_duration(count, unit).ok_or_else(|| {
                    MonitorError::InvalidSchedule {
                        slug: slug.to_string(),
                        schedule: schedule.to_string(),
                        reason: format!("unknown interval unit '{}'", unit),
                    }
                })?;
                Ok(Some(from + delta))
            }
            _ => {
                let normalized = normalize_cron(schedule);
                let parsed = cron::Schedule::from_str(&normalized).map_err(|e| {
                    MonitorError::InvalidSchedule {
                        slug: slug.to_string(),
                        schedule: schedule.to_string(),
                        reason: e.to_string(),
                    }
                })?;
                Ok(parsed.after(&from).next())
            }
        }
    }
}

/// Convert a `cron`-crate-incompatible 5-field expression to 6 fields by
/// prepending a seconds field (matches the deployment cron service behavior).
fn normalize_cron(schedule: &str) -> String {
    let fields = schedule.split_whitespace().count();
    if fields == 5 {
        format!("0 {}", schedule)
    } else {
        schedule.to_string()
    }
}

fn interval_to_duration(count: i64, unit: &str) -> Option<ChronoDuration> {
    let unit = unit.trim_end_matches('s').to_ascii_lowercase();
    match unit.as_str() {
        "minute" => Some(ChronoDuration::minutes(count)),
        "hour" => Some(ChronoDuration::hours(count)),
        "day" => Some(ChronoDuration::days(count)),
        "week" => Some(ChronoDuration::weeks(count)),
        "month" => Some(ChronoDuration::days(30 * count)),
        "year" => Some(ChronoDuration::days(365 * count)),
        _ => None,
    }
}

// ---- Small helpers to read back the effective value of an ActiveModel field ----

fn active_string(field: &sea_orm::ActiveValue<String>, fallback: &str) -> String {
    match field {
        sea_orm::ActiveValue::Set(v) => v.clone(),
        sea_orm::ActiveValue::Unchanged(v) => v.clone(),
        sea_orm::ActiveValue::NotSet => fallback.to_string(),
    }
}

fn active_str(
    field: &sea_orm::ActiveValue<Option<String>>,
    fallback: &Option<String>,
) -> Option<String> {
    match field {
        sea_orm::ActiveValue::Set(v) => v.clone(),
        sea_orm::ActiveValue::Unchanged(v) => v.clone(),
        sea_orm::ActiveValue::NotSet => fallback.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkin_status_parse() {
        assert_eq!(CheckInStatus::parse("ok"), Some(CheckInStatus::Ok));
        assert_eq!(CheckInStatus::parse("OK"), Some(CheckInStatus::Ok));
        assert_eq!(CheckInStatus::parse("error"), Some(CheckInStatus::Error));
        assert_eq!(CheckInStatus::parse("failed"), Some(CheckInStatus::Error));
        assert_eq!(
            CheckInStatus::parse("in_progress"),
            Some(CheckInStatus::InProgress)
        );
        assert_eq!(CheckInStatus::parse("bogus"), None);
        assert!(CheckInStatus::Ok.is_terminal());
        assert!(!CheckInStatus::InProgress.is_terminal());
    }

    #[test]
    fn test_normalize_cron_5_to_6_fields() {
        assert_eq!(normalize_cron("* * * * *"), "0 * * * * *");
        assert_eq!(normalize_cron("0 * * * * *"), "0 * * * * *");
        assert_eq!(normalize_cron("0 0 * * *"), "0 0 0 * * *");
    }

    #[test]
    fn test_interval_to_duration() {
        assert_eq!(
            interval_to_duration(5, "minute"),
            Some(ChronoDuration::minutes(5))
        );
        assert_eq!(
            interval_to_duration(2, "hours"),
            Some(ChronoDuration::hours(2))
        );
        assert_eq!(
            interval_to_duration(1, "day"),
            Some(ChronoDuration::days(1))
        );
        assert_eq!(interval_to_duration(1, "fortnight"), None);
    }

    // ===================== DB-backed tests =====================

    use crate::sentry::envelope::{CheckIn, CheckInMonitorConfig, CheckInSchedule};
    use sea_orm::{ActiveModelTrait, Set};
    use temps_database::test_utils::TestDatabase;
    use temps_entities::preset::Preset;
    use uuid::Uuid;

    async fn setup() -> (TestDatabase, Arc<MonitorService>, i32) {
        let db = TestDatabase::with_migrations().await.unwrap();
        let project = temps_entities::projects::ActiveModel {
            name: Set("Test Project".to_string()),
            repo_name: Set("test-repo".to_string()),
            repo_owner: Set("test-owner".to_string()),
            directory: Set("/test".to_string()),
            main_branch: Set("main".to_string()),
            slug: Set(format!("test-project-{}", Uuid::new_v4())),
            preset: Set(Preset::NextJs),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(db.connection())
        .await
        .unwrap();

        let service = Arc::new(MonitorService::new(db.connection_arc()));
        (db, service, project.id)
    }

    fn check_in(slug: &str, status: &str) -> CheckIn {
        CheckIn {
            check_in_id: None,
            monitor_slug: Some(slug.to_string()),
            status: status.to_string(),
            duration: None,
            release: None,
            environment: None,
            monitor_config: None,
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn test_record_check_in_auto_creates_monitor_with_schedule() {
        let (_db, service, project_id) = setup().await;

        let mut ci = check_in("nightly-report", "ok");
        ci.monitor_config = Some(CheckInMonitorConfig {
            schedule: Some(CheckInSchedule {
                schedule_type: "crontab".to_string(),
                value: serde_json::json!("0 0 * * *"),
                unit: None,
            }),
            checkin_margin: Some(5),
            max_runtime: Some(30),
            timezone: None,
        });

        service
            .record_check_in(project_id, None, &ci)
            .await
            .unwrap();

        let (monitors, total) = service.list_monitors(project_id, None, None).await.unwrap();
        assert_eq!(total, 1);
        let m = &monitors[0];
        assert_eq!(m.slug, "nightly-report");
        assert_eq!(m.status, "ok");
        assert_eq!(m.schedule.as_deref(), Some("0 0 * * *"));
        assert_eq!(m.checkin_margin_minutes, Some(5));
        assert_eq!(m.checkin_retention_days, 30);
        assert!(m.last_checkin_at.is_some());
        assert!(m.next_checkin_expected_at.is_some());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn test_in_progress_then_terminal_updates_single_row() {
        let (_db, service, project_id) = setup().await;
        let cid = Uuid::new_v4().to_string();

        let mut start = check_in("import-job", "in_progress");
        start.check_in_id = Some(cid.clone());
        service
            .record_check_in(project_id, None, &start)
            .await
            .unwrap();

        let mut finish = check_in("import-job", "ok");
        finish.check_in_id = Some(cid.clone());
        finish.duration = Some(2.5);
        service
            .record_check_in(project_id, None, &finish)
            .await
            .unwrap();

        let (monitors, _) = service.list_monitors(project_id, None, None).await.unwrap();
        let monitor_id = monitors[0].id;

        let (check_ins, total) = service
            .list_check_ins(project_id, monitor_id, None, None)
            .await
            .unwrap();
        // The in_progress row was resolved in place — exactly one row remains.
        assert_eq!(total, 1);
        assert_eq!(check_ins[0].status, "ok");
        assert_eq!(check_ins[0].duration_ms, Some(2500));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn test_missing_slug_is_validation_error() {
        let (_db, service, project_id) = setup().await;
        let mut ci = check_in("x", "ok");
        ci.monitor_slug = None;
        let err = service
            .record_check_in(project_id, None, &ci)
            .await
            .unwrap_err();
        assert!(matches!(err, MonitorError::MissingSlug { .. }));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn test_detect_unhealthy_flags_missed_check_in() {
        let (_db, service, project_id) = setup().await;

        // Create a monitor whose next expected check-in is already in the past.
        let now = Utc::now();
        let monitor = monitors::ActiveModel {
            project_id: Set(project_id),
            environment_id: Set(None),
            slug: Set("overdue".to_string()),
            name: Set(Some("overdue".to_string())),
            schedule: Set(Some("* * * * *".to_string())),
            schedule_type: Set("crontab".to_string()),
            schedule_unit: Set(None),
            checkin_margin_minutes: Set(Some(0)),
            max_runtime_minutes: Set(None),
            timezone: Set(None),
            status: Set("ok".to_string()),
            last_checkin_at: Set(Some(now - ChronoDuration::hours(2))),
            next_checkin_expected_at: Set(Some(now - ChronoDuration::hours(1))),
            checkin_retention_days: Set(30),
            muted: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(_db.connection())
        .await
        .unwrap();

        let transitioned = service.detect_unhealthy().await.unwrap();
        assert_eq!(transitioned, 1);

        let refreshed = service.get_monitor(project_id, monitor.id).await.unwrap();
        assert_eq!(refreshed.status, "missed");
        // Expectation advanced into the future so we don't re-alert every minute.
        assert!(refreshed.next_checkin_expected_at.unwrap() > now);
    }
}
