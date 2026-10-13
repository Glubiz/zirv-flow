//! Persistence for zirv-flow.

mod accounts;
mod ai;
mod api_keys;
mod audit;
mod bounded;
mod consent;
mod contacts;
mod decisions;
mod events;
mod identities;
mod imports;
mod ingest;
mod jobs;
mod lists;
mod migrations;
mod people;
mod pool;
mod privacy;
pub mod secrets;
mod sessions;
mod sources;
mod suppressions;
mod sync_runs;
mod workspaces;

use std::error::Error;
use std::fmt;

pub use accounts::{
    User, count_owners, create_user, find_user, find_user_by_email, is_locked, list_users,
    record_failed_sign_in, record_successful_sign_in, set_password,
};
pub use ai::{
    AiCall, AiSettings, AiSettingsUpdate, AiUsage, CallOutcome, FeatureUsage, FinishedAiCall,
    NewAiCall, ProbeRecord, Reservation, ReservationLimits, TRANSCRIPT_PURGE_BATCH, UserUsage,
    ai_usage, calls_in_last_minute, erase_ai_transcripts, find_ai_settings, finish_ai_call,
    insert_ai_transcript, lock_ai_settings, month_start, purge_expired_ai_transcripts,
    put_ai_settings, record_ai_call, reserve_ai_call, spent_this_month,
};
pub use api_keys::{
    ApiKey, StoredApiKey, create_api_key, find_api_key_for_auth, list_api_keys, revoke_api_key,
    touch_api_key,
};
pub use audit::{Actor, ActorKind, AuditEntry, NewAuditEntry, append_audit, list_audit};
pub use consent::{
    ConsentChange, ConsentEntry, ConsentHistoryEntry, ConsentOutcome, NewTopic, Topic, TopicUpdate,
    consent_for, consent_history, create_topic, find_topic, find_topic_by_key, list_topics,
    set_consent, update_topic,
};
pub use contacts::{
    Contact, ContactChange, ContactDefinition, ContactPage, ContactSearch, ContactSearchError,
    DefinitionError, DefinitionProblem, Idempotency, WriteError, Written, contact_registry,
    delete_contact, expire_contact_idempotency, find_contact, find_contact_definition,
    list_contacts, put_contact_definition, search_contacts, typed_definition, write_contact,
};
pub use decisions::{
    Decision, NewDecision, append_decision, append_decisions, ensure_decision_partitions,
    list_trace,
};
pub use events::{
    BREAKDOWN_VALUES, Breakdown, Bucket, CountResult, DEBUGGER_CHANNEL, DebuggerListener,
    EventOutcome, EventPolicy, EventRecord, EventRetention, EventSchema, EventSchemaUpdate,
    ExploreError, ExploreFilter, ExploreLimits, ExploreRow, ExploreScope, FEED_RETENTION_MINUTES,
    FeedFilter, FeedItem, FilterOp, IngestReject, MAX_FILTERS, MAX_RANGE_DAYS, MAX_REPLAY,
    NewEventSchema, PublishOutcome, REPLAY_JOB_KIND, RETENTION_BATCH, RejectCursor, RejectFilter,
    ReplayPrediction, ReplaySelection, ReplayTotals, RetentionReport, RowCursor, RowsPage,
    SchemaVersion, WATCH_SECONDS, apply_event_batch, apply_event_retention, create_event_schema,
    debugger_feed_head, debugger_feed_position, debugger_feed_seqs, debugger_feed_start,
    enqueue_replay, ensure_event_partitions, explore_breakdown, explore_count, explore_rows,
    explore_total, find_event_schema, get_event_policy, list_event_schemas, list_ingest_rejects,
    list_schema_versions, listen_for_debugger, preview_replay, prune_debugger_feed,
    publish_schema_draft, read_debugger_feed, replay_rejects, save_schema_draft, select_rejects,
    set_event_policy, touch_debugger_watch, update_event_schema,
};
pub use identities::{
    OidcLogin, create_oidc_login, delete_expired_oidc_logins, find_identity_user, link_identity,
    take_oidc_login,
};
pub use imports::{
    ABANDONED_AFTER_DAYS, BatchOutcome, BatchProgress, CHUNK_BYTES, CHUNK_RETENTION_DAYS,
    DRY_RUN_JOB_KIND, DryRunBatch, Import, ImportBatch, ImportPhase, ImportReject, ImportRetention,
    ImportStatus, ImportUpload, REJECT_CHECK_MIN_ROWS, REJECT_RETENTION_DAYS, RUN_JOB_KIND,
    RejectReason, RejectedRow, RowDetail, StoredReason, TransitionError,
    UPLOAD_ABANDONED_AFTER_DAYS, UploadScan, UploadStart, cancel_import, commit_dry_run_batch,
    commit_import_batch, delete_expired_import_data, enqueue_import_job, fail_import, find_import,
    finish_import_dry_run, finish_import_run, list_import_rejects, list_imports, read_import_chunk,
    set_import_mapping, start_import_dry_run, start_import_run,
};
pub use ingest::{
    DRAIN_JOB_KIND, DrainOutcome, InboxHead, IngestItem, IngestPayload, Shard, append_inbox,
    backfill_affinity, drain_batch, drain_batch_singly, enqueue_drain, inbox_heads,
};
pub use jobs::{
    ClaimedJob, DEFAULT_MAX_ATTEMPTS, DeadCursor, FailOutcome, Job, JobListener, JobSummary,
    KindDepth, Leadership, NOTIFY_CHANNEL, NewJob, NewSchedule, QueueCounts, QueueDepth, Reaped,
    Schedule, advance_schedule, claim_job, complete_job, create_schedule, database_now,
    disable_schedule, due_schedules, enqueue, fail_job, find_job, heartbeat_job, kind_depths,
    list_dead_jobs, listen_for_jobs, lock_lease, queue_counts, queue_depths, reap_expired,
    retry_dead_job, try_lead,
};
pub use lists::{
    List, ListMember, ListSource, MAX_PEOPLE_PER_CALL, MemberIdentifier, MembershipChange,
    MembershipError, MembershipOutcome, PersonList, add_members, create_list, delete_list,
    find_list, list_lists, lists_for, members, membership_history, remove_members, update_list,
};
pub use migrations::{SchemaState, migrate, schema_state};
pub use people::{
    ConflictResolution, DroppedTrait, IdentityConflict, IdentityGroup, NewTraitDefinition, Person,
    PersonSearch, Profile, ProfileMerge, ProfileOutcome, ProfileRecord, ProfileTrait,
    RECOMPUTE_BATCH, REWRITE_BATCH, REWRITE_JOB_KIND, ResolveError, SearchError, SearchHit,
    SearchPage, StoredIdentity, TimelineCursor, TimelineDecision, TimelineEntry, TimelineEvent,
    TimelineFilter, TimelineKind, TimelinePage, TraitDefinition, TraitDefinitionUpdate, TraitOp,
    apply_profile_batch, create_trait_definition, current_person, find_conflict,
    find_people_by_identifiers, find_person, find_person_by_identifier, find_trait_definition,
    find_trait_definition_for_update, list_conflicts, list_identities, list_identity_conflicts,
    list_trait_definitions, load_profile, load_timeline, recompute_trait_batch,
    remove_contribution, resolve_conflict, rewrite_merged_person, search_people,
    update_trait_definition,
};
pub use pool::{PgPool, Tx, begin, commit, connect, observe_pool, sized_like};
pub use privacy::{
    BuildOutcome, DeleteReason, ERASE_JOB_KIND, EXPORT_FORMAT, EXPORT_JOB_KIND, EraseOutcome,
    ErasureCursor, ErasureRequest, ErasureStatus, ExportCursor, ExportDownload, ExportStatus,
    PrivacyExport, ReplayReport, build_export, delete_person, download_export, erase_person,
    expire_exports, find_erasure_request, find_privacy_export, list_erasure_requests,
    list_privacy_exports, replay_erasures, request_erasure, request_export,
};
pub use sessions::{
    ActiveSession, create_session, delete_expired_sessions, delete_session,
    delete_sessions_for_user, find_active_session, touch_session,
};
pub use sources::{
    DiscoveredStream, NewSource, Source, SourceUpdate, Stream, StreamSettings, configure_stream,
    create_source, delete_source, find_source, find_source_for_update, find_stream,
    find_stream_by_id, list_sources, list_streams, set_stream_mapping, sync_reconcile_schedule,
    sync_schedule, update_source, upsert_discovered_streams,
};
pub use sqlx::PgConnection;
pub use suppressions::{
    AddOutcome, BatchEntry, NewSuppression, RemoveOutcome, SUPPRESSION_KINDS, Suppression,
    SuppressionCursor, add_suppression, add_suppressions, count_suppressed, find_suppression,
    is_suppressed, list_suppressions, remove_suppression,
};
pub use sync_runs::{
    BatchOutcome as SyncBatchOutcome, BatchProgress as SyncBatchProgress, CancelOutcome,
    Counts as SyncCounts, DELETE_BY_SIGNAL as SYNC_DELETE_BY_SIGNAL,
    DELETE_BY_SOURCE as SYNC_DELETE_BY_SOURCE, FanOut, Lease as SyncLease,
    RUN_JOB_KIND as SYNC_RUN_JOB_KIND, ReconcileQueued, RunCursor, RunEnd, RunKind, RunReject,
    RunTrigger, SYNC_REJECT_CHECK_MIN_RECORDS, SYNC_SWEEP_BATCH, SweepOutcome, SweepUnit,
    SyncBatch, SyncReject, SyncRun, SyncUpsert, cancel_run, commit_sync_batch, fail_orphaned_runs,
    fail_run, find_run, finish_run, list_run_rejects, list_runs, queue_reconcile_run, queue_run,
    queue_runs_for_source, run_trace, start_run, sweep_exceeds_limit, sweep_unreached,
};
pub use workspaces::{
    Member, Membership, Workspace, create_workspace, find_workspace, find_workspace_by_slug,
    list_members, list_workspaces_for_user, membership_role, orphan_policy, remove_membership,
    set_membership, set_orphan_policy,
};
/// The operator master key, its derived keys and the sealing of secrets, re-exported so the
/// binary reaches them through the store.
pub use zirv_flow_core::{
    HmacKey, HmacPurpose, HmacRoot, Keyring, MasterKey, SecretBytes, SecretId, SecretPurpose,
    deployment_key_aad, open as open_secret, seal as seal_secret, secret_aad,
};

