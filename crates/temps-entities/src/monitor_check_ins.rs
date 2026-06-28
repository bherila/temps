use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use temps_core::DBDateTime;

/// A single check-in reported for a [`super::monitors`] monitor.
///
/// This is a TimescaleDB hypertable partitioned on `created_at`. `id` is an
/// auto-increment sequence used for ORM lookups but is intentionally *not* a
/// primary-key constraint (hypertables require the partition column to participate
/// in any unique/PK constraint — same pattern as `error_events`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "monitor_check_ins")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub monitor_id: i32,
    /// App-supplied check-in UUID, used to correlate an `in_progress` check-in with
    /// its later terminal (`ok`/`error`) report.
    pub check_in_id: Option<String>,
    /// `in_progress`, `ok`, `error`, `timeout`, or `missed`.
    pub status: String,
    pub duration_ms: Option<i64>,
    pub environment: Option<String>,
    pub release: Option<String>,
    pub created_at: DBDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::monitors::Entity",
        from = "Column::MonitorId",
        to = "super::monitors::Column::Id"
    )]
    Monitor,
}

impl Related<super::monitors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Monitor.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
