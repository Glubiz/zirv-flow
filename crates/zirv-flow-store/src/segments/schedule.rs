//! A segment's cadence, mirrored to the generic `schedules` row the scheduler leader fires, as a
//! source's cron is (ruling 303ac6ac). The cron is UTC with a stable phase from the segment id,
//! so a workspace's segments spread over the interval instead of refreshing on the same minute.

use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::schedule::{cadence_cron, next_after};

use zirv_flow_core::{Cause, TraceContext};

use super::refresh::{REFRESH_JOB_KIND, RefreshRequest, RefreshTrigger, enqueue_refresh};
use crate::StoreError;

/// The zone a segment cron is read in.
const SCHEDULE_TIMEZONE: &str = "UTC";

/// The phase of a segment: the last 8 bytes of its id as a number. A version 7 id keeps its
/// random bits there, where the first bytes are a timestamp that ids created together share.
pub(crate) fn phase_of(segment: Uuid) -> u64 {
    segment.as_u64_pair().1
}

/// The UTC cron of a segment's refresh; none for a cadence outside the ruled set.
pub fn segment_cron(segment: Uuid, cadence_minutes: i32) -> Option<String> {
    cadence_cron(cadence_minutes, phase_of(segment))
}

/// Makes the `schedules` row of a segment match its cadence, in the caller's transaction: the row
/// exists with the cron of the cadence and the segment's phase. A changed cron moves the first
/// slot to the next one after now; an unchanged one keeps its place.
pub(crate) async fn sync_segment_schedule(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment: Uuid,
    cadence_minutes: i32,
) -> Result<(), StoreError> {
    let slot = segment_cron(segment, cadence_minutes).and_then(|cron| {
        let slot = next_after(&cron, SCHEDULE_TIMEZONE, OffsetDateTime::now_utc()).ok()?;
        Some((cron, slot))
    });
    let Some((cron, next_run_at)) = slot else {
        return remove_segment_schedule(connection, workspace, segment).await;
    };
    sqlx::query(
        "INSERT INTO schedules (id, workspace_id, kind, cron, timezone, next_run_at, payload, \
            enabled, segment_id) \
         VALUES ($1, $2, $3, $4, $5, $6, \
                 jsonb_build_object('workspace_id', $2::uuid, 'segment_id', $7::uuid, \
                                    'trigger', $8::text), true, $7) \
         ON CONFLICT (segment_id) WHERE segment_id IS NOT NULL DO UPDATE SET \
            next_run_at = CASE WHEN schedules.cron <> EXCLUDED.cron \
                               THEN EXCLUDED.next_run_at ELSE schedules.next_run_at END, \
            cron = EXCLUDED.cron, enabled = true \
         WHERE schedules.workspace_id = EXCLUDED.workspace_id",
    )
    .bind(Uuid::new_v4())
    .bind(workspace.as_uuid())
    .bind(REFRESH_JOB_KIND)
    .bind(cron)
    .bind(SCHEDULE_TIMEZONE)
    .bind(next_run_at)
    .bind(segment)
    .bind(RefreshTrigger::Schedule.as_str())
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Removes the segment's `schedules` row, if it has one.
pub(crate) async fn remove_segment_schedule(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment: Uuid,
) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM schedules WHERE segment_id = $1 AND workspace_id = $2")
        .bind(segment)
        .bind(workspace.as_uuid())
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

/// Brings segments saved before schedules and membership existed in line, in one transaction:
/// every live segment without a `schedules` row gets one, and every live segment never computed
/// gets a `save` refresh under its version's key, so one already queued is not doubled. The cron
/// phase is computed in Rust, so this runs from the leader's upkeep and not from a migration.
/// Returns the schedules created and the refreshes enqueued; a second call finds nothing to do.
pub async fn reconcile_segment_schedules(
    connection: &mut PgConnection,
) -> Result<(u64, u64), StoreError> {
    let unscheduled: Vec<(Uuid, Uuid, i32)> = sqlx::query_as(
        "SELECT s.workspace_id, s.id, s.cadence_minutes FROM segments s \
         WHERE s.archived_at IS NULL \
           AND NOT EXISTS (SELECT 1 FROM schedules c WHERE c.segment_id = s.id) \
         ORDER BY s.id",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut scheduled = 0;
    for (workspace, segment, cadence) in unscheduled {
        sync_segment_schedule(
            connection,
            WorkspaceId::from_uuid(workspace),
            segment,
            cadence,
        )
        .await?;
        scheduled += 1;
    }
    let uncomputed: Vec<(Uuid, Uuid, i32)> = sqlx::query_as(
        "SELECT workspace_id, id, current_version FROM segments \
         WHERE archived_at IS NULL AND computed_version IS NULL ORDER BY id",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut queued = 0;
    for (workspace, segment, version) in uncomputed {
        let trace = TraceContext::root(Cause::definition_change(segment));
        if enqueue_refresh(
            connection,
            WorkspaceId::from_uuid(workspace),
            segment,
            RefreshRequest::Save { version },
            &trace,
        )
        .await?
        {
            queued += 1;
        }
    }
    Ok((scheduled, queued))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::*;
    use crate::segments::tests_support::{segment, workspace};
    use crate::{archive_segment, begin, commit, lock_segments};

    #[test]
    fn the_phase_is_the_last_8_bytes_of_the_id_and_stable() {
        let id = Uuid::from_bytes([
            0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0, 0, 0, 0, 0, 0, 0x10, 0x92,
        ]);

        assert_eq!(phase_of(id), 0x1092);
        assert_eq!(phase_of(id), phase_of(id));
    }

    #[test]
    fn the_cron_follows_the_cadence_and_the_phase_of_the_id() {
        let id = Uuid::from_u128(4242);

        assert_eq!(segment_cron(id, 60).as_deref(), Some("42 * * * *"));
        assert_eq!(segment_cron(id, 1440).as_deref(), Some("42 22 * * *"));
        assert_eq!(segment_cron(id, 7), None);
    }

    async fn rows(pool: &PgPool) -> Vec<(String, String, bool, OffsetDateTime, Value)> {
        sqlx::query_as(
            "SELECT kind, cron, enabled, next_run_at, payload FROM schedules \
             WHERE segment_id IS NOT NULL",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_schedule_is_mirrored_on_create_moved_on_a_cadence_change_and_removed_on_archive(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;

        let created = segment(&pool, ws, "Pros", 60).await;
        let first = rows(&pool).await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, "segment.refresh");
        assert_eq!(first[0].1, segment_cron(created.id, 60).unwrap());
        assert!(first[0].2);
        assert!(first[0].3 > OffsetDateTime::now_utc());
        assert_eq!(
            first[0].4,
            json!({"workspace_id": ws, "segment_id": created.id, "trigger": "schedule"})
        );

        // The same cadence keeps its slot; a changed one moves it to the new cron.
        sqlx::query("UPDATE schedules SET next_run_at = next_run_at + interval '1 day'")
            .execute(&pool)
            .await
            .unwrap();
        let kept = rows(&pool).await[0].3;
        let mut tx = begin(&pool).await.unwrap();
        sync_segment_schedule(&mut tx, ws, created.id, 60)
            .await
            .unwrap();
        commit(tx).await.unwrap();
        assert_eq!(rows(&pool).await[0].3, kept);
        let mut tx = begin(&pool).await.unwrap();
        sync_segment_schedule(&mut tx, ws, created.id, 1440)
            .await
            .unwrap();
        commit(tx).await.unwrap();
        let moved = rows(&pool).await;
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].1, segment_cron(created.id, 1440).unwrap());
        assert_ne!(moved[0].3, kept);

        let mut tx = begin(&pool).await.unwrap();
        lock_segments(&mut tx, ws).await.unwrap();
        archive_segment(&mut tx, ws, created.id).await.unwrap();
        commit(tx).await.unwrap();
        assert!(rows(&pool).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn another_workspaces_row_is_never_changed(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let mine = segment(&pool, one, "Pros", 60).await;

        let mut tx = begin(&pool).await.unwrap();
        sync_segment_schedule(&mut tx, two, mine.id, 1440)
            .await
            .unwrap();
        remove_segment_schedule(&mut tx, two, mine.id)
            .await
            .unwrap();
        commit(tx).await.unwrap();

        let all = rows(&pool).await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1, segment_cron(mine.id, 60).unwrap());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn reconciling_schedules_and_queues_a_segment_left_by_version_0_50_0_once(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = Uuid::now_v7();
        // As migration 0030 left it: live, no schedule, never computed.
        let mut tx = begin(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO segments (id, workspace_id, name, current_version, cadence_minutes) \
             VALUES ($1, $2, 'Old', 1, 30)",
        )
        .bind(id)
        .bind(ws.as_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO segment_versions (workspace_id, segment_id, version, definition, \
                normalized, deps, time_sensitive, trace_id) \
             VALUES ($1, $2, 1, '{}', '{}', '{}', false, gen_random_uuid())",
        )
        .bind(ws.as_uuid())
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();
        commit(tx).await.unwrap();

        let mut tx = begin(&pool).await.unwrap();
        let first = reconcile_segment_schedules(&mut tx).await.unwrap();
        commit(tx).await.unwrap();
        let mut tx = begin(&pool).await.unwrap();
        let second = reconcile_segment_schedules(&mut tx).await.unwrap();
        commit(tx).await.unwrap();

        assert_eq!((first, second), ((1, 1), (0, 0)));
        let cron: String = sqlx::query_scalar("SELECT cron FROM schedules WHERE segment_id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(cron, segment_cron(id, 30).unwrap());
        let keys: Vec<String> =
            sqlx::query_scalar("SELECT unique_key FROM jobs WHERE kind = 'segment.refresh'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(keys, [format!("segment.refresh:{id}:v1")]);
    }
}
