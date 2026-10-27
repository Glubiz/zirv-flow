//! `privacy.erase`: carries one erasure request out (architecture section 16.3).
//!
//! The payload is `{workspace_id, request_id}`. The whole erasure runs in the job's own
//! transaction, so a failure leaves nothing done and the request pending for the retry; once
//! the attempts run out the request reads as failed. It needs the identifier key of the worker's master key and fails without one.
//!
//! **Erase calls (#227).** The job lists the workspace's destinations and decides, with the
//! destination registry, which declare erase: a built-in type through `Capabilities.erase`, keyed
//! on the person id; a provider destination when its manifest has `erase_profile`, keyed on the
//! identity or trait the manifest names. The store reads no manifest, so it gets these as
//! targets, keeps the ones that handled the person and enqueues one `destination.erase` job each,
//! inside the erasure transaction.

use serde_json::Value;
use tracing::warn;
use uuid::Uuid;
use zirv_flow_connectors::destinations::{DestinationSpec, Registry, provider_erase_key_source};
use zirv_flow_connectors::manifest::KeySource;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{Destination, EraseOutcome, EraseTarget, ExternalKey, list_destinations};

use crate::{JobContext, JobError, JobKind};

pub(crate) struct PrivacyErase;

impl JobKind for PrivacyErase {
    const KIND: &'static str = zirv_flow_store::ERASE_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let (workspace, request) = parse(&payload)?;
        if ctx.workspace_id() != Some(workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let key = ctx
            .identifier_key()
            .ok_or(JobError::new("the worker has no master key"))?;
        let trace = ctx.trace().clone();
        let destinations = list_destinations(ctx.conn(), workspace, true).await?;
        let targets = erase_targets(&destinations);
        match zirv_flow_store::erase_person(ctx.conn(), workspace, request, &key, &targets, &trace)
            .await?
        {
            EraseOutcome::Completed { .. } | EraseOutcome::AlreadyCompleted => {
                write_ledger_file(ctx, workspace, request).await
            }
            EraseOutcome::NoSuchRequest => Err(JobError::new("the erasure request does not exist")),
        }
    }
}

/// Copies the completed ledger entry to `privacy.ledger_dir`, when it is set, before the job
/// returns. A failure fails the job, so its transaction rolls back and the request stays pending
/// for the retry: an erasure is never complete without its file.
async fn write_ledger_file(
    ctx: &mut JobContext<'_>,
    workspace: WorkspaceId,
    request: Uuid,
) -> Result<(), JobError> {
    let Some(dir) = ctx.ledger_dir() else {
        return Ok(());
    };
    let entry = zirv_flow_store::find_ledger_entry(ctx.conn(), workspace, request)
        .await?
        .ok_or(JobError::new("the completed erasure has no ledger entry"))?;
    let written = tokio::task::spawn_blocking(move || {
        zirv_flow_store::write_ledger_file(dir.as_path(), &entry)
    })
    .await
    .map_err(|_| JobError::new("the erasure ledger file write was interrupted"))?;
    match written {
        Ok(_) => Ok(()),
        Err(error @ zirv_flow_store::LedgerFileError::Conflict { .. }) => {
            warn!(%error, "privacy.ledger_dir holds a different file for this erasure");
            Err(JobError::new(
                "an erasure ledger file with other content exists",
            ))
        }
        Err(error) => {
            warn!(%error, "could not write the erasure ledger file to privacy.ledger_dir");
            Err(JobError::new(
                "the erasure ledger file could not be written",
            ))
        }
    }
}

/// Where a destination of this type and configuration takes its external key from, when it
/// declares erase. A built-in type declares it through `Capabilities.erase` and keys on the person
/// id; a provider destination declares it when its manifest has `erase_profile`, and the manifest
/// names the identity or trait the key is.
fn erase_key(spec: &DestinationSpec, config: &Value) -> Option<ExternalKey> {
    if spec.capabilities.channels.contains(&"profile_sync") {
        return provider_erase_key_source(config).map(|source| match source {
            KeySource::Identity(kind) => ExternalKey::Identity(kind),
            KeySource::Trait(key) => ExternalKey::Trait(key),
        });
    }
    spec.capabilities.erase.then_some(ExternalKey::PersonId)
}