#[derive(Debug)]
pub struct StoreError {
    kind: StoreErrorKind,
    source: Option<Box<dyn Error + Send + Sync>>,
}

#[derive(Debug)]
enum StoreErrorKind {
    Connect,
    InspectSchema,
    ApplyMigrations,
    NewerSchema(i64),
    DirtyMigration(i64),
    ModifiedMigration(i64),
    Query,
    Conflict,
    Retryable,
    Timeout,
    RunActive,
}

impl StoreError {
    fn connect(source: sqlx::Error) -> Self {
        Self::with_source(StoreErrorKind::Connect, source)
    }

    fn inspect_schema(source: sqlx::Error) -> Self {
        Self::with_source(StoreErrorKind::InspectSchema, source)
    }

    fn migration(source: sqlx::migrate::MigrateError) -> Self {
        use sqlx::migrate::MigrateError;

        match source {
            MigrateError::VersionMissing(version) => {
                Self::new(StoreErrorKind::NewerSchema(version))
            }
            MigrateError::Dirty(version) => Self::new(StoreErrorKind::DirtyMigration(version)),
            MigrateError::VersionMismatch(version) => {
                Self::new(StoreErrorKind::ModifiedMigration(version))
            }
            source => Self::with_source(StoreErrorKind::ApplyMigrations, source),
        }
    }

