//! The size of the decision log: what the partitions of `decision_records` take on disk, for the
//! `zirv_flow_decision_log_bytes` and `zirv_flow_decision_log_partitions` gauges.

use sqlx::PgPool;

use crate::StoreError;

/// What the partitions of the decision log hold on disk.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DecisionLogSize {
    /// The sum of `pg_total_relation_size` (table, indexes and TOAST) over the partitions.
    pub bytes: u64,
    /// The partitions, the DEFAULT one included.
    pub partitions: u64,
}

/// Reads the size of the decision log.
pub async fn decision_log_size(pool: &PgPool) -> Result<DecisionLogSize, StoreError> {
    let (bytes, partitions): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(sum(pg_total_relation_size(c.oid)), 0)::bigint, count(*)::bigint \
         FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
         WHERE i.inhparent = 'decision_records'::regclass",
    )
    .fetch_one(pool)
    .await
    .map_err(StoreError::query)?;
    Ok(DecisionLogSize {
        bytes: u64::try_from(bytes).unwrap_or(0),
        partitions: u64::try_from(partitions).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use time::Date;
    use uuid::Uuid;

    use super::*;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_size_is_the_sum_of_pg_total_relation_size_over_the_partitions(pool: PgPool) {
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = 'decision_records'::regclass",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let mut expected = 0_i64;
        for name in &names {
            let size: i64 = sqlx::query_scalar("SELECT pg_total_relation_size($1::regclass)")
                .bind(name)
                .fetch_one(&pool)
                .await
                .unwrap();
            expected += size;
        }

        let size = decision_log_size(&pool).await.unwrap();

        assert_eq!(size.bytes, u64::try_from(expected).unwrap());
        assert_eq!(size.partitions, names.len() as u64);
        assert!(size.partitions >= 3, "the default, this month and next");
    }

    /// A decision shape as the runtime writes it: what the record carries beyond its keys.
    struct Shape {
        name: &'static str,
        action: &'static str,
        outcome: &'static str,
        reason: &'static str,
        subjects: &'static str,
        job: bool,
    }

    const SHAPES: [Shape; 3] = [
        Shape {
            name: "ingest",
            action: "event.applied",
            outcome: "applied",
            reason: "NULL",
            subjects: "jsonb_build_object('person', gen_random_uuid())",
            job: false,
        },
        Shape {
            name: "flow",
            action: "flow.step",
            outcome: "done",
            reason: "NULL",
            subjects: "jsonb_build_object('flow', gen_random_uuid(), \
                'enrollment', gen_random_uuid(), 'person', gen_random_uuid(), \
                'version', 3, 'node', 'send-welcome')",
            job: true,
        },
        Shape {
            name: "send",
            action: "message.send",
            outcome: "sent",
            reason: "'gates_passed'",
            subjects: "jsonb_build_object('message', gen_random_uuid(), \
                'enrollment', gen_random_uuid(), 'template', gen_random_uuid(), \
                'destination', gen_random_uuid(), 'person', gen_random_uuid())",
            job: true,
        },
    ];

    /// Loads `count` records of `shape` into the partition of `month` and returns the bytes the
    /// partition takes with all its indexes and without the three secondary ones.
    async fn measure(
        pool: &PgPool,
        ws: Uuid,
        shape: &Shape,
        month: Date,
        count: i64,
    ) -> (i64, i64) {
        let partition = format!(
            "decision_records_y{:04}m{:02}",
            month.year(),
            u8::from(month.month())
        );
        let insert = format!(
            "INSERT INTO decision_records \
             (id, workspace_id, trace_id, recorded_at, action, outcome, reason, cause_kind, \
              cause_id, subject_ids, job_id, attempt) \
             SELECT gen_random_uuid(), $1, gen_random_uuid(), \
                    $2::timestamptz + g * interval '1 second', '{}', '{}', {}, 'event', \
                    gen_random_uuid()::text, {}, {}, {} \
             FROM generate_series(1, $3) g",
            shape.action,
            shape.outcome,
            shape.reason,
            shape.subjects,
            if shape.job {
                "gen_random_uuid()"
            } else {
                "NULL::uuid"
            },
            if shape.job { "1" } else { "NULL::int" },
        );
        let start = month.midnight().assume_utc();
        sqlx::query(sqlx::AssertSqlSafe(insert))
            .bind(ws)
            .bind(start)
            .bind(count)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!("VACUUM ANALYZE {partition}")))
            .execute(pool)
            .await
            .unwrap();
        let total: i64 = sqlx::query_scalar("SELECT pg_total_relation_size($1::regclass)")
            .bind(&partition)
            .fetch_one(pool)
            .await
            .unwrap();
        let secondary: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(pg_relation_size(c.oid)), 0)::bigint \
             FROM pg_index x JOIN pg_class c ON c.oid = x.indexrelid \
             JOIN pg_inherits h ON h.inhrelid = c.oid \
             WHERE x.indrelid = $1::regclass AND h.inhparent IN ( \
                'decision_records_trace_idx'::regclass, \
                'decision_records_person_idx'::regclass, \
                'decision_records_enrollment_idx'::regclass)",
        )
        .bind(&partition)
        .fetch_one(pool)
        .await
        .unwrap();
        (total, total - secondary)
    }

    /// The measurement for the benchmark report (architecture 12.4, OQ-28): bytes per record of
    /// each decision shape with and without the three secondary indexes, and the size of a day at
    /// 1,000 events/s with one record of that shape per event. `ZIRV_FLOW_BENCH_DECISION_SIZE`
    /// sets the records per shape; the default is the smoke size.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn bytes_per_record_of_each_decision_shape_with_and_without_the_secondary_indexes(
        pool: PgPool,
    ) {
        use crate::decisions::ensure_decision_partitions;
        use time::macros::date;
        use zirv_flow_core::{Slug, WorkspaceName};

        let count: i64 = std::env::var("ZIRV_FLOW_BENCH_DECISION_SIZE")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(2_000);
        let ws = crate::create_workspace(
            &pool,
            &WorkspaceName::parse("ours").unwrap(),
            &Slug::parse("ours").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let months = [
            date!(2030 - 01 - 01),
            date!(2030 - 03 - 01),
            date!(2030 - 05 - 01),
        ];
        for month in months {
            ensure_decision_partitions(&pool, month).await.unwrap();
        }
        println!(
            "\n{:<8} {:>9} {:>14} {:>16} {:>16} {:>16}",
            "shape",
            "records",
            "bytes/record",
            "bytes/rec no idx",
            "GB/day at 1k/s",
            "GB at 395 days"
        );
        for (shape, month) in SHAPES.iter().zip(months) {
            let (with, without) = measure(&pool, ws.as_uuid(), shape, month, count).await;
            let per_record = with as f64 / count as f64;
            let per_day = per_record * 1_000.0 * 86_400.0 / 1e9;
            println!(
                "{:<8} {:>9} {:>14.1} {:>16.1} {:>16.1} {:>16.0}",
                shape.name,
                count,
                per_record,
                without as f64 / count as f64,
                per_day,
                per_day * 395.0,
            );
            assert!(without > 0 && without < with, "{}", shape.name);
        }
    }
}
