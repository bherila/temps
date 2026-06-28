//! Migration to add Sentry-compatible cron monitor check-ins.
//!
//! This migration adds:
//! - `monitors` table: one row per scheduled job being monitored (cron check-ins).
//!   A monitor is identified by `(project_id, slug)` and carries its expected
//!   schedule plus per-monitor retention configuration.
//! - `monitor_check_ins` table: one row per check-in reported by the app. This is
//!   high-volume time-series data (a 1-minute job produces ~525k rows/year), so it
//!   is converted to a TimescaleDB hypertable with:
//!     * a coarse table-level retention backstop (365 days) bounding worst-case
//!       storage even if application-level cleanup fails, and
//!     * a per-monitor `checkin_retention_days` column (enforced by an application
//!       cleanup loop) so operators can tune retention per record at runtime.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Coarse storage backstop. Per-monitor retention (`checkin_retention_days`,
/// default 30) is enforced in the application; this table-level policy only
/// guarantees old chunks are eventually dropped even if that loop stops running.
const CHECKIN_RETENTION_BACKSTOP_DAYS: i64 = 365;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // ===== monitors =====
        manager
            .create_table(
                Table::create()
                    .table(Monitors::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Monitors::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Monitors::ProjectId).integer().not_null())
                    .col(ColumnDef::new(Monitors::EnvironmentId).integer().null())
                    .col(ColumnDef::new(Monitors::Slug).string_len(255).not_null())
                    .col(ColumnDef::new(Monitors::Name).string_len(255).null())
                    .col(ColumnDef::new(Monitors::Schedule).string_len(255).null())
                    .col(
                        ColumnDef::new(Monitors::ScheduleType)
                            .string_len(32)
                            .not_null()
                            .default("crontab"),
                    )
                    .col(ColumnDef::new(Monitors::ScheduleUnit).string_len(16).null())
                    .col(
                        ColumnDef::new(Monitors::CheckinMarginMinutes)
                            .integer()
                            .null(),
                    )
                    .col(ColumnDef::new(Monitors::MaxRuntimeMinutes).integer().null())
                    .col(ColumnDef::new(Monitors::Timezone).string_len(64).null())
                    .col(
                        ColumnDef::new(Monitors::Status)
                            .string_len(32)
                            .not_null()
                            .default("active"),
                    )
                    .col(
                        ColumnDef::new(Monitors::LastCheckinAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(Monitors::NextCheckinExpectedAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(Monitors::CheckinRetentionDays)
                            .integer()
                            .not_null()
                            .default(30),
                    )
                    .col(
                        ColumnDef::new(Monitors::Muted)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        ColumnDef::new(Monitors::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        ColumnDef::new(Monitors::UpdatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_foreign_key(
                ForeignKey::create()
                    .name("fk_monitors_project")
                    .from(Monitors::Table, Monitors::ProjectId)
                    .to(Projects::Table, Projects::Id)
                    .on_delete(ForeignKeyAction::Cascade)
                    .to_owned(),
            )
            .await?;

        // A monitor is uniquely identified by (project_id, slug) — this is the
        // upsert key used when a check-in arrives for a not-yet-seen monitor.
        manager
            .create_index(
                Index::create()
                    .name("idx_monitors_project_slug")
                    .table(Monitors::Table)
                    .col(Monitors::ProjectId)
                    .col(Monitors::Slug)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // ===== monitor_check_ins =====
        // NOTE: `id` is auto_increment for ORM lookups but is NOT a primary key
        // constraint — TimescaleDB hypertables require any unique/PK constraint to
        // include the partitioning column. We mirror the `error_events` pattern:
        // a bare auto_increment id plus a `created_at` partition column.
        manager
            .create_table(
                Table::create()
                    .table(MonitorCheckIns::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(MonitorCheckIns::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment(),
                    )
                    .col(
                        ColumnDef::new(MonitorCheckIns::MonitorId)
                            .integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(MonitorCheckIns::CheckInId)
                            .string_len(64)
                            .null(),
                    )
                    .col(
                        ColumnDef::new(MonitorCheckIns::Status)
                            .string_len(16)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(MonitorCheckIns::DurationMs)
                            .big_integer()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(MonitorCheckIns::Environment)
                            .string_len(255)
                            .null(),
                    )
                    .col(
                        ColumnDef::new(MonitorCheckIns::Release)
                            .string_len(255)
                            .null(),
                    )
                    .col(
                        ColumnDef::new(MonitorCheckIns::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_foreign_key(
                ForeignKey::create()
                    .name("fk_monitor_check_ins_monitor")
                    .from(MonitorCheckIns::Table, MonitorCheckIns::MonitorId)
                    .to(Monitors::Table, Monitors::Id)
                    .on_delete(ForeignKeyAction::Cascade)
                    .to_owned(),
            )
            .await?;

        // Primary listing path: latest check-ins for a monitor.
        manager
            .create_index(
                Index::create()
                    .name("idx_monitor_check_ins_monitor_created")
                    .table(MonitorCheckIns::Table)
                    .col(MonitorCheckIns::MonitorId)
                    .col(MonitorCheckIns::CreatedAt)
                    .to_owned(),
            )
            .await?;

        // Correlation path: resolve an in_progress check-in to its terminal status.
        manager
            .create_index(
                Index::create()
                    .name("idx_monitor_check_ins_checkin_id")
                    .table(MonitorCheckIns::Table)
                    .col(MonitorCheckIns::CheckInId)
                    .to_owned(),
            )
            .await?;

        // Convert to a TimescaleDB hypertable + retention backstop (Postgres only).
        if manager.get_database_backend() == sea_orm::DatabaseBackend::Postgres {
            let sql = format!(
                r#"
                SELECT create_hypertable(
                    'monitor_check_ins',
                    'created_at',
                    chunk_time_interval => INTERVAL '1 day',
                    if_not_exists => TRUE,
                    migrate_data => TRUE
                );

                SELECT add_retention_policy('monitor_check_ins', INTERVAL '{} days', if_not_exists => TRUE);
                "#,
                CHECKIN_RETENTION_BACKSTOP_DAYS
            );

            manager.get_connection().execute_unprepared(&sql).await?;
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(MonitorCheckIns::Table).to_owned())
            .await?;

        manager
            .drop_table(Table::drop().table(Monitors::Table).to_owned())
            .await?;

        Ok(())
    }
}

#[derive(DeriveIden)]
enum Monitors {
    Table,
    Id,
    ProjectId,
    EnvironmentId,
    Slug,
    Name,
    Schedule,
    ScheduleType,
    ScheduleUnit,
    CheckinMarginMinutes,
    MaxRuntimeMinutes,
    Timezone,
    Status,
    LastCheckinAt,
    NextCheckinExpectedAt,
    CheckinRetentionDays,
    Muted,
    CreatedAt,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum MonitorCheckIns {
    Table,
    Id,
    MonitorId,
    CheckInId,
    Status,
    DurationMs,
    Environment,
    Release,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Projects {
    Table,
    Id,
}
