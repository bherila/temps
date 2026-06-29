use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use temps_core::DBDateTime;

/// A scheduled job being monitored via Sentry-compatible cron check-ins.
///
/// Unlike [`super::crons`] (which Temps actively invokes over HTTP), a monitor is
/// passive: the deployed application reports in by sending check-ins. A monitor is
/// uniquely identified by `(project_id, slug)` and is auto-created the first time a
/// check-in arrives for an unknown slug.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "monitors")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub project_id: i32,
    pub environment_id: Option<i32>,
    /// Stable, app-supplied identifier (Sentry `monitor_slug`).
    pub slug: String,
    pub name: Option<String>,
    /// Schedule expression. Interpreted according to `schedule_type`.
    pub schedule: Option<String>,
    /// `"crontab"` (default) or `"interval"`.
    pub schedule_type: String,
    /// For interval schedules: the unit (`minute`, `hour`, `day`, `week`, `month`, `year`).
    pub schedule_unit: Option<String>,
    /// Grace period (minutes) after the expected time before a missing check-in is
    /// flagged as `missed`.
    pub checkin_margin_minutes: Option<i32>,
    /// Maximum allowed runtime (minutes) for an in-progress check-in before it is
    /// flagged as `timeout`.
    pub max_runtime_minutes: Option<i32>,
    pub timezone: Option<String>,
    /// Lifecycle/health status: `active`, `disabled`, `ok`, `error`, `timeout`, `missed`.
    pub status: String,
    pub last_checkin_at: Option<DBDateTime>,
    /// When the next check-in is expected; used by the missed-check-in detector.
    pub next_checkin_expected_at: Option<DBDateTime>,
    /// Per-monitor retention for check-in rows, enforced by the application cleanup
    /// loop. Bounds row growth for high-frequency monitors (a 1-minute job produces
    /// ~525k rows/year).
    pub checkin_retention_days: i32,
    /// When true, health alerts are suppressed for this monitor.
    pub muted: bool,
    pub created_at: DBDateTime,
    pub updated_at: DBDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::monitor_check_ins::Entity")]
    CheckIns,
}

impl Related<super::monitor_check_ins::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::CheckIns.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
