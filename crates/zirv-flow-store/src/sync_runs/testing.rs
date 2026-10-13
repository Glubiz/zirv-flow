//! Fixtures for the tests of sync runs: a workspace with a source, a mapped stream, a queued run
//! and the claimed job that works it.

use std::time::Duration;

use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{Slug, TraceContext, WorkspaceId, WorkspaceName};

use super::batch::{SyncBatch, SyncUpsert};
use super::job::run_trace;
use super::model::{Lease, RunKind, RunTrigger, SyncRun, finish_run, queue_run, start_run};
use crate::{
    ClaimedJob, DiscoveredStream, NewSource, ProfileRecord, begin, claim_job, commit,
    configure_stream, create_source, create_workspace, set_stream_mapping,
    upsert_discovered_streams,
};

pub(crate) struct Fixture {
    pub(crate) ws: WorkspaceId,
    pub(crate) source_id: Uuid,
    pub(crate) stream_id: Uuid,
    pub(crate) run: SyncRun,
    pub(crate) job: ClaimedJob,
    pub(crate) trace: TraceContext,
}

pub(crate) async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
    let name = WorkspaceName::parse(slug).unwrap();
    let slug = Slug::parse(slug).unwrap();
    create_workspace(pool, &name, &slug).await.unwrap().id
}

pub(crate) fn mapping() -> Value {
    json!({
        "key_column": "id",
        "columns": [{"column": "email", "target": {"type": "identifier", "kind": "email"}}]
    })
}

/// A source with one enabled stream `contacts` in `sync_mode` and a mapping. Nothing is queued.
pub(crate) async fn source_with_stream(
    pool: &PgPool,
    ws: WorkspaceId,
    name: &str,
    sync_mode: &str,
) -> (Uuid, Uuid) {
    let config = json!({});
    let source = create_source(
        pool,
        ws,
        &NewSource {
            id: Uuid::now_v7(),
            kind: "fake",
            name,
            config: &config,
            credential_secret_id: None,
            credential_fields: &[],
            schedule: None,
            created_by: None,
        },
    )
    .await
    .unwrap();
    let cursor = (sync_mode == "incremental").then_some("updated_at");
    let descriptor = json!({});
    let mut tx = begin(pool).await.unwrap();
    upsert_discovered_streams(
        &mut tx,
        ws,
        source.id,
        &[DiscoveredStream {
            name: "contacts",
            sync_mode,
            cursor_field: cursor,
            primary_key: &[],
            descriptor: &descriptor,
        }],
    )
    .await
    .unwrap();
    commit(tx).await.unwrap();
    let stream = configure_stream(
        &mut pool.acquire().await.unwrap(),
        ws,
        source.id,
        "contacts",
        &crate::StreamSettings {
            reconcile_schedule: None,
            sweep_limit: 0.5,
            enabled: true,
            sync_mode,
            cursor_field: cursor,
            primary_key: &[],
            staleness_threshold_seconds: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    set_stream_mapping(pool, ws, source.id, "contacts", &mapping())
        .await
        .unwrap()
        .unwrap();
    (source.id, stream.id)
}

/// Queues a run of the stream, claims its job and starts the run, as a worker would.
pub(crate) async fn started_run(
    pool: &PgPool,
    ws: WorkspaceId,
    source_id: Uuid,
    stream_id: Uuid,
) -> Fixture {
    let mut connection = pool.acquire().await.unwrap();
    let queued = queue_run(
        &mut connection,
        ws,
        source_id,
        stream_id,
        RunKind::Sync,
        RunTrigger::Manual,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    drop(connection);
    let job = claim_job(
        pool,
        "w",
        &["sync.run", "kafka.consume"],
        Duration::from_secs(30),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let run = start_run(pool, ws, queued.id).await.unwrap().unwrap();
    let trace = run_trace(&run);
    Fixture {
        ws,
        source_id,
        stream_id,
        run,
        job,
        trace,
    }
}

/// Claims the next `sync.run` job and returns the lease it gives.
pub(crate) async fn claim_lease(pool: &PgPool) -> Lease {
    let job = claim_job(
        pool,
        "w",
        &["sync.run", "kafka.consume"],
        Duration::from_secs(30),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    Lease {
        job_id: job.id,
        token: job.lease_token,
    }
}

pub(crate) async fn fixture(pool: &PgPool, slug: &str, sync_mode: &str) -> Fixture {
    let ws = workspace(pool, slug).await;
    let (source_id, stream_id) = source_with_stream(pool, ws, "crm", sync_mode).await;
    started_run(pool, ws, source_id, stream_id).await
}

pub(crate) fn upsert(fixture: &Fixture, key: &str, email: &str) -> SyncUpsert {
    SyncUpsert {
        record: ProfileRecord {
            source: fixture.source_id.to_string(),
            stream: "contacts".into(),
            record_key: key.into(),
            identifiers: vec![("email".into(), email.into())],
            traits: serde_json::Map::new(),
            observed_at: OffsetDateTime::now_utc(),
            trace: fixture.trace.clone(),
        },
        sample: json!({"properties": {}, "masked": []}),
        lists: Vec::new(),
        event: None,
        occurrence: None,
    }
}

/// A batch of the fixture's run with the fixture's own lease.
pub(crate) fn batch<'a>(
    fixture: &'a Fixture,
    source: &'a str,
    upserts: &'a [SyncUpsert],
    checkpoint: &'a Value,
) -> SyncBatch<'a> {
    SyncBatch {
        workspace: fixture.ws,
        run_id: fixture.run.id,
        stream_id: fixture.stream_id,
        source,
        stream: "contacts",
        job_id: fixture.job.id,
        lease_token: fixture.job.lease_token,
        attempt: fixture.job.attempts,
        trace: &fixture.trace,
        received_at: OffsetDateTime::now_utc(),
        redaction_key: None,
        upserts,
        deleted: &[],
        seen: &[],
        rejected: &[],
        duplicates: 0,
        offsets: &[],
        checkpoint,
        incremental: false,
        singly: false,
    }
}

/// Ends the fixture's run as succeeded and starts the next run of the same stream with its own
/// claimed job, as the next firing of a schedule would.
pub(crate) async fn next_run(pool: &PgPool, previous: &Fixture) -> Fixture {
    let ended = finish_run(
        pool,
        previous.ws,
        previous.run.id,
        Lease {
            job_id: previous.job.id,
            token: previous.job.lease_token,
        },
    )
    .await
    .unwrap();
    assert_eq!(ended, super::model::RunEnd::Ended);
    started_run(pool, previous.ws, previous.source_id, previous.stream_id).await
}

/// Commits `n` records `k0..` of the fixture's run in one batch, and returns their keys.
pub(crate) async fn commit_records(pool: &PgPool, fixture: &Fixture, n: usize) -> Vec<String> {
    let upserts: Vec<SyncUpsert> = (0..n)
        .map(|i| upsert(fixture, &format!("k{i}"), &format!("a{i}@example.com")))
        .collect();
    let source = fixture.source_id.to_string();
    let checkpoint = json!({"offset": n});
    let outcome =
        super::batch::commit_sync_batch(pool, &batch(fixture, &source, &upserts, &checkpoint))
            .await
            .unwrap();
    assert!(matches!(outcome, super::batch::BatchOutcome::Committed(_)));
    (0..n).map(|i| format!("k{i}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixture_mapping_has_an_identifier_column() {
        assert_eq!(mapping()["columns"][0]["column"], "email");
    }
}
