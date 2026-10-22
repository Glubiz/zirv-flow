//! The record of each delivery attempt, and the per-destination report the leader publishes as
//! metrics.
//!
//! An attempt names the destination, the item delivered (a message, a forwarding item or a sync
//! item, by id), the attempt number, how it ended, the class of the answer's HTTP status and how
//! long it took. It holds no person id and no address, so deleting a person leaves it as it is.

use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// What kind of item an attempt delivered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubjectKind {
    Message,
    Forwarding,
    Sync,
}

impl SubjectKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Forwarding => "forwarding",
            Self::Sync => "sync",
        }
    }
}

/// How an attempt ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptOutcome {
    Delivered,
    /// Failed; a later attempt may succeed.
    Retryable,
    /// Failed; no retry can succeed.
    Terminal,
    /// The destination asked for a slower rate.
    Throttled,
}

impl AttemptOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Retryable => "retryable",
            Self::Terminal => "terminal",
            Self::Throttled => "throttled",
        }
    }
}

/// The class of the HTTP status an attempt got back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusClass {
    /// The call produced no answer.
    None,
    Success,
    Redirect,
    ClientError,
    ServerError,
}

impl StatusClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Success => "2xx",
            Self::Redirect => "3xx",
            Self::ClientError => "4xx",
            Self::ServerError => "5xx",
        }
    }

    /// The class of `status`; any status outside 200 to 599 is `None`.
    pub const fn of(status: Option<u16>) -> Self {
        match status {
            Some(200..=299) => Self::Success,
            Some(300..=399) => Self::Redirect,
            Some(400..=499) => Self::ClientError,
            Some(500..=599) => Self::ServerError,
            _ => Self::None,
        }
    }
}

/// An attempt to record.
#[derive(Clone, Debug)]
pub struct NewDeliveryAttempt {
    pub destination_id: Uuid,
    pub subject_kind: SubjectKind,
    pub subject_id: Uuid,
    /// 1 for the first attempt at this item.
    pub attempt: i32,
    pub outcome: AttemptOutcome,
    pub status_class: StatusClass,
    pub duration_ms: i32,
}

/// A recorded attempt. No person and no address.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct DeliveryAttempt {
    pub id: Uuid,
    pub destination_id: Uuid,
    pub subject_kind: String,
    pub subject_id: Uuid,
    pub attempt: i32,
    pub outcome: String,
    pub status_class: String,
    pub duration_ms: i32,
    pub attempted_at: OffsetDateTime,
}

/// Records one attempt. The same item, destination and attempt number twice is a conflict.
///
/// This does not count the attempt in the metrics: the caller may still roll back or retry the
/// transaction. After the transaction that holds this row commits, call
/// [`count_delivery_attempt`] once.
pub async fn record_delivery_attempt<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    new: &NewDeliveryAttempt,
) -> Result<DeliveryAttempt, StoreError> {
    sqlx::query_as(
        "INSERT INTO delivery_attempts (id, workspace_id, destination_id, subject_kind, \
            subject_id, attempt, outcome, status_class, duration_ms) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         RETURNING id, destination_id, subject_kind, subject_id, attempt, outcome, status_class, \
            duration_ms, attempted_at",
    )
    .bind(Uuid::now_v7())
    .bind(workspace.as_uuid())
    .bind(new.destination_id)
    .bind(new.subject_kind.as_str())
    .bind(new.subject_id)
    .bind(new.attempt)
    .bind(new.outcome.as_str())
    .bind(new.status_class.as_str())
    .bind(new.duration_ms)
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)
}

