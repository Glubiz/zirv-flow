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

/// The cron and its next slot after now; none for no cron, an empty one, or one with no later slot.
fn first_slot(cron: Option<&str>) -> Option<(&str, OffsetDateTime)> {
    let cron = cron.filter(|cron| !cron.is_empty())?;
    let slot = next_after(cron, SCHEDULE_TIMEZONE, OffsetDateTime::now_utc()).ok()?;
    Some((cron, slot))
}

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
    let Some((cron, next_run_at)) = first_slot(schedule) else {
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

/// Makes the reconciliation `schedules` row of a stream match its settings, in the caller's
/// transaction, as [`sync_schedule`] does for a source:
///
/// - with a cron, the row exists with that cron, enabled as asked. Its payload names the source
///   and the stream and `reconcile`, and no `run_id`, which is how `sync.run` knows to queue a
///   reconciliation. A changed cron moves the first slot to the next one after now;
/// - without one, or with a cron that has no later slot, the row is removed.
///
/// The row leaves `source_id` null, so it never collides with the source's own schedule row.
/// Deleting the stream or its source removes it through the foreign key.
pub async fn sync_reconcile_schedule(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    source_id: Uuid,
    stream_id: Uuid,
    cron: Option<&str>,
    enabled: bool,
) -> Result<(), StoreError> {
    let Some((cron, next_run_at)) = first_slot(cron) else {
        sqlx::query("DELETE FROM schedules WHERE stream_id = $1 AND workspace_id = $2")
            .bind(stream_id)
            .bind(workspace.as_uuid())
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO schedules (id, workspace_id, kind, cron, timezone, next_run_at, payload, \
            enabled, stream_id) \
         VALUES ($1, $2, $3, $4, $5, $6, \
                 jsonb_build_object('source_id', $7::uuid, 'stream_id', $8::uuid, \
                                    'reconcile', true), $9, $8) \
         ON CONFLICT (stream_id) WHERE stream_id IS NOT NULL DO UPDATE SET \
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
    .bind(stream_id)
    .bind(enabled)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Mirrors a source's `enabled` flag onto the reconciliation `schedules` rows of its streams, in
/// the caller's transaction: a row fires only while the source and its stream are both enabled.
/// Only the workspace's own rows change.
pub async fn sync_source_reconcile_schedules(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    source_id: Uuid,
    source_enabled: bool,
) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE schedules SET enabled = $3 AND streams.enabled \
         FROM streams \
         WHERE schedules.stream_id = streams.id AND schedules.workspace_id = $1 \
           AND streams.workspace_id = $1 AND streams.source_id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(source_id)
    .bind(source_enabled)
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

    async fn reconcile_rows(
        pool: &PgPool,
    ) -> Vec<(String, String, bool, OffsetDateTime, Value, Option<Uuid>)> {
        sqlx::query_as(
            "SELECT kind, cron, enabled, next_run_at, payload, source_id FROM schedules \
             WHERE stream_id IS NOT NULL",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn mirror_reconcile(
        pool: &PgPool,
        ws: WorkspaceId,
        source: Uuid,
        stream: Uuid,
        cron: Option<&str>,
        on: bool,
    ) {
        let mut tx = begin(pool).await.unwrap();
        sync_reconcile_schedule(&mut tx, ws, source, stream, cron, on)
            .await
            .unwrap();
        commit(tx).await.unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reconcile_schedule_is_created_moved_cleared_and_leaves_the_sources_own_row_alone(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let (source, stream) = source_with_stream(&pool, ws, "crm", "incremental").await;
        mirror(&pool, ws, source, Some("0 * * * *"), true).await;

        mirror_reconcile(&pool, ws, source, stream, Some("0 3 * * *"), true).await;

        let created = reconcile_rows(&pool).await;
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].0, "sync.run");
        assert_eq!(created[0].1, "0 3 * * *");
        assert!(created[0].2 && created[0].3 > OffsetDateTime::now_utc());
        assert_eq!(
            created[0].4,
            json!({"source_id": source, "stream_id": stream, "reconcile": true})
        );
        assert_eq!(
            created[0].5, None,
            "the source's own row is a different row"
        );
        assert_eq!(
            rows(&pool).await.len(),
            1,
            "the source's schedule is untouched"
        );

        // The same cron keeps its slot; a changed one moves; a disabled stream does not fire.
        sqlx::query("UPDATE schedules SET next_run_at = next_run_at + interval '1 day' WHERE stream_id IS NOT NULL")
            .execute(&pool)
            .await
            .unwrap();
        let kept = reconcile_rows(&pool).await[0].3;
        mirror_reconcile(&pool, ws, source, stream, Some("0 3 * * *"), true).await;
        assert_eq!(reconcile_rows(&pool).await[0].3, kept);
        mirror_reconcile(&pool, ws, source, stream, Some("30 4 * * *"), false).await;
        let changed = reconcile_rows(&pool).await;
        assert_eq!(changed.len(), 1, "one row per stream");
        assert_eq!(changed[0].1, "30 4 * * *");
        assert!(!changed[0].2);
        assert!(changed[0].3 < kept);

        mirror_reconcile(&pool, ws, source, stream, None, true).await;
        assert!(reconcile_rows(&pool).await.is_empty());
        assert_eq!(
            rows(&pool).await.len(),
            1,
            "clearing leaves the source's row"
        );
        mirror_reconcile(&pool, ws, source, stream, Some(""), true).await;
        assert!(reconcile_rows(&pool).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_the_stream_or_the_source_removes_its_reconcile_schedule(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let (source, stream) = source_with_stream(&pool, ws, "crm", "incremental").await;
        mirror_reconcile(&pool, ws, source, stream, Some("0 3 * * *"), true).await;

        sqlx::query("DELETE FROM streams WHERE id = $1")
            .bind(stream)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            reconcile_rows(&pool).await.is_empty(),
            "the stream's row goes with it"
        );

        let (second, second_stream) = source_with_stream(&pool, ws, "erp", "incremental").await;
        mirror_reconcile(&pool, ws, second, second_stream, Some("0 3 * * *"), true).await;
        let mut connection = pool.acquire().await.unwrap();
        delete_source(&mut connection, ws, second).await.unwrap();
        assert!(
            reconcile_rows(&pool).await.is_empty(),
            "and with its source"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn mirroring_a_reconcile_schedule_changes_only_the_streams_own_workspace_row(
        pool: PgPool,
    ) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let (first, first_stream) = source_with_stream(&pool, one, "crm", "incremental").await;
        let (second, second_stream) = source_with_stream(&pool, two, "crm", "incremental").await;
        mirror_reconcile(&pool, one, first, first_stream, Some("0 3 * * *"), true).await;
        mirror_reconcile(&pool, two, second, second_stream, Some("0 3 * * *"), true).await;

        // A workspace that does not own the stream cannot change or clear its row.
        mirror_reconcile(&pool, two, first, first_stream, Some("5 5 * * *"), true).await;
        mirror_reconcile(&pool, two, first, first_stream, None, true).await;

        let rows = reconcile_rows(&pool).await;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.1 == "0 3 * * *"));
    }
}