    fn apply_migrations(source: sqlx::Error) -> Self {
        Self::with_source(StoreErrorKind::ApplyMigrations, source)
    }

    /// A failed query. A unique-constraint violation becomes a conflict.
    pub(crate) fn query(source: sqlx::Error) -> Self {
        let conflict = source
            .as_database_error()
            .is_some_and(|error| error.is_unique_violation());
        let retryable = source
            .as_database_error()
            .and_then(|error| error.code())
            .is_some_and(|code| matches!(code.as_ref(), "40P01" | "40001"));
        let kind = if conflict {
            StoreErrorKind::Conflict
        } else if retryable {
            StoreErrorKind::Retryable
        } else {
            StoreErrorKind::Query
        };
        Self::with_source(kind, source)
    }

    /// A failed query of a bounded read (search, profile, timeline). PostgreSQL cancelling it
    /// at its statement timeout (SQLSTATE 57014) becomes a timeout; anywhere else a
    /// cancellation stays a plain query failure.
    pub(crate) fn bounded(source: sqlx::Error) -> Self {
        let timeout = source
            .as_database_error()
            .and_then(|error| error.code())
            .is_some_and(|code| code.as_ref() == "57014");
        if timeout {
            Self::with_source(StoreErrorKind::Timeout, source)
        } else {
            Self::query(source)
        }
    }

