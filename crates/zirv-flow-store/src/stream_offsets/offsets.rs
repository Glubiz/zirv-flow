//! Reading offsets and reporting lag.

use std::collections::BTreeMap;

use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::sync_runs::{Lease, Unfenced, fence_run};
use crate::{
    PgPool, StoreError, StreamOutcome, begin, commit, database_now, record_stream_outcome,
};

/// One partition of a stream's topic: the next offset to read and the log's end when last seen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartitionOffset {
    pub partition: i32,
    /// The next offset to read.
    pub next: i64,
    /// The high watermark; `None` while no fetch has seen it.
    pub end: Option<i64>,
}

/// How far behind one partition is, in messages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartitionLag {
    pub partition: i32,
    pub lag: i64,
}

#[derive(FromRow)]
struct OffsetRow {
    partition: i32,
    next_offset: i64,
    high_watermark: Option<i64>,
}

/// The stored offsets of a stream of the workspace, by partition. A partition without a row
/// starts at its earliest offset.
pub async fn load_offsets<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    stream_id: Uuid,
) -> Result<Vec<PartitionOffset>, StoreError> {
    let rows: Vec<OffsetRow> = sqlx::query_as(
        "SELECT partition, next_offset, high_watermark FROM stream_offsets \
         WHERE workspace_id = $1 AND stream_id = $2 ORDER BY partition",
    )
    .bind(workspace.as_uuid())
    .bind(stream_id)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(|row| PartitionOffset {
            partition: row.partition,
            next: row.next_offset,
            end: row.high_watermark,
        })
        .collect())
}

/// What a lag report did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LagReport {
    /// The watermarks are stored and the stream's freshness recorded.
    Reported {
        /// The lag over all partitions, in messages.
        lag: i64,
        /// Whether it is within `max_lag`: the outcome recorded is `Succeeded`, else `Failed`.
        within_bound: bool,
    },
    /// The job's lease is gone. Nothing was written.
    LeaseLost,
    /// The run is no longer running. Nothing was written.
    Cancelled,
}

/// Stores the high watermarks the consumer saw, in a transaction fenced on the job's lease and
/// the run's state, and records the stream's freshness: `Succeeded` when the lag over all
/// partitions is at most `max_lag`, `Failed` otherwise. A partition without a row gets one at
/// `next`; an existing row keeps its `next_offset`, which only a batch commit moves.
pub async fn report_stream_lag(
    pool: &PgPool,
    workspace: WorkspaceId,
    stream_id: Uuid,
    run_id: Uuid,
    lease: Lease,
    offsets: &[PartitionOffset],
    max_lag: i64,
) -> Result<LagReport, StoreError> {
    let mut tx = begin(pool).await?;
    match fence_run(&mut tx, workspace, run_id, lease.job_id, lease.token).await? {
        Some(Unfenced::LeaseLost) => return Ok(LagReport::LeaseLost),
        Some(Unfenced::Cancelled) => return Ok(LagReport::Cancelled),
        None => {}
    }
    let partitions: Vec<i32> = offsets.iter().map(|offset| offset.partition).collect();
    let nexts: Vec<i64> = offsets.iter().map(|offset| offset.next).collect();
    let ends: Vec<Option<i64>> = offsets.iter().map(|offset| offset.end).collect();
    sqlx::query(
        "INSERT INTO stream_offsets \
            (workspace_id, stream_id, partition, next_offset, high_watermark, observed_at) \
         SELECT $1, $2, t.partition, t.next_offset, t.high_watermark, \
                CASE WHEN t.high_watermark IS NULL THEN NULL ELSE now() END \
         FROM unnest($3::int4[], $4::int8[], $5::int8[]) \
              AS t(partition, next_offset, high_watermark) \
         ON CONFLICT (stream_id, partition) DO UPDATE SET \
            high_watermark = COALESCE(EXCLUDED.high_watermark, stream_offsets.high_watermark), \
            observed_at = CASE WHEN EXCLUDED.high_watermark IS NULL \
                               THEN stream_offsets.observed_at ELSE now() END, \
            updated_at = now()",
    )
    .bind(workspace.as_uuid())
    .bind(stream_id)
    .bind(&partitions)
    .bind(&nexts)
    .bind(&ends)
    .execute(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    let lag: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(GREATEST(high_watermark - next_offset, 0)), 0)::int8 \
         FROM stream_offsets \
         WHERE workspace_id = $1 AND stream_id = $2 AND high_watermark IS NOT NULL",
    )
    .bind(workspace.as_uuid())
    .bind(stream_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    let within_bound = lag <= max_lag;
    let at = database_now(&mut *tx).await?;
    record_stream_outcome(
        &mut tx,
        workspace,
        stream_id,
        if within_bound {
            StreamOutcome::Succeeded
        } else {
            StreamOutcome::Failed
        },
        at,
    )
    .await?;
    commit(tx).await?;
    Ok(LagReport::Reported { lag, within_bound })
}