/// Counts a recorded attempt in this process:
/// `zirv_flow_destination_attempts_total{workspace,destination}` and
/// `zirv_flow_destination_outcomes_total{workspace,destination,outcome}`. Ids only, never a name,
/// a host or an address.
///
/// Call it once per attempt, **after** the transaction that holds the row from
/// [`record_delivery_attempt`] has committed, so a rolled-back insert is never counted and a
/// committed retry of the transaction is counted once. The send path (#220) calls it; the leader
/// never scans the table.
pub fn count_delivery_attempt(workspace: WorkspaceId, attempt: &NewDeliveryAttempt) {
    let workspace = workspace.as_uuid().to_string();
    let destination = attempt.destination_id.to_string();
    metrics::counter!(
        "zirv_flow_destination_attempts_total",
        "workspace" => workspace.clone(),
        "destination" => destination.clone(),
    )
    .increment(1);
    metrics::counter!(
        "zirv_flow_destination_outcomes_total",
        "workspace" => workspace,
        "destination" => destination,
        "outcome" => attempt.outcome.as_str(),
    )
    .increment(1);
}

/// Counts an attempt that was made but could not be recorded:
/// `zirv_flow_delivery_attempt_record_failures_total{workspace,destination}`. Ids only. The send
/// is not repeated for it, so this counter is the only trace of the missing row.
pub fn count_delivery_attempt_record_failure(workspace: WorkspaceId, destination: Uuid) {
    metrics::counter!(
        "zirv_flow_delivery_attempt_record_failures_total",
        "workspace" => workspace.as_uuid().to_string(),
        "destination" => destination.to_string(),
    )
    .increment(1);
}

/// The newest attempts of a destination, newest first, at most `limit`.
pub async fn list_delivery_attempts<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    destination: Uuid,
    limit: i64,
) -> Result<Vec<DeliveryAttempt>, StoreError> {
    sqlx::query_as(
        "SELECT id, destination_id, subject_kind, subject_id, attempt, outcome, status_class, \
            duration_ms, attempted_at \
         FROM delivery_attempts WHERE workspace_id = $1 AND destination_id = $2 \
         ORDER BY attempted_at DESC, id DESC LIMIT $3",
    )
    .bind(workspace.as_uuid())
    .bind(destination)
    .bind(limit)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// What the leader publishes for one live destination: its queue gauges.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DestinationStat {
    pub workspace_id: Uuid,
    pub destination_id: Uuid,
    /// Items waiting for delivery. 0 until the send pipeline (#220) has a queue to count.
    pub queue_depth: i64,
    /// Age in seconds of the oldest waiting item; 0 when none waits.
    pub oldest_age_seconds: i64,
}

