//! Persistence for zirv-flow.

mod accounts;
mod ai;
mod api_keys;
mod assets;
mod audit;
mod bounded;
mod by_value;
mod changes;
mod clock;
mod consent;
mod contacts;
mod decisions;
mod destinations;
mod events;
mod feedback;
mod flows;
mod forwarding;
mod identities;
mod imports;
mod ingest;
mod jobs;
mod lists;
mod messages;
mod migrations;
mod people;
mod pool;
mod privacy;
pub mod secrets;
mod segments;
mod send_limits;
mod sending;
mod sessions;
mod snippets;
mod sources;
mod stream_freshness;
mod stream_offsets;
mod suppressions;
mod sync_runs;
mod templates;
mod test_sends;
mod themes;
mod tracking;
mod webhooks;
mod workspace_credentials;
mod workspaces;

use std::error::Error;
use std::fmt;

pub use accounts::{
    User, count_owners, create_user, find_user, find_user_by_email, is_locked, list_users,
    record_failed_sign_in, record_successful_sign_in, set_password,
};
pub use ai::{
    AiCall, AiDraft, AiSettings, AiSettingsUpdate, AiUsage, CallOutcome, DRAFT_PURGE_BATCH,
    DraftKind, DraftStatus, FeatureUsage, FinishedAiCall, NewAiCall, NewAiDraft, ProbeRecord,
    Reservation, ReservationLimits, TRANSCRIPT_PURGE_BATCH, UserUsage, ai_usage,
    calls_in_last_minute, erase_ai_transcripts, find_ai_draft, find_ai_settings, finish_ai_call,
    insert_ai_draft, insert_ai_transcript, lock_ai_settings, mark_ai_draft_accepted, month_start,
    purge_expired_ai_drafts, purge_expired_ai_transcripts, put_ai_settings, record_ai_call,
    reserve_ai_call, spent_this_month,
};
pub use api_keys::{
    ApiKey, StoredApiKey, create_api_key, find_api_key_for_auth, list_api_keys, revoke_api_key,
    touch_api_key,
};
pub use assets::{
    Asset, AssetError, AssetListing, AssetOwner, AssetOwnerKind, AssetReferenceError,
    DeleteAssetError, NewAsset, ServedAsset, StoredAsset, asset_owners, asset_url, delete_asset,
    find_asset, find_served_asset, list_assets, parse_sha256_hex, record_references, sha256_hex,
    store_asset, workspace_asset_bytes,
};
pub use audit::{Actor, ActorKind, AuditEntry, NewAuditEntry, append_audit, list_audit};
pub use changes::{
    DirtyHead, DirtyPerson, REEVALUATE_JOB_KIND, claim_dirty, dirty_backlog, dirty_heads,
    enqueue_reevaluate,
};
pub use clock::{Clock, ManualClock};
pub use consent::{
    ConsentChange, ConsentEntry, ConsentExportCursor, ConsentExportRow, ConsentHistoryEntry,
    ConsentOutcome, ConsentPolicy, DecidingRule, Dimension, DimensionUpdate, DimensionValue,
    NewDimension, NewTopic, PolicyVersion, ReadOutcome, RuleCheck, RuleToCheck, Topic, TopicUpdate,
    consent_export_page, consent_for, consent_history, consent_policy_draft, consent_read,
    create_dimension, create_dimension_value, create_topic, dimensions_of, evaluate_consent_policy,
    find_consent_policy, find_dimension, find_topic, find_topic_by_key,
    list_consent_policy_versions, list_dimensions, list_topics, lock_consent_policy,
    publish_consent_policy, published_consent_policy, save_consent_policy_draft,
    save_consent_policy_draft_at, set_consent, unknown_dimension_entry, update_dimension,
    update_dimension_value, update_topic,
};
pub use contacts::{
    Contact, ContactChange, ContactDefinition, ContactPage, ContactSearch, ContactSearchError,
    DefinitionError, DefinitionProblem, Idempotency, WriteError, Written, contact_registry,
    delete_contact, erase_contact_idempotency, expire_contact_idempotency, find_contact,
    find_contact_definition, list_contacts, put_contact_definition, search_contacts,
    typed_definition, write_contact,
};
pub use decisions::{
    Decision, GraphPoint, NewDecision, append_decision, append_decisions,
    ensure_decision_partitions, list_trace,
};
pub use destinations::{
    Admission, AttemptOutcome, AttemptStats, BucketState, DEFAULT_RETRY_AFTER, DeliveryAttempt,
    Destination, DestinationStat, DestinationUpdate, LEASE, MAX_BLOCK, MAX_RETRY_AFTER,
    NewDeliveryAttempt, NewDestination, RateGate, StatusClass, SubjectKind, archive_destination,
    bucket_state, count_delivery_attempt, count_delivery_attempt_record_failure,
    create_destination, destination_attempt_stats, destination_report, find_destination,
    find_destination_for_update, find_destination_workspace, list_delivery_attempts,
    list_destinations, record_delivery_attempt, throttle_bucket, update_destination,
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
    enqueue_replay, ensure_event_partitions, erase_unattributed_rejects, explore_breakdown,
    explore_count, explore_rows, explore_total, find_event_schema, get_event_policy,
    list_event_schemas, list_ingest_rejects, list_schema_versions, listen_for_debugger,
    preview_replay, prune_debugger_feed, publish_schema_draft, read_debugger_feed, replay_rejects,
    save_schema_draft, select_rejects, set_event_policy, touch_debugger_watch, update_event_schema,
};
pub use feedback::{
    FeedbackEvent, FeedbackOutcome, ProviderFeedback, SuppressionMade, record_provider_feedback,
};
pub use flows::{
    AudienceSource, ControlError, DELIVERY_UNCONFIRMED, ENROLL_JOB_KIND, EffectError, EndedOrphan,
    EnrollEntries, EnrollEntry, EnrollError, EnrollJobPayload, EnrollOutcome, EnrollRequest,
    EnrollWork, Enrolled, Enrollment, EnrollmentCursor, EnrollmentFilter, EnrollmentPage,
    EnrollmentPath, EnrollmentStatus, EnrollmentSummary, EnrollmentUpdate, ExitBatch, Flow,
    FlowAction, FlowError, FlowPatch, FlowState, FlowTraitWrite, FlowVersion, HARD_BOUNCE,
    INTERNAL_ERROR, JOB_DEAD, LIFECYCLE_BATCH, LIFECYCLE_JOB_KIND, LifecycleAction, LifecycleBatch,
    LifecyclePayload, LockedFlow, MAX_ENROLL_PEOPLE, MAX_PATH_DECISIONS, NewFlow, NewStepExecution,
    NewWait, NodeCount, Occurrence, PathEventWait, PathStep, Rotation, SEND_JOB_KIND,
    SETTINGS_GOAL_NODE, SETTLE_JOB_KIND, SOFT_BOUNCE, STEP_JOB_KIND, SYNC_JOB_KIND, SendOutcome,
    SettleItem, SettlePayload, Signal, SigningKey, SigningKeys, StepJobInput, StepJobPayload,
    StepOutcome, StepRecord, StoredEvent, SyncDetail, TriggerEvent, TriggerKind, TriggeredFlow,
    UnhealthyInput, WEBHOOK_JOB_KIND, WebhookOutcome, WebhookPerson, WebhookResult, WebhookSubject,
    apply_flow_list, apply_flow_trait, archive_flow, audience_page, clear_waits,
    count_in_flight_enrollments, create_flow, create_signing_key, end_orphaned_messages,
    enqueue_lifecycle, enroll, enroll_entries, enroll_page_key, exit_batch, find_enrollment,
    find_event_of_person, find_events, find_flow, find_flow_version, flow_source,
    flow_state_for_step, has_webhook_signing_secret, hold_enrollment, list_flow_enrollments,
    list_flow_versions, list_flows, live_signing_keys, load_enrollment_path, lock_audience_page,
    lock_enrollment, lock_flow, lock_sync_person, lock_webhook_person, occurrence_of, pause_flow,
    publish_flow, read_webhook_subject, reconcile_flow_schedules, record_send_outcome,
    record_steps, record_sync_evidence, record_webhook_outcome, register_wait, release_enrollment,
    reopen_failed_step, resolve_survivors, resume_batch, resume_failed, resume_flow,
    rotate_signing_key, save_enrollment, segment_is_watched, segment_trigger_flows,
    settle_wait_step, step_zones, unhealthy_inputs, update_flow_draft, version_node_counts,
    wait_filters,
};
pub use forwarding::{
    ClaimedRow, DELIVERED_RETENTION, DeadLetter, FORWARD_JOB_KIND, ForwardRef, ForwardingRule,
    ForwardingStat, NewRule, QueueStatus, Redrive, RuleUpdate, RuleWrite, Settled, claim_due,
    create_rule, defer_row, delete_rule, enqueue_forwarding, enqueue_segment_forwarding,
    ensure_forward_job, extend_lease, find_rule, forwarding_report, forwarding_status,
    list_dead_letters, list_rules, lock_rule, next_due, purge_delivered, redrive, settle_row,
    update_rule,
};
pub use identities::{
    OidcLogin, create_oidc_login, delete_expired_oidc_logins, find_identity_user, link_identity,
    take_oidc_login,
};
pub use imports::{
    ABANDONED_AFTER_DAYS, BatchOutcome, BatchProgress, CHUNK_BYTES, CHUNK_RETENTION_DAYS,
    DRY_RUN_JOB_KIND, DryRunBatch, Import, ImportBatch, ImportErasure, ImportPhase, ImportReject,
    ImportRetention, ImportStatus, ImportUpload, REJECT_CHECK_MIN_ROWS, REJECT_RETENTION_DAYS,
    RUN_JOB_KIND, RejectReason, RejectedRow, RowDetail, StoredReason, TransitionError,
    UPLOAD_ABANDONED_AFTER_DAYS, UploadScan, UploadStart, cancel_import, commit_dry_run_batch,
    commit_import_batch, delete_expired_import_data, enqueue_import_job, erase_import_values,
    fail_import, find_import, finish_import_dry_run, finish_import_run, list_import_rejects,
    list_imports, read_import_chunk, set_import_mapping, start_import_dry_run, start_import_run,
};
pub use ingest::{
    DRAIN_JOB_KIND, DrainOutcome, InboxHead, IngestItem, IngestPayload, Shard, append_inbox,
    backfill_affinity, drain_batch, drain_batch_singly, enqueue_drain, inbox_heads,
};
pub use jobs::{
    ClaimedJob, DEFAULT_MAX_ATTEMPTS, DeadCursor, FailOutcome, Job, JobListener, JobSummary,
    KindDepth, Leadership, NOTIFY_CHANNEL, NewJob, NewSchedule, QueueCounts, QueueDepth, Reaped,
    Schedule, advance_schedule, claim_job, complete_job, create_schedule, database_now,
    disable_schedule, due_schedules, enqueue, enqueue_many, fail_job, find_job, heartbeat_job,
    kind_depths, list_dead_jobs, listen_for_jobs, lock_lease, queue_counts, queue_depths,
    reap_expired, retry_dead_job, try_lead,
};
pub use lists::{
    List, ListMember, ListSource, MAX_PEOPLE_PER_CALL, MemberIdentifier, MembershipChange,
    MembershipError, MembershipOutcome, PersonList, add_members, create_list, delete_list,
    find_list, list_lists, lists_for, members, membership_history, remove_members, update_list,
};
pub use messages::{
    GateRecord, LoggedMessage, Message, MessageLogCursor, MessageLogFilter, MessageLogPage,
    MessageRetention, MessageRetentionReport, MessageStatus, NewMessage, WaitFilter, WaitReason,
    apply_message_retention, apply_message_retention_for, clear_message_in_flight, find_message,
    insert_message, list_person_messages, load_person_message_log, mark_message_sending,
    mark_message_unconfirmed, message_address_hash, message_in_flight, message_status,
    message_unconfirmed, read_send_address,
};
pub use migrations::{SchemaState, migrate, schema_state};
pub use people::{
    Assigned, Retired, SLOT_BATCH, SLOT_LOCK_CLASS, SlotAssignment, SlotError, SlotStep,
    analyze_slot_column, assign_trait_slot, backfill_slot_batch, clear_slot_batch, finish_backfill,
    free_trait_slot, hold_trait_slots, ready_trait_slots, retire_trait_slot,
    trait_slot_assignments, trait_slot_of,
};
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
    BuildOutcome, DeleteReason, ERASE_CALL_ATTEMPTS, ERASE_CALL_JOB_KIND, ERASE_JOB_KIND,
    EXPORT_EXCLUDED, EXPORT_FORMAT, EXPORT_JOB_KIND, EXPORT_MAX_BYTES_DEFAULT,
    EXPORT_MAX_BYTES_MAX, EXPORT_MAX_BYTES_MIN, EXPORT_SECTIONS, EXPORT_TOO_LARGE,
    EXTERNAL_KEY_FIELD, EraseCallOutcome, EraseOutcome, EraseTarget, ErasureCall, ErasureCursor,
    ErasureRequest, ErasureStatus, ExportCursor, ExportDownload, ExportError, ExportReport,
    ExportStatus, ExternalKey, LEDGER_FORMAT, LedgerEntry, LedgerFileError, LedgerWrite,
    MISSING_KEY, MergeError, MergeReport, PrivacyExport, ReplayReport, build_export,
    check_ledger_dir, delete_person, download_export, erase_person, expire_exports, export_ledger,
    find_erasure_request, find_ledger_entry, find_privacy_export, finish_erasure_call,
    idempotency_key, list_erasure_calls, list_erasure_requests, list_privacy_exports,
    lock_person_call_shared, merge_ledger_entries, read_ledger_dir, reconcile_ledger_file,
    replay_erasures, request_erasure, request_export, update_ledger_lists, write_ledger_file,
};
#[cfg(any(test, feature = "test-support"))]
pub use segments::run_segment_ids;
pub use segments::{
    CADENCE_MINUTES, CatalogDetails, CatalogSnapshot, ClearFields, CompiledCondition,
    DEFAULT_CADENCE_MINUTES, DataAge, DirtyBatch, EXPLAIN_EVENT_CAP, EventDetail, ExplainConsent,
    ExplainEvent, ExplainEventScope, ExplainPerson, FeedingSource, HistoryCursor, HistoryRow,
    MemberRow, MembershipCause, NewSegment, NewSegmentVersion, PropertyDetail, REFRESH_JOB_KIND,
    REFRESH_LOCK_CLASS, ReevaluateApply, RefreshApply, RefreshOutcome, RefreshRequest,
    RefreshTrigger, SampleRow, Segment, SegmentError, SegmentPatch, SegmentRef, SegmentVersion,
    SqlParam, SqlQuery, TopicDetail, TraitDetail, apply_reevaluation, apply_refresh,
    archive_segment, begin_segment_snapshot, create_segment, decision_cause_kind, enqueue_refresh,
    evaluate_conditions, explain_clear_fields, find_segment, find_segment_version,
    list_segment_versions, list_segments, load_explain_person, lock_reevaluation,
    lock_segment_refresh, lock_segments, membership_cause, membership_trace, read_segment_catalog,
    reconcile_segment_schedules, reevaluation_order, run_person_batch, run_segment_preview,
    segment_catalog, segment_catalog_details, segment_cron, segment_data_age, segment_history_page,
    segment_members_page, snapshot_now, update_segment,
};
pub use send_limits::{
    CAP_LOCK_CLASS, CapMode, CapVerdict, FrequencyCap, NewFrequencyCap, QuietHours, SendLimits,
    check_frequency_caps, read_send_limits, record_message_deferral, replace_send_limits,
};
#[cfg(any(test, feature = "test-support"))]
pub use sending::fixtures as sending_fixtures;
pub use sending::{
    DomainCheck, DomainDelete, DomainRecord, IdentityError, NewSenderIdentity, NewSendingDomain,
    RecordFinding, SENDING_DOMAIN_VERIFY_KIND, SenderError, SenderIdentity, SenderIdentityPatch,
    SendingDomain, VerifiedSender, archive_sender_identity, create_sender_identity,
    create_sender_identity_in, create_sending_domain, delete_sending_domain, due_sending_domains,
    enqueue_domain_verify, find_sender_identity, find_sending_domain, list_domain_records,
    list_sender_identities, list_sending_domains, list_workspace_domain_records,
    record_domain_check, require_verified_sender, update_sender_identity,
};
pub use sessions::{
    ActiveSession, create_session, delete_expired_sessions, delete_session,
    delete_sessions_for_user, find_active_session, touch_session,
};
pub use snippets::{
    LockedSnippet, NewSnippet, Snippet, SnippetDraft, SnippetError, SnippetPatch, SnippetTemplate,
    SnippetThemeUse, SnippetUse, SnippetVersion, SnippetVersionSummary, archive_snippet,
    create_snippet, find_snippet, find_snippet_draft, find_snippet_version, list_snippet_versions,
    list_snippets, lock_snippet, publish_snippet, save_snippet_draft, snippet_live_templates,
    snippet_template_counts, snippet_where_used, update_snippet,
};
pub use sources::{
    DiscoveredStream, NewOAuthState, NewSource, OAuthState, RefreshError, Source, SourceUpdate,
    StoredTokens, Stream, StreamSettings, TokenError, clear_tokens, configure_stream,
    create_oauth_state, create_source, delete_expired_oauth_states, delete_source, find_source,
    find_source_for_update, find_stream, find_stream_by_id, find_stream_for_update, link_tokens,
    list_sources, list_streams, load_tokens, refresh_serialized, set_stream_mapping,
    sync_reconcile_schedule, sync_schedule, sync_source_reconcile_schedules, take_oauth_state,
    update_source, upsert_discovered_streams,
};
pub use sqlx::PgConnection;
pub use stream_freshness::{
    StreamFreshness, StreamOutcome, record_stream_outcome, stream_freshness,
};
pub use stream_offsets::{
    LagReport, OffsetsStored, PartitionLag, PartitionOffset, commit_stream_offsets, load_offsets,
    report_stream_lag, stream_lag,
};
pub use suppressions::{
    AddOutcome, BatchEntry, NewSuppression, RemoveOutcome, SUPPRESSION_KINDS, Suppression,
    SuppressionCursor, SuppressionScope, add_organization_suppression, add_suppression,
    add_suppressions, count_suppressed, find_suppression, is_suppressed, list_suppressions,
    remove_suppression, suppressing_scope,
};
pub use sync_runs::{
    BatchOutcome as SyncBatchOutcome, BatchProgress as SyncBatchProgress,
    CONSUME_JOB_KIND as SYNC_CONSUME_JOB_KIND, CancelOutcome, Counts as SyncCounts,
    DELETE_BY_SIGNAL as SYNC_DELETE_BY_SIGNAL, DELETE_BY_SOURCE as SYNC_DELETE_BY_SOURCE,
    EventCounts as SyncEventCounts, FanOut, Lease as SyncLease, RUN_JOB_KIND as SYNC_RUN_JOB_KIND,
    ReconcileQueued, Rotated as SyncRotated, RunCursor, RunEnd, RunKind, RunReject, RunTrigger,
    SYNC_REJECT_CHECK_MIN_RECORDS, SYNC_SWEEP_BATCH, SweepOutcome, SweepUnit, SyncBatch, SyncEvent,
    SyncReject, SyncRun, SyncUpsert, cancel_consumer_runs, cancel_run, commit_sync_batch,
    consume_unique_key as sync_consume_unique_key, fail_orphaned_runs, fail_run, find_run,
    finish_run, list_run_rejects, list_runs, queue_reconcile_run, queue_run, queue_runs_for_source,
    rotate_stream_run, run_trace, start_run, start_stream_run, sweep_exceeds_limit,
    sweep_unreached,
};
pub use templates::{
    Candidate, ExtrasRequest, LiveTemplate, LoadedInputs, LoadedSnippet, LockedTemplate,
    MAX_PREVIEW_PEOPLE, NOTE_MAX_CHARS as TEMPLATE_NOTE_MAX_CHARS, NewTemplate, PreviewPerson,
    PreviewPersonError, Template, TemplateDraft, TemplateError, TemplateFieldCatalog,
    TemplateFlowUse, TemplateListQuery, TemplatePatch, TemplateSendVolume, TemplateSort,
    TemplateStatus, TemplateSummary, TemplateTheme, TemplateThemeFilter, TemplateUse,
    TemplateVersion, TemplateVersionStats, TemplateVersionSummary, ThemeRequest, VersionRender,
    active_flows_using, active_published_flows, add_preview_person, archive_template,
    create_template, document_sender, find_template, find_template_draft, find_template_version,
    list_preview_people, list_template_versions, list_templates, load_render_inputs,
    load_render_inputs_with, lock_template, lock_templates_shared, publish_template,
    read_template_field_catalog, remove_preview_person, save_template_draft,
    set_template_draft_checks, template_active_flow_counts, template_send_volume,
    template_version_stats, template_where_used, update_template,
};
pub use test_sends::{
    ABANDONED as TEST_SEND_ABANDONED, AllowlistEntry, ListedTestSend, MEMBER_HOURLY_LIMIT,
    NewTestSend, QuotaUsage, REMOVED_ADDRESS as TEST_SEND_REMOVED_ADDRESS, TEST_SEND_JOB_KIND,
    TEST_SEND_RETENTION_MONTHS, TestSend, TestSendCursor, TestSendOutcome, TestSendQuota,
    TestSendReservation, WORKSPACE_HOURLY_LIMIT, abandon_orphaned_test_sends, add_allowlist_entry,
    delete_allowlist_by_hash, delete_allowlist_entry, delete_test_sends_before,
    delete_test_sends_by_hash, find_allowlist_entry, find_allowlist_target, find_test_send,
    finish_test_send, is_test_send, list_allowlist, list_test_sends, masked_hint,
    open_allowlist_address, record_test_send_versions, reserve_test_send,
    set_test_send_draft_revision, test_send_may_go, test_send_quota,
};
pub use themes::{
    EffectiveTheme, LockedTheme, NewTheme, Theme, ThemeChoice, ThemeDraft, ThemeError, ThemePatch,
    ThemeTemplate, ThemeUse, ThemeVersion, ThemeVersionSummary, archive_theme, create_theme,
    effective_theme, find_theme, find_theme_draft, find_theme_version, footer_snippet_usable,
    list_theme_tokens, list_theme_versions, list_themes, lock_theme, publish_theme,
    save_theme_draft, set_default_theme, theme_live_templates, theme_template_counts,
    theme_where_used, update_theme,
};
pub use tracking::{
    Engagement, PreferenceChange, PreferencesSaved, TrackedMessage, Unsubscribed,
    find_message_link, find_message_links, find_tracked_message, record_engagement,
    record_message_links, save_preferences, unsubscribe_message,
};
pub use webhooks::{
    HookReceive, NewWebhook, Webhook, WebhookUpdate, append_webhook, create_webhook,
    find_for_receive, find_webhook, list_webhooks, replace_webhook_secret, update_webhook,
};
#[cfg(any(test, feature = "test-support"))]
pub use workspace_credentials::import_secret_as_credential;
pub use workspace_credentials::{
    ArchiveOutcome, CredentialUpdate, CredentialUse, CredentialUser, NewWorkspaceCredential,
    OpenedCredential, WorkspaceCredential, archive_credential, create_credential,
    credential_use_counts, credential_users, credential_uses, find_credential,
    find_credential_for_update, list_credentials, lock_credential_for_use, open_credential,
    record_credential_test, swap_credential_secret, update_credential,
};
pub use workspaces::{
    Member, Membership, UtmSettings, Workspace, create_workspace, find_workspace,
    find_workspace_by_slug, list_members, list_workspaces_for_user, membership_role, orphan_policy,
    remove_membership, set_membership, set_orphan_policy, set_workspace_timezone,
    set_workspace_utm,
};
/// The operator master key, its derived keys and the sealing of secrets, and the workspace and role
/// types the test server seeds with, re-exported so the
/// binary reaches them through the store.
pub use zirv_flow_core::{
    HmacKey, HmacPurpose, HmacRoot, Keyring, MasterKey, Role, Scope, SecretBytes, SecretId,
    SecretPurpose, Slug, WorkspaceId, WorkspaceName, deployment_key_aad, open as open_secret,
    seal as seal_secret, secret_aad,
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
    pub fn into_bounded(self) -> Self {
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