/// The lag per partition of every stream of a source of the workspace that has seen a watermark,
/// by stream id. Counts, not record values: members may read them.
pub async fn stream_lag<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    source_id: Uuid,
) -> Result<BTreeMap<Uuid, Vec<PartitionLag>>, StoreError> {
    let rows: Vec<(Uuid, i32, i64)> = sqlx::query_as(
        "SELECT o.stream_id, o.partition, GREATEST(o.high_watermark - o.next_offset, 0) \
         FROM stream_offsets o JOIN streams s ON s.id = o.stream_id \
         WHERE s.workspace_id = $1 AND s.source_id = $2 AND o.workspace_id = $1 \
           AND o.high_watermark IS NOT NULL \
         ORDER BY o.stream_id, o.partition",
    )
    .bind(workspace.as_uuid())
    .bind(source_id)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    let mut lags: BTreeMap<Uuid, Vec<PartitionLag>> = BTreeMap::new();
    for (stream, partition, lag) in rows {
        lags.entry(stream)
            .or_default()
            .push(PartitionLag { partition, lag });
    }
    Ok(lags)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::sync_runs::testing::{fixture, workspace};
    use crate::{cancel_run, find_stream_by_id};

    fn lease(fx: &crate::sync_runs::testing::Fixture) -> Lease {
        Lease {
            job_id: fx.job.id,
            token: fx.job.lease_token,
        }
    }

    fn offset(partition: i32, next: i64, end: Option<i64>) -> PartitionOffset {
        PartitionOffset {
            partition,
            next,
            end,
        }
    }

    async fn stream_state(pool: &PgPool, fx: &crate::sync_runs::testing::Fixture) -> (bool, bool) {
        let stream = find_stream_by_id(pool, fx.ws, fx.source_id, fx.stream_id)
            .await
            .unwrap()
            .unwrap();
        (stream.last_success_at.is_some(), stream.last_run_failed)
    }

    #[test]
    fn a_partition_offset_is_plain_numbers() {
        let offset = offset(3, 12, Some(40));

        assert_eq!(
            (offset.partition, offset.next, offset.end),
            (3, 12, Some(40))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_report_stores_the_watermarks_and_decides_the_outcome_by_the_bound(pool: PgPool) {
        let fx = fixture(&pool, "one", "stream").await;

        // 5 + 0 messages behind, bound 10: within.
        let within = report_stream_lag(
            &pool,
            fx.ws,
            fx.stream_id,
            fx.run.id,
            lease(&fx),
            &[offset(0, 5, Some(10)), offset(1, 7, Some(7))],
            10,
        )
        .await
        .unwrap();
        assert_eq!(
            within,
            LagReport::Reported {
                lag: 5,
                within_bound: true
            }
        );
        assert_eq!(stream_state(&pool, &fx).await, (true, false));
        let stored = load_offsets(&pool, fx.ws, fx.stream_id).await.unwrap();
        assert_eq!(
            stored,
            [offset(0, 5, Some(10)), offset(1, 7, Some(7))],
            "a partition without a row gets one at its position"
        );

        // The log grew by 20 in partition 0: 25 behind, bound 10: failing. The stored next
        // offset stays where a batch commit left it.
        let over = report_stream_lag(
            &pool,
            fx.ws,
            fx.stream_id,
            fx.run.id,
            lease(&fx),
            &[offset(0, 999, Some(30))],
            10,
        )
        .await
        .unwrap();
        assert_eq!(
            over,
            LagReport::Reported {
                lag: 25,
                within_bound: false
            }
        );
        assert_eq!(stream_state(&pool, &fx).await, (true, true));
        assert_eq!(
            load_offsets(&pool, fx.ws, fx.stream_id).await.unwrap()[0],
            offset(0, 5, Some(30))
        );

        // Back within the bound: success again.
        let back = report_stream_lag(
            &pool,
            fx.ws,
            fx.stream_id,
            fx.run.id,
            lease(&fx),
            &[offset(0, 5, Some(8))],
            10,
        )
        .await
        .unwrap();
        assert!(matches!(
            back,
            LagReport::Reported {
                within_bound: true,
                ..
            }
        ));
        assert_eq!(stream_state(&pool, &fx).await, (true, false));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_report_from_a_lost_lease_or_a_cancelled_run_writes_nothing(pool: PgPool) {
        let fx = fixture(&pool, "one", "stream").await;
        let wrong = Lease {
            job_id: fx.job.id,
            token: Uuid::now_v7(),
        };

        let lost = report_stream_lag(
            &pool,
            fx.ws,
            fx.stream_id,
            fx.run.id,
            wrong,
            &[offset(0, 1, Some(9))],
            0,
        )
        .await
        .unwrap();
        assert_eq!(lost, LagReport::LeaseLost);

        cancel_run(
            &mut pool.acquire().await.unwrap(),
            fx.ws,
            fx.source_id,
            fx.run.id,
        )
        .await
        .unwrap();
        let cancelled = report_stream_lag(
            &pool,
            fx.ws,
            fx.stream_id,
            fx.run.id,
            lease(&fx),
            &[offset(0, 1, Some(9))],
            0,
        )
        .await
        .unwrap();

        assert_eq!(cancelled, LagReport::Cancelled);
        assert!(
            load_offsets(&pool, fx.ws, fx.stream_id)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(stream_state(&pool, &fx).await, (false, false));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn lag_by_stream_counts_only_watermarked_partitions_of_the_source(pool: PgPool) {
        let fx = fixture(&pool, "one", "stream").await;
        let other = workspace(&pool, "two").await;
        report_stream_lag(
            &pool,
            fx.ws,
            fx.stream_id,
            fx.run.id,
            lease(&fx),
            &[offset(0, 2, Some(9)), offset(1, 4, Some(4))],
            100,
        )
        .await
        .unwrap();

        let lags = stream_lag(&pool, fx.ws, fx.source_id).await.unwrap();
        let foreign = stream_lag(&pool, other, fx.source_id).await.unwrap();

        assert_eq!(
            lags[&fx.stream_id],
            [
                PartitionLag {
                    partition: 0,
                    lag: 7
                },
                PartitionLag {
                    partition: 1,
                    lag: 0
                }
            ]
        );
        assert!(foreign.is_empty(), "another workspace sees nothing");
        assert!(
            load_offsets(&pool, other, fx.stream_id)
                .await
                .unwrap()
                .is_empty(),
            "another workspace reads no offsets of the stream"
        );
    }
}