    /// Reclassifies a query cancelled at its statement timeout (SQLSTATE 57014) as a timeout.
    /// For the errors of a bounded read that goes through helpers which report plain query
    /// failures; any other error is returned as it was.
    pub(crate) fn into_bounded(self) -> Self {
        let cancelled = matches!(self.kind, StoreErrorKind::Query)
            && self
                .source
                .as_deref()
                .and_then(|source| source.downcast_ref::<sqlx::Error>())
                .and_then(sqlx::Error::as_database_error)
                .and_then(|error| error.code())
                .is_some_and(|code| code.as_ref() == "57014");
        if cancelled {
            Self {
                kind: StoreErrorKind::Timeout,
                source: self.source,
            }
        } else {
            self
        }
    }

    /// A transaction that lost a race it could not resolve: nothing was committed and the
    /// whole unit of work can be run again.
    pub(crate) fn retryable_race() -> Self {
        Self::new(StoreErrorKind::Retryable)
    }

    /// An inbox row that cannot be mapped to a record: retrying the batch cannot help.
    pub(crate) fn poison() -> Self {
        Self::new(StoreErrorKind::Query)
    }

    /// Whether running the whole transaction again can succeed: PostgreSQL reported a
    /// deadlock (40P01) or a serialization failure (40001), or a race could not be
    /// resolved within its retries.
    pub fn is_retryable(&self) -> bool {
        matches!(self.kind, StoreErrorKind::Retryable)
    }

    /// A change the stream's active sync run would undo: it was refused and nothing was written.
    pub(crate) fn run_active() -> Self {
        Self::new(StoreErrorKind::RunActive)
    }

    /// Whether the change was refused because a sync run is queued or running.
    pub fn is_run_active(&self) -> bool {
        matches!(self.kind, StoreErrorKind::RunActive)
    }

    /// Whether the query ran past its statement timeout and PostgreSQL cancelled it.
    pub fn is_timeout(&self) -> bool {
        matches!(self.kind, StoreErrorKind::Timeout)
    }

    /// Whether the write was refused because the value already exists.
    pub fn is_conflict(&self) -> bool {
        matches!(self.kind, StoreErrorKind::Conflict)
    }

    fn newer_schema(version: i64) -> Self {
        Self::new(StoreErrorKind::NewerSchema(version))
    }

    fn dirty_migration(version: i64) -> Self {
        Self::new(StoreErrorKind::DirtyMigration(version))
    }

    fn modified_migration(version: i64) -> Self {
        Self::new(StoreErrorKind::ModifiedMigration(version))
    }

    fn new(kind: StoreErrorKind) -> Self {
        Self { kind, source: None }
    }