/// One row per live destination with its queue gauges. The attempt and outcome counters are not
/// here: they are counted in-process where an attempt is recorded, so this never scans
/// `delivery_attempts`.
///
/// There is no delivery queue yet, so `queue_depth` and `oldest_age_seconds` are 0; the send
/// pipeline (#220) replaces that part of the query with the count and the age of its waiting
/// items.
pub async fn destination_report<'e>(
    executor: impl PgExecutor<'e>,
) -> Result<Vec<DestinationStat>, StoreError> {
    let destinations: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT workspace_id, id FROM destinations WHERE archived_at IS NULL ORDER BY id",
    )
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(destinations
        .into_iter()
        .map(|(workspace_id, destination_id)| DestinationStat {
            workspace_id,
            destination_id,
            queue_depth: 0,
            oldest_age_seconds: 0,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::{NewDestination, create_destination, create_workspace};

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    #[test]
    fn a_status_is_classified_and_every_label_is_stable() {
        assert_eq!(StatusClass::of(Some(202)), StatusClass::Success);
        assert_eq!(StatusClass::of(Some(302)), StatusClass::Redirect);
        assert_eq!(StatusClass::of(Some(429)), StatusClass::ClientError);
        assert_eq!(StatusClass::of(Some(503)), StatusClass::ServerError);
        assert_eq!(StatusClass::of(Some(99)), StatusClass::None);
        assert_eq!(StatusClass::of(None), StatusClass::None);
        assert_eq!(
            [
                StatusClass::None,
                StatusClass::Success,
                StatusClass::Redirect,
                StatusClass::ClientError,
                StatusClass::ServerError
            ]
            .map(StatusClass::as_str),
            ["none", "2xx", "3xx", "4xx", "5xx"]
        );
        assert_eq!(
            [
                AttemptOutcome::Delivered,
                AttemptOutcome::Retryable,
                AttemptOutcome::Terminal,
                AttemptOutcome::Throttled
            ]
            .map(AttemptOutcome::as_str),
            ["delivered", "retryable", "terminal", "throttled"]
        );
        assert_eq!(
            [
                SubjectKind::Message,
                SubjectKind::Forwarding,
                SubjectKind::Sync
            ]
            .map(SubjectKind::as_str),
            ["message", "forwarding", "sync"]
        );
    }

    async fn destination(pool: &PgPool, ws: WorkspaceId, name: &str) -> Uuid {
        let config = json!({});
        create_destination(
            pool,
            ws,
            &NewDestination {
                id: Uuid::now_v7(),
                kind: "fake",
                name,
                config: &config,
                credential_secret_id: None,
                credential_fields: &[],
                rate: 10,
                burst: 10,
                created_by: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    fn attempt(
        destination: Uuid,
        subject: Uuid,
        number: i32,
        outcome: AttemptOutcome,
    ) -> NewDeliveryAttempt {
        NewDeliveryAttempt {
            destination_id: destination,
            subject_kind: SubjectKind::Message,
            subject_id: subject,
            attempt: number,
            outcome,
            status_class: StatusClass::of(Some(503)),
            duration_ms: 12,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn attempts_are_recorded_listed_newest_first_and_a_repeat_number_conflicts(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = destination(&pool, ws, "Mail").await;
        let subject = Uuid::now_v7();

        let first = record_delivery_attempt(
            &pool,
            ws,
            &attempt(id, subject, 1, AttemptOutcome::Retryable),
        )
        .await
        .unwrap();
        let second = record_delivery_attempt(
            &pool,
            ws,
            &attempt(id, subject, 2, AttemptOutcome::Delivered),
        )
        .await
        .unwrap();
        let repeat = record_delivery_attempt(
            &pool,
            ws,
            &attempt(id, subject, 2, AttemptOutcome::Terminal),
        )
        .await
        .unwrap_err();

        assert!(repeat.is_conflict());
        assert_eq!(first.status_class, "5xx");
        assert_eq!(second.outcome, "delivered");
        let listed = list_delivery_attempts(&pool, ws, id, 10).await.unwrap();
        assert_eq!(listed, vec![second, first]);
        assert_eq!(
            list_delivery_attempts(&pool, ws, id, 1)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_report_lists_live_destinations_with_zero_queue_gauges(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let live = destination(&pool, ws, "Live").await;
        let gone = destination(&pool, ws, "Gone").await;
        crate::archive_destination(&pool, ws, gone).await.unwrap();

        let report = destination_report(&pool).await.unwrap();

        assert_eq!(
            report,
            vec![DestinationStat {
                workspace_id: ws.as_uuid(),
                destination_id: live,
                queue_depth: 0,
                oldest_age_seconds: 0,
            }]
        );
    }

    #[test]
    fn a_failed_recording_is_counted_by_ids_only() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _installed = ::metrics::set_default_local_recorder(&recorder);
        let ws = WorkspaceId::new();
        let id = Uuid::from_u128(9);

        count_delivery_attempt_record_failure(ws, id);

        let output = handle.render();
        let want = format!(
            "zirv_flow_delivery_attempt_record_failures_total{{workspace=\"{}\",destination=\"{id}\"}} 1",
            ws.as_uuid()
        );
        assert!(output.contains(&want), "{output}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn counting_a_committed_attempt_counts_it_by_destination_and_outcome_with_ids_only(
        pool: PgPool,
    ) {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _installed = ::metrics::set_default_local_recorder(&recorder);
        let ws = workspace(&pool, "one").await;
        let id = destination(&pool, ws, "secret-mail-name").await;

        for (number, outcome) in [
            (1, AttemptOutcome::Retryable),
            (2, AttemptOutcome::Retryable),
            (3, AttemptOutcome::Delivered),
        ] {
            let new = attempt(id, Uuid::nil(), number, outcome);
            record_delivery_attempt(&pool, ws, &new).await.unwrap();
            count_delivery_attempt(ws, &new);
        }
        // A refused (conflicting) record is never counted: the caller has nothing to count.
        record_delivery_attempt(
            &pool,
            ws,
            &attempt(id, Uuid::nil(), 3, AttemptOutcome::Terminal),
        )
        .await
        .unwrap_err();

        let output = handle.render();
        let labels = format!("workspace=\"{}\",destination=\"{id}\"", ws.as_uuid());
        assert!(
            output.contains(&format!(
                "zirv_flow_destination_attempts_total{{{labels}}} 3"
            )),
            "{output}"
        );
        for (outcome, count) in [("retryable", 2), ("delivered", 1)] {
            assert!(
                output.contains(&format!(
                    "zirv_flow_destination_outcomes_total{{{labels},outcome=\"{outcome}\"}} {count}"
                )),
                "{output}"
            );
        }
        assert!(!output.contains("terminal"), "{output}");
        assert!(!output.contains("secret-mail-name"), "{output}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn recording_an_attempt_alone_counts_nothing(pool: PgPool) {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _installed = ::metrics::set_default_local_recorder(&recorder);
        let ws = workspace(&pool, "one").await;
        let id = destination(&pool, ws, "Mail").await;

        // Recorded, then rolled back: nothing was delivered as far as the database knows.
        let mut tx = crate::begin(&pool).await.unwrap();
        record_delivery_attempt(
            &mut *tx,
            ws,
            &attempt(id, Uuid::nil(), 1, AttemptOutcome::Delivered),
        )
        .await
        .unwrap();
        drop(tx);
        record_delivery_attempt(
            &pool,
            ws,
            &attempt(id, Uuid::nil(), 1, AttemptOutcome::Delivered),
        )
        .await
        .unwrap();

        let output = handle.render();
        assert!(
            !output.contains("zirv_flow_destination_attempts_total"),
            "{output}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_index_of_the_new_tables_leads_with_the_workspace_except_the_primary_keys(
        pool: PgPool,
    ) {
        let leading: Vec<(String, String)> = sqlx::query_as(
            "SELECT t.relname::text, a.attname::text \
             FROM pg_index x \
             JOIN pg_class t ON t.oid = x.indrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace AND n.nspname = current_schema() \
             JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = x.indkey[0] \
             WHERE t.relname IN ('destinations', 'destination_rate_buckets', 'delivery_attempts') \
               AND NOT x.indisprimary \
             ORDER BY 1, 2",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(leading.len() >= 5, "{leading:?}");
        for (table, column) in &leading {
            assert_eq!(
                column, "workspace_id",
                "an index of {table} leads with {column}"
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_new_tables_hold_no_person_or_address_column_for_the_erasure_scan(pool: PgPool) {
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text || '.' || column_name::text \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() \
               AND table_name IN ('destinations', 'destination_rate_buckets', 'delivery_attempts') \
               AND (column_name = 'person_id' OR column_name LIKE '%\\_person\\_id' \
                    OR column_name ~ '(email|address|phone|recipient|\\mto\\M)') \
             ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(columns.is_empty(), "{columns:?}");
        // The scan the privacy tests run names person columns the same way.
        let scan: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND data_type = 'uuid' \
               AND (column_name = 'person_id' OR column_name LIKE '%\\_person\\_id') \
               AND table_name IN ('destinations', 'destination_rate_buckets', 'delivery_attempts')",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(scan.is_empty(), "{scan:?}");
    }
}