/// Whether a destination of this type and configuration declares the erase capability.
pub(super) fn declares_erase(spec: &DestinationSpec, config: &Value) -> bool {
    spec.abilities(config).erase
}

/// The destinations that declare erase, with where each one's key comes from. The store keeps the
/// ones that handled the person.
fn erase_targets(destinations: &[Destination]) -> Vec<EraseTarget> {
    let registry = Registry::built_in();
    destinations
        .iter()
        .filter_map(|destination| {
            let driver = registry.get(&destination.kind)?;
            Some(EraseTarget {
                destination: destination.id,
                key: erase_key(driver.spec(), &destination.config)?,
            })
        })
        .collect()
}

fn parse(payload: &Value) -> Result<(WorkspaceId, Uuid), JobError> {
    super::workspace_and_id(
        payload,
        "request_id",
        "the payload needs a workspace_id and a request_id",
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{
        Cause, HmacKey, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceName,
    };
    use zirv_flow_store::{ErasureStatus, ProfileOutcome, ProfileRecord, apply_profile_batch};

    use super::*;

    #[test]
    fn the_kind_name_is_privacy_erase() {
        assert_eq!(PrivacyErase::KIND, "privacy.erase");
    }

    #[test]
    fn the_payload_needs_a_workspace_and_a_request() {
        let ws = WorkspaceId::new();
        let request = Uuid::now_v7();

        let parsed = parse(&json!({"workspace_id": ws, "request_id": request})).unwrap();

        assert_eq!(parsed, (ws, request));
        assert!(parse(&json!({"request_id": request})).is_err());
        assert!(parse(&json!({"workspace_id": ws})).is_err());
        assert!(parse(&json!({"workspace_id": ws, "request_id": 5})).is_err());
    }

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("zirv-flow-erase-{}", Uuid::now_v7()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn files(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .unwrap()
                .map(|item| item.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A workspace with one person and a pending erasure request for them.
    async fn pending_request(
        pool: &PgPool,
        key: &Arc<HmacKey>,
    ) -> (WorkspaceId, Uuid, zirv_flow_store::ErasureRequest) {
        zirv_flow_store::migrate(pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(pool, &name, &Slug::parse("ours").unwrap())
            .await
            .unwrap()
            .id;
        let trace = TraceContext::root(Cause::api("t"));
        let outcomes = apply_profile_batch(
            pool,
            ws,
            &[ProfileRecord {
                source: "crm".into(),
                stream: "s".into(),
                record_key: "1".into(),
                identifiers: vec![("email".into(), "ada@example.com".into())],
                traits: json!({"plan": "pro"}).as_object().cloned().unwrap(),
                observed_at: datetime!(2026-01-01 00:00 UTC),
                trace: trace.clone(),
            }],
        )
        .await
        .unwrap();
        let ProfileOutcome::Applied { person_id, .. } = outcomes[0] else {
            panic!("not applied");
        };
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        let request = zirv_flow_store::request_erasure(&mut tx, ws, person_id, key, None, &trace)
            .await
            .unwrap()
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        (ws, person_id, request)
    }

    fn identifier_key() -> Arc<HmacKey> {
        Arc::new(HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Identifier))
    }

    /// Runs the claimed job once; commits only when `commit` is set and the job succeeded.
    async fn run_once(
        pool: &PgPool,
        claimed: &zirv_flow_store::ClaimedJob,
        key: &Arc<HmacKey>,
        dir: Option<&std::path::Path>,
        commit: bool,
    ) -> Result<(), JobError> {
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, claimed)
            .with_identifier_key(Some(key.clone()))
            .with_ledger_dir(dir.map(|dir| Arc::new(dir.to_path_buf())));
        let result = PrivacyErase.run(&mut ctx, claimed.payload.clone()).await;
        if result.is_ok() && commit {
            zirv_flow_store::commit(tx).await.unwrap();
        }
        result
    }

    async fn claim(pool: &PgPool) -> zirv_flow_store::ClaimedJob {
        zirv_flow_store::claim_job(
            pool,
            "w",
            &[PrivacyErase::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .expect("the request enqueued the job")
    }

    async fn status(pool: &PgPool, ws: WorkspaceId, id: Uuid) -> ErasureStatus {
        zirv_flow_store::find_erasure_request(pool, ws, id)
            .await
            .unwrap()
            .unwrap()
            .status
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_writes_the_ledger_file_before_it_returns(pool: PgPool) {
        let key = identifier_key();
        let (ws, _, request) = pending_request(&pool, &key).await;
        let dir = TempDir::new();
        let claimed = claim(&pool).await;

        run_once(&pool, &claimed, &key, Some(&dir.0), true)
            .await
            .unwrap();

        assert_eq!(
            dir.files(),
            [format!("{}-{}.json", ws.as_uuid(), request.id)]
        );
        let entries = zirv_flow_store::read_ledger_dir(&dir.0).unwrap();
        assert_eq!(entries[0].id, request.id);
        assert_eq!(entries[0].identifier_kinds, ["email"]);
        assert_eq!(
            status(&pool, ws, request.id).await,
            ErasureStatus::Completed
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_a_ledger_dir_the_job_still_completes(pool: PgPool) {
        let key = identifier_key();
        let (ws, _, request) = pending_request(&pool, &key).await;
        let claimed = claim(&pool).await;

        run_once(&pool, &claimed, &key, None, true).await.unwrap();

        assert_eq!(
            status(&pool, ws, request.id).await,
            ErasureStatus::Completed
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unwritable_directory_fails_the_job_and_leaves_the_request_pending(pool: PgPool) {
        let key = identifier_key();
        let (ws, person, request) = pending_request(&pool, &key).await;
        let dir = TempDir::new();
        // A directory that is really a file, and one that is missing.
        let file = dir.0.join("plain-file");
        std::fs::write(&file, b"x").unwrap();
        let claimed = claim(&pool).await;

        for bad in [file, dir.0.join("absent")] {
            let error = run_once(&pool, &claimed, &key, Some(&bad), true)
                .await
                .unwrap_err();
            assert_eq!(
                error.message(),
                "the erasure ledger file could not be written"
            );
        }

        assert_eq!(status(&pool, ws, request.id).await, ErasureStatus::Pending);
        assert!(
            zirv_flow_store::find_person(&pool, ws, person)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_retry_after_a_crash_between_rename_and_commit_finds_the_file_and_succeeds(
        pool: PgPool,
    ) {
        let key = identifier_key();
        let (ws, _, request) = pending_request(&pool, &key).await;
        let dir = TempDir::new();
        let claimed = claim(&pool).await;

        // The file is renamed into place, then the transaction is dropped uncommitted.
        run_once(&pool, &claimed, &key, Some(&dir.0), false)
            .await
            .unwrap();
        assert_eq!(dir.files().len(), 1);
        assert_eq!(status(&pool, ws, request.id).await, ErasureStatus::Pending);

        run_once(&pool, &claimed, &key, Some(&dir.0), true)
            .await
            .unwrap();

        assert_eq!(dir.files().len(), 1);
        assert_eq!(
            status(&pool, ws, request.id).await,
            ErasureStatus::Completed
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_of_a_request_merged_from_its_file_succeeds_and_leaves_the_file_as_it_was(
        pool: PgPool,
    ) {
        let key = identifier_key();
        let (ws, _, request) = pending_request(&pool, &key).await;
        let dir = TempDir::new();
        let claimed = claim(&pool).await;
        // The file exists, the transaction never committed: the row is pending, as in a backup.
        run_once(&pool, &claimed, &key, Some(&dir.0), false)
            .await
            .unwrap();
        let name = format!("{}-{}.json", ws.as_uuid(), request.id);
        let before = std::fs::read(dir.0.join(&name)).unwrap();
        let entries = zirv_flow_store::read_ledger_dir(&dir.0).unwrap();
        let merged = zirv_flow_store::merge_ledger_entries(&pool, &entries)
            .await
            .unwrap();
        assert_eq!(merged.merged, 1);
        assert_eq!(
            status(&pool, ws, request.id).await,
            ErasureStatus::Completed
        );

        run_once(&pool, &claimed, &key, Some(&dir.0), true)
            .await
            .unwrap();

        assert_eq!(std::fs::read(dir.0.join(&name)).unwrap(), before);
        assert_eq!(dir.files(), [name]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_different_file_of_the_same_name_fails_the_job(pool: PgPool) {
        let key = identifier_key();
        let (ws, _, request) = pending_request(&pool, &key).await;
        let dir = TempDir::new();
        std::fs::write(
            dir.0.join(format!("{}-{}.json", ws.as_uuid(), request.id)),
            b"{}",
        )
        .unwrap();
        let claimed = claim(&pool).await;

        let error = run_once(&pool, &claimed, &key, Some(&dir.0), true)
            .await
            .unwrap_err();

        assert_eq!(
            error.message(),
            "an erasure ledger file with other content exists"
        );
        assert_eq!(status(&pool, ws, request.id).await, ErasureStatus::Pending);
    }

    async fn run_claimed(pool: &PgPool, key: Option<Arc<HmacKey>>) -> Result<(), JobError> {
        let claimed = zirv_flow_store::claim_job(
            pool,
            "w",
            &[PrivacyErase::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .expect("the request enqueued the job");
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed).with_identifier_key(key);
        let result = PrivacyErase.run(&mut ctx, claimed.payload.clone()).await;
        if result.is_ok() {
            zirv_flow_store::commit(tx).await.unwrap();
        }
        result
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_erases_the_person_suppresses_the_email_and_completes_the_request(
        pool: PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &Slug::parse("ours").unwrap())
            .await
            .unwrap()
            .id;
        let trace = TraceContext::root(Cause::api("t"));
        let outcomes = apply_profile_batch(
            &pool,
            ws,
            &[ProfileRecord {
                source: "crm".into(),
                stream: "s".into(),
                record_key: "1".into(),
                identifiers: vec![("email".into(), "ada@example.com".into())],
                traits: json!({"plan": "pro"}).as_object().cloned().unwrap(),
                observed_at: datetime!(2026-01-01 00:00 UTC),
                trace: trace.clone(),
            }],
        )
        .await
        .unwrap();
        let ProfileOutcome::Applied { person_id, .. } = outcomes[0] else {
            panic!("not applied");
        };
        let key = Arc::new(
            HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Identifier),
        );
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let request = zirv_flow_store::request_erasure(&mut tx, ws, person_id, &key, None, &trace)
            .await
            .unwrap()
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();

        run_claimed(&pool, Some(key.clone())).await.unwrap();

        assert!(
            zirv_flow_store::find_person(&pool, ws, person_id)
                .await
                .unwrap()
                .is_none()
        );
        let hash = zirv_flow_core::people::identifier_hash(
            &key,
            &zirv_flow_core::people::Identifier::parse("email", "ada@example.com").unwrap(),
        );
        assert!(
            zirv_flow_store::is_suppressed(&pool, ws, &hash)
                .await
                .unwrap()
        );
        let stored = zirv_flow_store::find_erasure_request(&pool, ws, request.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, ErasureStatus::Completed);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_request_that_does_not_exist_fails_the_job_with_a_fixed_message(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &Slug::parse("ours").unwrap())
            .await
            .unwrap()
            .id;
        let mut job =
            zirv_flow_store::NewJob::new(PrivacyErase::KIND, TraceContext::root(Cause::api("t")));
        job.workspace_id = Some(ws);
        job.payload = json!({"workspace_id": ws, "request_id": Uuid::now_v7()});
        zirv_flow_store::enqueue(&pool, &job).await.unwrap();

        let key = Arc::new(
            HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Identifier),
        );
        let error = run_claimed(&pool, Some(key.clone())).await.unwrap_err();

        assert_eq!(error.message(), "the erasure request does not exist");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_the_identifier_key_the_job_fails_and_erases_nothing(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &Slug::parse("ours").unwrap())
            .await
            .unwrap()
            .id;
        let mut job =
            zirv_flow_store::NewJob::new(PrivacyErase::KIND, TraceContext::root(Cause::api("t")));
        job.workspace_id = Some(ws);
        job.payload = json!({"workspace_id": ws, "request_id": Uuid::now_v7()});
        zirv_flow_store::enqueue(&pool, &job).await.unwrap();

        let error = run_claimed(&pool, None).await.unwrap_err();

        assert_eq!(error.message(), "the worker has no master key");
    }
}
