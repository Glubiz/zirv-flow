//! A source's cron, mirrored to the generic `schedules` row the scheduler leader fires.

use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::schedule::next_after;

use crate::StoreError;
use crate::sync_runs::RUN_JOB_KIND;

/// The zone a source's cron is read in.
pub const SCHEDULE_TIMEZONE: &str = "UTC";

/// Makes the `schedules` row of a source match its settings, in the caller's transaction:
///
/// - with a `schedule`, the row exists with that cron, enabled as the source is. A changed cron
///   moves the first slot to the next one after now; an unchanged one keeps its place;
/// - without one, the row is removed.
///
/// A cron that has no later slot removes the row as well. Deleting the source removes the row
/// through its foreign key.
pub async fn sync_schedule(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    source_id: Uuid,
    schedule: Option<&str>,
    enabled: bool,
) -> Result<(), StoreError> {
    let next = schedule
        .filter(|cron| !cron.is_empty())
        .map(|cron| {
            next_after(cron, SCHEDULE_TIMEZONE, OffsetDateTime::now_utc()).map(|slot| (cron, slot))
        })
        .transpose();
    let Ok(Some((cron, next_run_at))) = next else {
        sqlx::query("DELETE FROM schedules WHERE source_id = $1 AND workspace_id = $2")
            .bind(source_id)
            .bind(workspace.as_uuid())
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO schedules (id, workspace_id, kind, cron, timezone, next_run_at, payload, \
            enabled, source_id) \
         VALUES ($1, $2, $3, $4, $5, $6, jsonb_build_object('source_id', $7::uuid), $8, $7) \
         ON CONFLICT (source_id) WHERE source_id IS NOT NULL DO UPDATE SET \
            next_run_at = CASE WHEN schedules.cron <> EXCLUDED.cron \
                               THEN EXCLUDED.next_run_at ELSE schedules.next_run_at END, \
            cron = EXCLUDED.cron, enabled = EXCLUDED.enabled \
         WHERE schedules.workspace_id = EXCLUDED.workspace_id",
    )
    .bind(Uuid::new_v4())
    .bind(workspace.as_uuid())
    .bind(RUN_JOB_KIND)
    .bind(cron)
    .bind(SCHEDULE_TIMEZONE)
    .bind(next_run_at)
    .bind(source_id)
    .bind(enabled)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::*;
    use crate::sync_runs::testing::{source_with_stream, workspace};
    use crate::{begin, commit, delete_source};

    async fn rows(pool: &PgPool) -> Vec<(String, String, bool, OffsetDateTime, Value)> {
        sqlx::query_as(
            "SELECT kind, cron, enabled, next_run_at, payload FROM schedules \
             WHERE source_id IS NOT NULL",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn mirror(pool: &PgPool, ws: WorkspaceId, source: Uuid, cron: Option<&str>, on: bool) {
        let mut tx = begin(pool).await.unwrap();
        sync_schedule(&mut tx, ws, source, cron, on).await.unwrap();
        commit(tx).await.unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_schedule_is_mirrored_on_create_moved_on_change_and_stopped_on_disable_clear_and_delete(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let (source, _) = source_with_stream(&pool, ws, "crm", "full_refresh").await;
        assert!(rows(&pool).await.is_empty());

        mirror(&pool, ws, source, Some("0 * * * *"), true).await;
        let created = rows(&pool).await;
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].0, "sync.run");
        assert_eq!(created[0].1, "0 * * * *");
        assert!(created[0].2);
        assert!(created[0].3 > OffsetDateTime::now_utc());
        assert_eq!(created[0].4, json!({"source_id": source}));

        // The same cron keeps its slot; a changed one moves to the next slot of the new cron.
        sqlx::query("UPDATE schedules SET next_run_at = next_run_at + interval '1 day'")
            .execute(&pool)
            .await
            .unwrap();
        let kept_slot = rows(&pool).await[0].3;
        mirror(&pool, ws, source, Some("0 * * * *"), true).await;
        assert_eq!(rows(&pool).await[0].3, kept_slot);
        mirror(&pool, ws, source, Some("30 * * * *"), true).await;
        let changed = rows(&pool).await;
        assert_eq!(changed.len(), 1, "one row per source");
        assert_eq!(changed[0].1, "30 * * * *");
        assert!(changed[0].3 < kept_slot);

        mirror(&pool, ws, source, Some("30 * * * *"), false).await;
        assert!(!rows(&pool).await[0].2, "a disabled source does not fire");
        mirror(&pool, ws, source, Some("30 * * * *"), true).await;
        assert!(rows(&pool).await[0].2);

        mirror(&pool, ws, source, None, true).await;
        assert!(rows(&pool).await.is_empty(), "no schedule, no row");
        mirror(&pool, ws, source, Some(""), true).await;
        assert!(rows(&pool).await.is_empty());

        mirror(&pool, ws, source, Some("0 * * * *"), true).await;
        let mut connection = pool.acquire().await.unwrap();
        delete_source(&mut connection, ws, source).await.unwrap();
        assert!(rows(&pool).await.is_empty(), "deleting the source stops it");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn mirroring_changes_only_the_sources_own_workspace_row(pool: PgPool) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let (first, _) = source_with_stream(&pool, one, "crm", "full_refresh").await;
        let (second, _) = source_with_stream(&pool, two, "crm", "full_refresh").await;
        mirror(&pool, one, first, Some("0 * * * *"), true).await;
        mirror(&pool, two, second, Some("0 * * * *"), true).await;

        // A workspace that does not own the source cannot clear its row.
        mirror(&pool, two, first, None, true).await;

        assert_eq!(rows(&pool).await.len(), 2);
    }
}