    fn with_source(kind: StoreErrorKind, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            kind,
            source: Some(Box::new(source)),
        }
    }
}

impl StoreError {
    fn write_source(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Some(source) => write!(formatter, ": {source}"),
            None => Ok(()),
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            StoreErrorKind::Connect => {
                formatter.write_str("could not connect to PostgreSQL")?;
                self.write_source(formatter)
            }
            StoreErrorKind::InspectSchema => {
                formatter.write_str("could not inspect the database schema")?;
                self.write_source(formatter)
            }
            StoreErrorKind::ApplyMigrations => {
                formatter.write_str("could not apply database migrations")?;
                self.write_source(formatter)
            }
            StoreErrorKind::Query => {
                formatter.write_str("database query failed")?;
                self.write_source(formatter)
            }
            StoreErrorKind::Conflict => formatter.write_str("the value already exists"),
            StoreErrorKind::Timeout => formatter.write_str("the query ran past its time limit"),
            StoreErrorKind::RunActive => formatter.write_str("a sync run is queued or running"),
            StoreErrorKind::Retryable => {
                formatter
                    .write_str("the transaction conflicted with another and can be retried")?;
                self.write_source(formatter)
            }
            StoreErrorKind::NewerSchema(version) => write!(
                formatter,
                "database schema version {version} is newer than this binary supports"
            ),
            StoreErrorKind::DirtyMigration(version) => {
                write!(formatter, "migration {version} is partially applied")
            }
            StoreErrorKind::ModifiedMigration(version) => write!(
                formatter,
                "migration {version} was changed after it was applied; restore the original migration file"
            ),
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    /// Tables that hold instance-level data and so carry no `workspace_id`. Every other
    /// table is workspace-scoped and must have the column.
    const INSTANCE_LEVEL_TABLES: [&str; 8] = [
        "users",
        "user_identities",
        "oidc_logins",
        "workspaces",
        "sessions",
        "audit_log",
        "deployment_keys",
        "_sqlx_migrations",
    ];

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_table_without_a_workspace_id_is_on_the_instance_level_allowlist(pool: PgPool) {
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT t.table_name::text FROM information_schema.tables t              WHERE t.table_schema = current_schema() AND t.table_type = 'BASE TABLE'                AND NOT EXISTS (SELECT 1 FROM information_schema.columns c                                WHERE c.table_schema = t.table_schema                                  AND c.table_name = t.table_name                                  AND c.column_name = 'workspace_id')              ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        let unexpected: Vec<&String> = tables
            .iter()
            .filter(|table| !INSTANCE_LEVEL_TABLES.contains(&table.as_str()))
            .collect();
        assert!(
            unexpected.is_empty(),
            "tables without workspace_id that are not on the allowlist: {unexpected:?}"
        );
        assert!(tables.contains(&"users".to_owned()));
    }

    #[test]
    fn a_plain_query_error_is_not_a_conflict() {
        let error = StoreError::query(sqlx::Error::RowNotFound);

        assert!(!error.is_conflict());
        assert!(error.to_string().starts_with("database query failed"));
    }

    #[test]
    fn only_a_lost_race_or_a_database_conflict_is_retryable() {
        assert!(StoreError::retryable_race().is_retryable());
        assert!(!StoreError::query(sqlx::Error::RowNotFound).is_retryable());
        assert!(!StoreError::new(StoreErrorKind::Conflict).is_retryable());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_postgres_serialization_failure_is_retryable(pool: PgPool) {
        let error = sqlx::query("DO $$ BEGIN RAISE EXCEPTION 'x' USING ERRCODE = '40001'; END $$")
            .execute(&pool)
            .await
            .unwrap_err();

        assert!(StoreError::query(error).is_retryable());
    }

    #[test]
    fn a_conflict_does_not_show_database_detail() {
        let error = StoreError::new(StoreErrorKind::Conflict);

        assert!(error.is_conflict());
        assert_eq!(error.to_string(), "the value already exists");
    }
}
