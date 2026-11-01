//! Export v1 (architecture section 16.3): one gzip-compressed JSON document of everything held
//! about a person, kept for 24 hours.
//!
//! An export is the one place personal values leave the system, so the API shows it to
//! workspace admins only. Nothing here logs, labels or audits a value.

use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;
use futures_util::{Stream, TryStreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, WorkspaceId};

use crate::{NewJob, PgPool, StoreError, current_person, enqueue};

/// The job kind that builds an export.
pub const EXPORT_JOB_KIND: &str = "privacy.export";

/// The `format` of the document.
pub const EXPORT_FORMAT: &str = "zirv-flow.person-export/v1";

/// Events are read this many at a time.
const EVENT_BATCH: i64 = 1000;

/// The default of `privacy.export_max_bytes`: 256 MiB of uncompressed JSON.
pub const EXPORT_MAX_BYTES_DEFAULT: u64 = 256 * 1024 * 1024;

/// The bounds of `privacy.export_max_bytes`: 1 MiB to 4 GiB.
pub const EXPORT_MAX_BYTES_MIN: u64 = 1024 * 1024;
pub const EXPORT_MAX_BYTES_MAX: u64 = 4 * 1024 * 1024 * 1024;

/// The code an export shows when it grew past the cap and was stopped.
pub const EXPORT_TOO_LARGE: &str = "export_too_large";

/// The sections of the document, each with the tables it reads. Every table that holds a person
/// is here or in [`EXPORT_EXCLUDED`]; a catalog test fails on one that is in neither.
pub const EXPORT_SECTIONS: &[(&str, &[&str])] = &[
    ("profile", &["people"]),
    ("identities", &["identities"]),
    ("source_records", &["source_records"]),
    ("events", &["events"]),
    ("consent", &["consent"]),
    ("consent_history", &["consent_changes"]),
    ("lists", &["list_members"]),
    ("segments", &["segment_members"]),
    ("list_history", &["list_member_changes"]),
    ("merges", &["person_merges"]),
    ("identity_conflicts", &["identity_conflicts"]),
    ("enrollments", &["enrollments"]),
    ("step_executions", &["step_executions"]),
    ("enrollment_waits", &["enrollment_waits"]),
    ("flow_entries", &["flow_entries"]),
    ("messages", &["messages"]),
    ("message_links", &["message_links"]),
    ("forwarding_outbox", &["forwarding_outbox"]),
    ("erasure_handled_markers", &["erasure_handled_markers"]),
    ("ingest_rejects", &["ingest_rejects"]),
    ("decision_records", &["decision_records"]),
    ("template_preview_people", &["template_preview_people"]),
];

/// Tables that hold or name a person and are deliberately not exported, with the reason.
pub const EXPORT_EXCLUDED: &[(&str, &str)] = &[
    ("person_changes", "internal change feed; ids and times only"),
    (
        "flow_warmup_waiting",
        "ids and a place in the window of an entry not admitted yet; it joins flow_entries when admitted",
    ),
    ("privacy_exports", "the export itself"),
    ("event_dedupe", "message ids only"),
    ("debugger_feed", "a 15-minute buffer of event ids"),
    (
        "jobs",
        "payload copies of data already exported; purged on erase",
    ),
    (
        "ai_transcripts",
        "free text matched by needle on erase only, so an export cannot find it",
    ),
    ("suppressions", "the ledger; kept on purpose"),
    ("erasure_requests", "the ledger; kept on purpose"),
];

/// Where an export stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportStatus {
    Pending,
    Ready,
    /// The job ran out of attempts, or the document passed the size cap.
    Failed,
    /// The configured window (`retention.export_hours`) is over and the content is gone.
    Expired,
}

impl ExportStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Expired => "expired",
        }
    }

    fn parse(text: &str) -> Result<Self, StoreError> {
        match text {
            "pending" => Ok(Self::Pending),
            "ready" => Ok(Self::Ready),
            "failed" => Ok(Self::Failed),
            "expired" => Ok(Self::Expired),
            _ => Err(StoreError::query(sqlx::Error::Decode(
                "unknown export status".into(),
            ))),
        }
    }
}

/// An export as listed: never its content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrivacyExport {
    pub id: Uuid,
    pub person_id: Uuid,
    pub status: ExportStatus,
    pub requested_by: Option<Uuid>,
    pub requested_at: OffsetDateTime,
    pub ready_at: Option<OffsetDateTime>,
    pub expires_at: Option<OffsetDateTime>,
    pub size_bytes: Option<i64>,
    pub trace_id: Uuid,
    /// Why the export failed, when the build knows: [`EXPORT_TOO_LARGE`]. A job that ran out of
    /// attempts carries none.
    pub failure_code: Option<String>,
}

#[derive(FromRow)]
struct ExportRow {
    id: Uuid,
    person_id: Uuid,
    status: String,
    requested_by: Option<Uuid>,
    requested_at: OffsetDateTime,
    ready_at: Option<OffsetDateTime>,
    expires_at: Option<OffsetDateTime>,
    size_bytes: Option<i64>,
    trace_id: Uuid,
    failure_code: Option<String>,
}

impl ExportRow {
    fn into_export(self) -> Result<PrivacyExport, StoreError> {
        Ok(PrivacyExport {
            id: self.id,
            person_id: self.person_id,
            status: ExportStatus::parse(&self.status)?,
            requested_by: self.requested_by,
            requested_at: self.requested_at,
            ready_at: self.ready_at,
            expires_at: self.expires_at,
            size_bytes: self.size_bytes,
            trace_id: self.trace_id,
            failure_code: self.failure_code,
        })
    }
}

/// The columns of [`PrivacyExport`]. A row stored as failed was stopped by the size cap, the only
/// failure the build records itself. A pending export whose job went dead reads as failed,
/// and a ready one past its expiry as expired, even before the hourly upkeep has run.
const EXPORT_COLUMNS: &str = "x.id, x.person_id, \
    CASE WHEN x.status = 'pending' AND EXISTS ( \
            SELECT 1 FROM jobs j WHERE j.state = 'dead' \
              AND j.unique_key = 'privacy.export:' || x.id::text) THEN 'failed' \
         WHEN x.status = 'ready' AND x.expires_at <= now() THEN 'expired' \
         ELSE x.status END AS status, \
    x.requested_by, x.requested_at, x.ready_at, x.expires_at, x.size_bytes, x.trace_id, \
    CASE WHEN x.status = 'failed' THEN 'export_too_large' END AS failure_code";

/// Where the next page of exports starts: strictly before this `(requested_at, id)`.
pub type ExportCursor = (OffsetDateTime, Uuid);

/// Records an export request for the person (or their survivor) and enqueues the job, in the
/// caller's transaction. `None` when the workspace has no such person.
pub async fn request_export(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    requested_by: Option<Uuid>,
    trace: &TraceContext,
) -> Result<Option<PrivacyExport>, StoreError> {
    let Some(current) = current_person(&mut *connection, workspace, person).await? else {
        return Ok(None);
    };
    let id = Uuid::now_v7();
    let row: ExportRow = sqlx::query_as(
        "INSERT INTO privacy_exports (id, workspace_id, person_id, requested_by, trace_id) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, person_id, status, requested_by, requested_at, ready_at, expires_at, \
                   size_bytes, trace_id, NULL::text AS failure_code",
    )
    .bind(id)
    .bind(workspace.as_uuid())
    .bind(current)
    .bind(requested_by)
    .bind(trace.trace_id.as_uuid())
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut job = NewJob::new(EXPORT_JOB_KIND, trace.clone());
    job.workspace_id = Some(workspace);
    job.payload = json!({ "workspace_id": workspace.as_uuid(), "export_id": id });
    job.unique_key = Some(format!("{EXPORT_JOB_KIND}:{id}"));
    enqueue(&mut *connection, &job).await?;
    row.into_export().map(Some)
}

/// One export of the workspace, without its content.
pub async fn find_privacy_export(
    pool: &PgPool,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<PrivacyExport>, StoreError> {
    let row: Option<ExportRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {EXPORT_COLUMNS} FROM privacy_exports x WHERE x.workspace_id = $1 AND x.id = $2"
    )))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(StoreError::query)?;
    row.map(ExportRow::into_export).transpose()
}

/// A page of the workspace's exports, newest first, at most `limit`, after the `before`
/// cursor. The flag is true when more exist.
pub async fn list_privacy_exports(
    pool: &PgPool,
    workspace: WorkspaceId,
    limit: u32,
    before: Option<ExportCursor>,
) -> Result<(Vec<PrivacyExport>, bool), StoreError> {
    let mut rows: Vec<ExportRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {EXPORT_COLUMNS} FROM privacy_exports x \
         WHERE x.workspace_id = $1 \
           AND ($2::timestamptz IS NULL OR (x.requested_at, x.id) < ($2, $3)) \
         ORDER BY x.requested_at DESC, x.id DESC LIMIT $4"
    )))
    .bind(workspace.as_uuid())
    .bind(before.map(|(at, _)| at))
    .bind(before.map(|(_, id)| id))
    .bind(i64::from(limit) + 1)
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let exports = rows
        .into_iter()
        .map(ExportRow::into_export)
        .collect::<Result<_, _>>()?;
    Ok((exports, more))
}

/// What [`build_export`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildOutcome {
    /// The document was built and stored; it expires after the configured window (`retention.export_hours`).
    Built { size_bytes: i64 },
    /// An earlier attempt built it, or it expired since.
    NotPending,
    /// The workspace has no such export, or its person is gone.
    NoSuchExport,
    /// The document passed the size cap; the export is stored as failed, with no content.
    TooLarge,
}

/// Builds the export in the caller's transaction and marks it ready. The uncompressed document
/// may hold at most `max_bytes`; collection stops at the row that passes it and the export is
/// stored as failed, with no content.
pub async fn build_export(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    export: Uuid,
    max_bytes: u64,
    export_hours: u32,
) -> Result<BuildOutcome, StoreError> {
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT person_id, status FROM privacy_exports \
         WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(export)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let Some((person, status)) = row else {
        return Ok(BuildOutcome::NoSuchExport);
    };
    if status != ExportStatus::Pending.as_str() {
        return Ok(BuildOutcome::NotPending);
    }
    let document = match person_document(connection, workspace, person, max_bytes).await {
        Ok(Some(document)) => document,
        Ok(None) => return Ok(BuildOutcome::NoSuchExport),
        Err(Abort::Store(error)) => return Err(error),
        Err(Abort::TooLarge) => {
            sqlx::query(
                "UPDATE privacy_exports SET status = 'failed', content = NULL, size_bytes = NULL \
                 WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace.as_uuid())
            .bind(export)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
            return Ok(BuildOutcome::TooLarge);
        }
    };
    let compressed = gzip(&document)?;
    let size_bytes = i64::try_from(compressed.len()).unwrap_or(i64::MAX);
    sqlx::query(
        "UPDATE privacy_exports SET status = 'ready', content = $3, size_bytes = $4, \
            ready_at = now(), expires_at = now() + make_interval(hours => $5) \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(export)
    .bind(compressed)
    .bind(size_bytes)
    .bind(i32::try_from(export_hours).unwrap_or(i32::MAX))
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(BuildOutcome::Built { size_bytes })
}

/// What a download finds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExportDownload {
    /// The gzip-compressed JSON document.
    Content(Vec<u8>),
    /// The export is past its configured window, or already marked expired.
    Expired,
    /// The export is not ready: it is pending, or its job failed.
    NotReady,
    NotFound,
}

/// The content of an export, unless it expired. Expiry is judged by the database's clock at
/// the moment of the call, so an export is gone at its time even if the hourly upkeep has
/// not yet dropped the content.
pub async fn download_export(
    pool: &PgPool,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<ExportDownload, StoreError> {
    let row: Option<(String, bool, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT status, COALESCE(expires_at <= now(), false), content FROM privacy_exports \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(StoreError::query)?;
    Ok(match row {
        None => ExportDownload::NotFound,
        Some((status, _, _)) if status == ExportStatus::Expired.as_str() => ExportDownload::Expired,
        Some((status, past, content)) if status == ExportStatus::Ready.as_str() => match content {
            Some(content) if !past => ExportDownload::Content(content),
            _ => ExportDownload::Expired,
        },
        Some(_) => ExportDownload::NotReady,
    })
}

/// Marks the ready exports past their expiry as expired and drops their content. Run by the
/// leader's hourly upkeep. Returns how many expired.
pub async fn expire_exports(pool: &PgPool) -> Result<u64, StoreError> {
    let done = sqlx::query(
        "UPDATE privacy_exports SET status = 'expired', content = NULL \
         WHERE status = 'ready' AND expires_at <= now()",
    )
    .execute(pool)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected())
}

fn gzip(document: &Value) -> Result<Vec<u8>, StoreError> {
    let bytes = serde_json::to_vec(document)
        .map_err(|error| StoreError::query(sqlx::Error::Encode(error.into())))?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&bytes)
        .and_then(|()| encoder.finish())
        .map_err(|error| StoreError::query(sqlx::Error::Io(error)))
}

/// Why collecting a document stopped.
#[derive(Debug)]
enum Abort {
    Store(StoreError),
    /// The rows read so far passed the cap.
    TooLarge,
}

impl From<StoreError> for Abort {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<serde_json::Error> for Abort {
    fn from(error: serde_json::Error) -> Self {
        Self::Store(StoreError::query(sqlx::Error::Encode(error.into())))
    }
}

/// Counts the JSON bytes of the rows a document takes, so collection stops at the cap.
struct Budget {
    max: u64,
    used: u64,
}

impl Budget {
    fn new(max: u64) -> Self {
        Self { max, used: 0 }
    }

    fn take(&mut self, row: &Value) -> Result<(), Abort> {
        let mut counter = Counter(0);
        // Counting cannot fail.
        let _ = serde_json::to_writer(&mut counter, row);
        self.used = self.used.saturating_add(counter.0).saturating_add(1);
        if self.used > self.max {
            return Err(Abort::TooLarge);
        }
        Ok(())
    }
}

struct Counter(u64);

impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The export document of the person's group (the survivor and the people merged into them),
/// or none when the person is gone.
async fn person_document(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    max_bytes: u64,
) -> Result<Option<Value>, Abort> {
    let Some(current) = current_person(&mut *connection, workspace, person).await? else {
        return Ok(None);
    };
    let people: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM people WHERE workspace_id = $1 AND (id = $2 OR merged_into = $2) \
         ORDER BY id",
    )
    .bind(workspace.as_uuid())
    .bind(current)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let merged: Vec<&Uuid> = people.iter().filter(|id| **id != current).collect();

    let generated_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|error| StoreError::query(sqlx::Error::Encode(error.into())))?;
    let mut budget = Budget::new(max_bytes);
    // The empty document carries every key, delimiter and the envelope; the rows and the merged
    // ids are charged on top of it.
    budget.take(&serde_json::to_value(ExportDocument::empty(
        &generated_at,
        current,
    ))?)?;
    budget.take(&json!(merged))?;
    let mut section = Section {
        connection,
        workspace,
        people: &people,
        budget,
    };
    let profile = section
        .rows(
            "SELECT traits, trait_sources, created_at, updated_at FROM people \
             WHERE workspace_id = $1 AND id = ANY($2) AND merged_into IS NULL",
        )
        .await?
        .into_iter()
        .next()
        .unwrap_or(Value::Null);
    let identities = section
        .rows(
            "SELECT id, kind, value, created_at FROM identities \
             WHERE workspace_id = $1 AND person_id = ANY($2) ORDER BY kind, value",
        )
        .await?;
    let source_records = section
        .rows(
            "SELECT id, source, stream, record_key, identifiers, traits, created_at, updated_at, \
                    deleted_at \
             FROM source_records WHERE workspace_id = $1 AND person_id = ANY($2) \
             ORDER BY created_at, id",
        )
        .await?;
    let events = section.events().await?;
    let consent = section
        .rows(
            "SELECT c.channel, t.key AS topic, c.dimensions, c.status, c.explicit, c.changed_at \
             FROM consent c LEFT JOIN topics t \
                ON t.workspace_id = c.workspace_id AND t.id = c.topic_id \
             WHERE c.workspace_id = $1 AND c.person_id = ANY($2) \
             ORDER BY c.channel, t.key NULLS FIRST",
        )
        .await?;
    let consent_history = section
        .rows(
            "SELECT c.id, c.channel, t.key AS topic, c.dimensions, c.from_status, c.to_status, \
                    c.source, c.evidence, c.occurred_at, c.actor_user_id, c.trace_id \
             FROM consent_changes c LEFT JOIN topics t \
                ON t.workspace_id = c.workspace_id AND t.id = c.topic_id \
             WHERE c.workspace_id = $1 AND c.person_id = ANY($2) \
             ORDER BY c.occurred_at, c.id",
        )
        .await?;
    let lists = section
        .rows(
            "SELECT l.id AS list_id, l.name, m.added_at, m.source \
             FROM list_members m JOIN lists l ON l.id = m.list_id \
             WHERE m.workspace_id = $1 AND m.person_id = ANY($2) \
             ORDER BY lower(l.name), l.id",
        )
        .await?;
    let segments = section
        .rows(
            "SELECT s.id AS segment_id, s.name, m.version, m.entered_at, m.entered_cause, \
                    m.exited_version, m.exited_at, m.exited_cause \
             FROM segment_members m JOIN segments s ON s.id = m.segment_id \
             WHERE m.workspace_id = $1 AND m.person_id = ANY($2) \
             ORDER BY lower(s.name), s.id, m.entered_at, m.id",
        )
        .await?;
    let list_history = section
        .rows(
            "SELECT c.id, c.list_id, l.name AS list_name, c.action, c.source, c.occurred_at, \
                    c.actor_user_id, c.trace_id \
             FROM list_member_changes c JOIN lists l ON l.id = c.list_id \
             WHERE c.workspace_id = $1 AND c.person_id = ANY($2) \
             ORDER BY c.occurred_at, c.id",
        )
        .await?;
    let merges = section
        .rows(
            "SELECT id, survivor_person_id, merged_person_id, identity_ids, source_record_ids, \
                    trace_id, created_at \
             FROM person_merges WHERE workspace_id = $1 \
               AND (survivor_person_id = ANY($2) OR merged_person_id = ANY($2)) \
             ORDER BY created_at, id",
        )
        .await?;
    let identity_conflicts = section
        .rows(
            "SELECT id, person_id, other_person_id, kind, source_record_id, identity_id, reason, \
                    created_at, resolved_at, resolution \
             FROM identity_conflicts WHERE workspace_id = $1 \
               AND (person_id = ANY($2) OR other_person_id = ANY($2)) \
             ORDER BY created_at, id",
        )
        .await?;

    let enrollments = section
        .rows(
            "SELECT id, flow_id, flow_version, person_id, state, node, context, occurrence_id, \
                    wake_at, exit_reason, failed_node, cause_kind, cause_id, trace_id, \
                    created_at, updated_at, ended_at \
             FROM enrollments WHERE workspace_id = $1 AND person_id = ANY($2) \
             ORDER BY created_at, id",
        )
        .await?;
    let step_executions = section
        .rows(
            "SELECT s.id, s.enrollment_id, s.node_id, s.outcome, s.attempts, s.started_at \
             FROM step_executions s JOIN enrollments e ON e.id = s.enrollment_id \
             WHERE e.workspace_id = $1 AND e.person_id = ANY($2) \
             ORDER BY s.started_at, s.id",
        )
        .await?;
    let enrollment_waits = section
        .rows(
            "SELECT enrollment_id, person_id, event_name, filters, wake_version, timeout_at \
             FROM enrollment_waits WHERE workspace_id = $1 AND person_id = ANY($2) \
             ORDER BY timeout_at, enrollment_id, event_name",
        )
        .await?;
    let flow_entries = section
        .rows(
            "SELECT flow_id, person_id, entries, last_entered_at \
             FROM flow_entries WHERE workspace_id = $1 AND person_id = ANY($2) \
             ORDER BY last_entered_at, flow_id",
        )
        .await?;
    let messages = section
        .rows(
            "SELECT m.id, m.person_id, m.enrollment_id, m.flow_id, m.flow_version, m.node_id, \
                    m.template_id, m.template_version, m.destination_id, m.sender_identity_id, \
                    m.category, m.channel, m.topic_id, t.key AS topic, m.dimensions, m.status, \
                    m.reason, m.deciding_rule, m.gate_results, m.test, m.attempts, \
                    m.created_at, m.completed_at \
             FROM messages m LEFT JOIN topics t \
                ON t.workspace_id = m.workspace_id AND t.id = m.topic_id \
             WHERE m.workspace_id = $1 AND m.person_id = ANY($2) \
             ORDER BY m.created_at, m.id",
        )
        .await?;
    let message_links = section
        .rows(
            "SELECT l.message_id, l.link_index, l.url \
             FROM message_links l JOIN messages m \
                ON m.workspace_id = l.workspace_id AND m.id = l.message_id \
             WHERE m.workspace_id = $1 AND m.person_id = ANY($2) \
             ORDER BY m.created_at, l.message_id, l.link_index",
        )
        .await?;
    let forwarding_outbox = section
        .rows(
            "SELECT id, destination_id, rule_id, event_id, person_id, idempotency_key, payload, \
                    state, attempts, last_error, next_attempt_at, created_at, updated_at \
             FROM forwarding_outbox WHERE workspace_id = $1 AND person_id = ANY($2) \
             ORDER BY created_at, id",
        )
        .await?;
    let erasure_handled_markers = section
        .rows(
            "SELECT person_id, destination_id, first_handled_at \
             FROM erasure_handled_markers WHERE workspace_id = $1 AND person_id = ANY($2) \
             ORDER BY first_handled_at, destination_id",
        )
        .await?;
    let ingest_rejects = section
        .rows(
            "SELECT id, person_id, received_at, source, kind, name, reason, reasons, payload, \
                    message_id, trace_id, replayed_at, replay_event_id \
             FROM ingest_rejects WHERE workspace_id = $1 AND person_id = ANY($2) \
             ORDER BY received_at, id",
        )
        .await?;
    // Read through the partial person index of migration 0012.
    let decision_records = section
        .rows(
            "SELECT id, trace_id, recorded_at, action, outcome, reason, cause_kind, cause_id, \
                    subject_ids, job_id, attempt \
             FROM decision_records WHERE workspace_id = $1 AND subject_ids ? 'person' \
               AND subject_ids ->> 'person' = ANY($2::text[]) \
             ORDER BY recorded_at, id",
        )
        .await?;
    let template_preview_people = section
        .rows(
            "SELECT template_id, person_id, added_by, created_at \
             FROM template_preview_people WHERE workspace_id = $1 AND person_id = ANY($2) \
             ORDER BY created_at, template_id",
        )
        .await?;

    let document = ExportDocument {
        format: EXPORT_FORMAT,
        generated_at: &generated_at,
        person: ExportPerson {
            id: current,
            merged_ids: merged,
            profile,
            sections: Sections {
                identities,
                source_records,
                events,
                consent,
                consent_history,
                lists,
                segments,
                list_history,
                merges,
                identity_conflicts,
                enrollments,
                step_executions,
                enrollment_waits,
                flow_entries,
                messages,
                message_links,
                forwarding_outbox,
                erasure_handled_markers,
                ingest_rejects,
                decision_records,
                template_preview_people,
            },
        },
    };
    Ok(Some(serde_json::to_value(document)?))
}

/// The shape of the document. The budget charges the empty one, so a key can never go uncounted.
#[derive(Serialize)]
struct ExportDocument<'a> {
    format: &'static str,
    generated_at: &'a str,
    person: ExportPerson<'a>,
}

#[derive(Serialize)]
struct ExportPerson<'a> {
    id: Uuid,
    merged_ids: Vec<&'a Uuid>,
    profile: Value,
    #[serde(flatten)]
    sections: Sections,
}

#[derive(Default, Serialize)]
struct Sections {
    identities: Vec<Value>,
    source_records: Vec<Value>,
    events: Vec<Value>,
    consent: Vec<Value>,
    consent_history: Vec<Value>,
    lists: Vec<Value>,
    segments: Vec<Value>,
    list_history: Vec<Value>,
    merges: Vec<Value>,
    identity_conflicts: Vec<Value>,
    enrollments: Vec<Value>,
    step_executions: Vec<Value>,
    enrollment_waits: Vec<Value>,
    flow_entries: Vec<Value>,
    messages: Vec<Value>,
    message_links: Vec<Value>,
    forwarding_outbox: Vec<Value>,
    erasure_handled_markers: Vec<Value>,
    ingest_rejects: Vec<Value>,
    decision_records: Vec<Value>,
    template_preview_people: Vec<Value>,
}

impl<'a> ExportDocument<'a> {
    fn empty(generated_at: &'a str, person: Uuid) -> Self {
        Self {
            format: EXPORT_FORMAT,
            generated_at,
            person: ExportPerson {
                id: person,
                merged_ids: Vec::new(),
                profile: Value::Null,
                sections: Sections::default(),
            },
        }
    }
}

/// Reads sections of one person's group.
struct Section<'a> {
    connection: &'a mut PgConnection,
    workspace: WorkspaceId,
    people: &'a [Uuid],
    budget: Budget,
}

impl Section<'_> {
    /// Each row of `select` (which binds `$1` the workspace and `$2` the people) as JSON. Rows
    /// are counted as they arrive, so a section past the cap stops being read.
    async fn rows(&mut self, select: &str) -> Result<Vec<Value>, Abort> {
        // The statements are constants of this module, never built from input.
        let sql = format!("SELECT to_jsonb(t) FROM ({select}) t");
        let mut stream = sqlx::query_scalar::<_, Value>(sqlx::AssertSqlSafe(sql))
            .bind(self.workspace.as_uuid())
            .bind(self.people)
            .fetch(&mut *self.connection);
        let mut rows = Vec::new();
        while let Some(row) = stream.try_next().await.map_err(StoreError::query)? {
            self.budget.take(&row)?;
            rows.push(row);
        }
        Ok(rows)
    }

    /// Every event of every partition, oldest first, read in keyset batches. Each row is charged
    /// as it arrives, so a person past the cap is never read in full.
    async fn events(&mut self) -> Result<Vec<Value>, Abort> {
        let mut events = Vec::new();
        let mut after: Option<(OffsetDateTime, Uuid)> = None;
        loop {
            let mut stream = sqlx::query_as::<_, EventRow>(
                "SELECT occurred_at, id, to_jsonb(t) FROM ( \
                    SELECT id, name, schema_version, occurred_at, received_at, properties, \
                           source, message_id, trace_id, validation_state, validation_reasons, \
                           flags \
                    FROM events WHERE workspace_id = $1 AND person_id = ANY($2) \
                      AND ($3::timestamptz IS NULL OR (occurred_at, id) > ($3, $4)) \
                    ORDER BY occurred_at, id LIMIT $5) t \
                 ORDER BY occurred_at, id",
            )
            .bind(self.workspace.as_uuid())
            .bind(self.people)
            .bind(after.map(|(at, _)| at))
            .bind(after.map(|(_, id)| id))
            .bind(EVENT_BATCH)
            .fetch(&mut *self.connection);
            let in_batch =
                consume_events(&mut stream, &mut self.budget, &mut events, &mut after).await?;
            if in_batch < EVENT_BATCH {
                return Ok(events);
            }
        }
    }
}

type EventRow = (OffsetDateTime, Uuid, Value);

/// Charges and collects events from `stream` until it ends or the budget is spent. Stops polling
/// at the first row past the cap. Returns how many events it took.
async fn consume_events<S>(
    stream: &mut S,
    budget: &mut Budget,
    events: &mut Vec<Value>,
    after: &mut Option<(OffsetDateTime, Uuid)>,
) -> Result<i64, Abort>
where
    S: Stream<Item = Result<EventRow, sqlx::Error>> + Unpin,
{
    let mut taken = 0;
    while let Some((at, id, event)) = stream.try_next().await.map_err(StoreError::query)? {
        budget.take(&event)?;
        events.push(event);
        *after = Some((at, id));
        taken += 1;
    }
    Ok(taken)
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use flate2::read::GzDecoder;

    use futures_util::StreamExt;

    use super::*;
    use crate::privacy::testing::{
        BYSTANDER_EMAIL, EMAIL, Seeded, seed_everything, seed_export_extras, trace,
    };
    use crate::{begin, commit};

    async fn requested(pool: &PgPool, ws: WorkspaceId, person: Uuid) -> PrivacyExport {
        let mut tx = begin(pool).await.unwrap();
        let export = request_export(&mut tx, ws, person, None, &trace())
            .await
            .unwrap()
            .unwrap();
        commit(tx).await.unwrap();
        export
    }

    async fn built(pool: &PgPool, ws: WorkspaceId, export: Uuid) -> BuildOutcome {
        built_with_cap(pool, ws, export, EXPORT_MAX_BYTES_DEFAULT).await
    }

    async fn built_with_cap(
        pool: &PgPool,
        ws: WorkspaceId,
        export: Uuid,
        max_bytes: u64,
    ) -> BuildOutcome {
        let mut tx = begin(pool).await.unwrap();
        let outcome = build_export(&mut tx, ws, export, max_bytes, 24)
            .await
            .unwrap();
        commit(tx).await.unwrap();
        outcome
    }

    /// Fails, naming them, on a table with a person column that `sections` neither reads nor
    /// the exclusions deliberately leave out. The export twin of the erasure scan's catalog, over
    /// the same discovery.
    async fn check_catalog(pool: &PgPool, sections: &[(&str, &[&str])]) -> Result<(), String> {
        let mut tables: Vec<String> = crate::people::person_reference_places(pool)
            .await
            .into_iter()
            .map(|(table, _)| table)
            .collect();
        tables.dedup();
        let unclassified: Vec<String> = tables
            .into_iter()
            .filter(|table| {
                let read = sections
                    .iter()
                    .any(|(_, read)| read.contains(&table.as_str()));
                let left_out = EXPORT_EXCLUDED
                    .iter()
                    .any(|(left_out, _)| left_out == table);
                !read && !left_out
            })
            .collect();
        if unclassified.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "tables with a person column that the export neither reads nor excludes: {}",
                unclassified.join(", ")
            ))
        }
    }

    fn unzip(bytes: &[u8]) -> Value {
        let mut text = String::new();
        GzDecoder::new(bytes).read_to_string(&mut text).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn a_status_round_trips_and_an_unknown_one_is_refused() {
        for status in [
            ExportStatus::Pending,
            ExportStatus::Ready,
            ExportStatus::Failed,
            ExportStatus::Expired,
        ] {
            assert_eq!(ExportStatus::parse(status.as_str()).unwrap(), status);
        }
        assert!(ExportStatus::parse("burnt").is_err());
    }

    #[test]
    fn the_document_compresses_to_gzip_that_reads_back() {
        let document = json!({"format": EXPORT_FORMAT, "person": {"events": []}});

        let bytes = gzip(&document).unwrap();

        assert_eq!(&bytes[..2], [0x1f, 0x8b]);
        assert_eq!(unzip(&bytes), document);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_export_holds_every_section_of_the_person_and_the_merged_people(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let seeded = seed_everything(&pool).await;
            seed_export_extras(&pool, &seeded).await;
            let Seeded {
                ws, person, merged, ..
            } = seeded;
            let export = requested(&pool, ws, person).await;
            assert_eq!(export.status, ExportStatus::Pending);

            let outcome = built(&pool, ws, export.id).await;

            assert!(matches!(outcome, BuildOutcome::Built { .. }), "{outcome:?}");
            let ExportDownload::Content(bytes) =
                download_export(&pool, ws, export.id).await.unwrap()
            else {
                panic!("not ready");
            };
            let document = unzip(&bytes);
            assert_eq!(document["format"], EXPORT_FORMAT);
            assert!(document["generated_at"].is_string());
            let person_json = &document["person"];
            assert_eq!(person_json["id"], json!(person));
            assert_eq!(person_json["merged_ids"], json!([merged]));
            let count = |section: &str| person_json[section].as_array().unwrap().len();
            assert_eq!(count("identities"), 3);
            assert_eq!(count("source_records"), 3);
            assert_eq!(count("events"), 2);
            assert_eq!(count("consent"), 1);
            assert_eq!(count("consent_history"), 2);
            assert_eq!(count("lists"), 2);
            assert_eq!(count("list_history"), 2);
            assert_eq!(
                count("segments"),
                2,
                "the open row and the merged person's closed one"
            );
            assert_eq!(count("merges"), 1);
            assert_eq!(count("identity_conflicts"), 1);
            assert_eq!(count("enrollments"), 2);
            assert_eq!(count("step_executions"), 2);
            assert_eq!(count("enrollment_waits"), 2);
            assert_eq!(count("flow_entries"), 2);
            assert_eq!(
                count("messages"),
                2,
                "the survivor's and the merged person's"
            );
            assert_eq!(count("message_links"), 1);
            assert_eq!(
                count("forwarding_outbox"),
                3,
                "pending, dead and the merged one"
            );
            assert_eq!(count("erasure_handled_markers"), 1);
            assert_eq!(count("template_preview_people"), 2);
            for (section, _) in EXPORT_SECTIONS {
                if *section != "profile" {
                    assert!(person_json[*section].is_array(), "{section} is missing");
                }
            }
            assert!(person_json["profile"].is_object());
            let in_section =
                |section: &str, needle: &str| person_json[section].to_string().contains(needle);
            assert!(in_section("ingest_rejects", "survivor-reject"));
            assert!(in_section("decision_records", "survivor_decision"));
            assert!(in_section("decision_records", "merged_decision"));
            assert_eq!(person_json["messages"][0]["status"], "skipped");
            assert_eq!(
                person_json["message_links"][0]["url"],
                "https://example.com/seeded-link"
            );
            // The profile carries the traits, and events come oldest first.
            assert_eq!(person_json["profile"]["traits"]["plan"], "pro");
            let times: Vec<&str> = person_json["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|event| event["occurred_at"].as_str().unwrap())
                .collect();
            assert!(times[0] < times[1]);
            // Values are in the export, and the bystander's identity is not.
            let text = document.to_string();
            assert!(text.contains(EMAIL));
            assert!(!text.contains(BYSTANDER_EMAIL));
            // Nothing of the bystander's rows, and no recipient hash anywhere.
            for needle in [
                "bystander-reject",
                "bystander_decision",
                "bystander-node",
                "address_hash",
                "abababab",
            ] {
                assert!(!text.contains(needle), "{needle} is in the export");
            }
            let stored = find_privacy_export(&pool, ws, export.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stored.status, ExportStatus::Ready);
            let window = stored.expires_at.unwrap() - stored.ready_at.unwrap();
            assert_eq!(window, time::Duration::hours(24));
            assert_eq!(stored.size_bytes, Some(i64::try_from(bytes.len()).unwrap()));
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_table_with_a_person_column_is_exported_or_excluded_with_a_reason(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let checked = check_catalog(&pool, EXPORT_SECTIONS).await;

            assert_eq!(checked, Ok(()));
            let known: Vec<String> = sqlx::query_scalar(
                "SELECT table_name::text FROM information_schema.tables \
             WHERE table_schema = current_schema()",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            for (table, _) in EXPORT_EXCLUDED {
                assert!(
                    known.contains(&(*table).to_owned()),
                    "{table} is not a table"
                );
            }
            for (section, tables) in EXPORT_SECTIONS {
                for table in *tables {
                    assert!(
                        known.contains(&(*table).to_owned()),
                        "{section} reads {table}, which is not a table"
                    );
                }
            }
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_table_with_a_person_column_that_is_not_classified_fails_the_catalog(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            sqlx::query("CREATE TABLE unclassified_scratch (id uuid, person_id uuid)")
                .execute(&pool)
                .await
                .unwrap();

            let checked = check_catalog(&pool, EXPORT_SECTIONS).await;

            let error = checked.unwrap_err();
            assert!(error.contains("unclassified_scratch"), "{error}");
            assert!(!error.contains("identities"), "{error}");
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_person_over_the_cap_fails_with_export_too_large_and_leaves_no_content(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let Seeded { ws, person, .. } = seed_everything(&pool).await;
            let export = requested(&pool, ws, person).await;

            let outcome = built_with_cap(&pool, ws, export.id, 200).await;

            assert_eq!(outcome, BuildOutcome::TooLarge);
            let stored = find_privacy_export(&pool, ws, export.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stored.status, ExportStatus::Failed);
            assert_eq!(stored.failure_code.as_deref(), Some(EXPORT_TOO_LARGE));
            assert_eq!(stored.size_bytes, None);
            assert_eq!(stored.ready_at, None);
            let content: Option<Vec<u8>> =
                sqlx::query_scalar("SELECT content FROM privacy_exports WHERE id = $1")
                    .bind(export.id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(content, None);
            assert_eq!(
                download_export(&pool, ws, export.id).await.unwrap(),
                ExportDownload::NotReady
            );
            assert_eq!(built(&pool, ws, export.id).await, BuildOutcome::NotPending);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_catalog_reads_the_people_and_decision_tables_from_the_shared_discovery(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let discovered = crate::people::person_reference_places(&pool).await;
            let tables: Vec<&str> = discovered.iter().map(|(table, _)| table.as_str()).collect();
            assert!(tables.contains(&"people"));
            assert!(tables.contains(&"decision_records"));

            let without_decisions: Vec<(&str, &[&str])> = EXPORT_SECTIONS
                .iter()
                .filter(|(section, _)| *section != "decision_records")
                .copied()
                .collect();
            let error = check_catalog(&pool, &without_decisions).await.unwrap_err();

            assert!(error.contains("decision_records"), "{error}");
            assert!(!error.contains("people"), "{error}");
            let without_profile: Vec<(&str, &[&str])> = EXPORT_SECTIONS
                .iter()
                .filter(|(section, _)| *section != "profile")
                .copied()
                .collect();
            let error = check_catalog(&pool, &without_profile).await.unwrap_err();
            assert!(error.contains("people"), "{error}");
        })
        .await;
    }

    async fn seed_many_events(pool: &PgPool, ws: WorkspaceId, person: Uuid, count: i64) {
        sqlx::query(
            "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
                                 properties, source, trace_id, validation_state) \
             SELECT gen_random_uuid(), $1, $2, 'bulk', \
                    now() - interval '1 day' - n * interval '1 second', now(), \
                    jsonb_build_object('n', n), 'web', gen_random_uuid(), 'valid' \
             FROM generate_series(1, $3) n",
        )
        .bind(ws.as_uuid())
        .bind(person)
        .bind(count)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn events_past_the_cap_stop_polling_the_stream() {
        let polled = std::cell::Cell::new(0_u32);
        let at = OffsetDateTime::UNIX_EPOCH;
        let mut stream = futures_util::stream::iter(0..1000).map(|n| {
            polled.set(polled.get() + 1);
            Ok::<_, sqlx::Error>((at, Uuid::nil(), json!({"n": n})))
        });
        // Each row charges 8 to 9 bytes, so a cap of 100 is passed by the 13th.
        let mut budget = Budget::new(100);
        let mut events = Vec::new();
        let mut after = None;

        let result = consume_events(&mut stream, &mut budget, &mut events, &mut after).await;

        assert!(matches!(result, Err(Abort::TooLarge)));
        assert_eq!(events.len(), 12);
        assert_eq!(polled.get(), 13);
    }

    #[tokio::test]
    async fn events_within_the_cap_are_all_taken_and_the_last_is_remembered() {
        let at = OffsetDateTime::UNIX_EPOCH;
        let last = Uuid::from_u128(7);
        let mut stream = futures_util::stream::iter(vec![
            Ok::<_, sqlx::Error>((at, Uuid::nil(), json!({"n": 1}))),
            Ok((at, last, json!({"n": 2}))),
        ]);
        let mut budget = Budget::new(1000);
        let mut events = Vec::new();
        let mut after = None;

        let taken = consume_events(&mut stream, &mut budget, &mut events, &mut after)
            .await
            .unwrap();

        assert_eq!(taken, 2);
        assert_eq!(after, Some((at, last)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_document_over_the_cap_only_by_its_keys_fails_with_export_too_large(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let Seeded { ws, person, .. } = seed_everything(&pool).await;
            let mut connection = pool.acquire().await.unwrap();
            let document = person_document(&mut connection, ws, person, u64::MAX)
                .await
                .unwrap()
                .unwrap();
            let size = serde_json::to_vec(&document).unwrap().len() as u64;
            // The rows alone charge well under the document: the keys and delimiters make the rest.
            let under = person_document(&mut connection, ws, person, size - 1).await;

            assert!(matches!(under, Err(Abort::TooLarge)));
        })
        .await;
    }

    #[test]
    fn the_empty_document_names_every_section() {
        let document =
            serde_json::to_value(ExportDocument::empty("2026-01-01T00:00:00Z", Uuid::nil()))
                .unwrap();

        let person = document["person"].as_object().unwrap();
        for (name, _) in EXPORT_SECTIONS {
            assert!(person.contains_key(*name), "{name}");
        }
        assert!(person["profile"].is_null());
        assert_eq!(person["events"], json!([]));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_person_whose_events_pass_the_cap_fails_with_export_too_large(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let Seeded { ws, person, .. } = seed_everything(&pool).await;
            seed_many_events(&pool, ws, person, 2500).await;
            let export = requested(&pool, ws, person).await;

            let outcome = built_with_cap(&pool, ws, export.id, 100_000).await;

            assert_eq!(outcome, BuildOutcome::TooLarge);
            let stored = find_privacy_export(&pool, ws, export.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stored.failure_code.as_deref(), Some(EXPORT_TOO_LARGE));
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn merged_ids_alone_past_the_cap_fail_with_export_too_large(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = crate::privacy::testing::workspace(&pool, "merged-cap").await;
            let survivor = Uuid::now_v7();
            sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
                .bind(survivor)
                .bind(ws.as_uuid())
                .execute(&pool)
                .await
                .unwrap();
            let alone = requested(&pool, ws, survivor).await;
            let fits = built_with_cap(&pool, ws, alone.id, 1500).await;
            for _ in 0..30 {
                sqlx::query(
                    "INSERT INTO people (id, workspace_id, merged_into) VALUES ($1, $2, $3)",
                )
                .bind(Uuid::now_v7())
                .bind(ws.as_uuid())
                .bind(survivor)
                .execute(&pool)
                .await
                .unwrap();
            }
            let with_merged = requested(&pool, ws, survivor).await;

            let too_large = built_with_cap(&pool, ws, with_merged.id, 1500).await;

            assert!(matches!(fits, BuildOutcome::Built { .. }), "{fits:?}");
            assert_eq!(too_large, BuildOutcome::TooLarge);
        })
        .await;
    }

    #[test]
    fn the_budget_stops_at_the_row_that_passes_the_cap() {
        let mut budget = Budget::new(20);

        let first = budget.take(&json!({"a": 1}));
        let second = budget.take(&json!({"a": "a longer value than the cap allows"}));

        assert!(first.is_ok());
        assert!(matches!(second, Err(Abort::TooLarge)));
    }

    #[test]
    fn the_sections_and_the_exclusions_name_distinct_tables() {
        let mut names: Vec<&str> = EXPORT_SECTIONS.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        names.dedup();

        assert_eq!(names.len(), EXPORT_SECTIONS.len());
        for (table, reason) in EXPORT_EXCLUDED {
            assert!(!reason.is_empty(), "{table}");
            assert!(
                EXPORT_SECTIONS
                    .iter()
                    .all(|(_, read)| !read.contains(table)),
                "{table}"
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_pending_export_is_not_downloadable_and_a_second_build_does_nothing(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let Seeded { ws, person, .. } = seed_everything(&pool).await;
            let export = requested(&pool, ws, person).await;

            let before = download_export(&pool, ws, export.id).await.unwrap();
            built(&pool, ws, export.id).await;
            let again = built(&pool, ws, export.id).await;
            let missing = built(&pool, ws, Uuid::now_v7()).await;
            let unknown = download_export(&pool, ws, Uuid::now_v7()).await.unwrap();

            assert_eq!(before, ExportDownload::NotReady);
            assert_eq!(again, BuildOutcome::NotPending);
            assert_eq!(missing, BuildOutcome::NoSuchExport);
            assert_eq!(unknown, ExportDownload::NotFound);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_export_past_its_time_is_gone_even_before_the_upkeep_and_the_upkeep_drops_it(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let Seeded { ws, person, .. } = seed_everything(&pool).await;
            let export = requested(&pool, ws, person).await;
            built(&pool, ws, export.id).await;
            sqlx::query("UPDATE privacy_exports SET expires_at = now() - interval '1 minute'")
                .execute(&pool)
                .await
                .unwrap();

            let download = download_export(&pool, ws, export.id).await.unwrap();
            let listed = find_privacy_export(&pool, ws, export.id)
                .await
                .unwrap()
                .unwrap();
            let expired = expire_exports(&pool).await.unwrap();
            let after = download_export(&pool, ws, export.id).await.unwrap();

            assert_eq!(download, ExportDownload::Expired);
            assert_eq!(listed.status, ExportStatus::Expired);
            assert_eq!(expired, 1);
            assert_eq!(after, ExportDownload::Expired);
            let content: Option<Vec<u8>> =
                sqlx::query_scalar("SELECT content FROM privacy_exports")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert!(content.is_none());
            assert_eq!(expire_exports(&pool).await.unwrap(), 0);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn exports_page_newest_first_and_a_dead_job_reads_as_failed(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let Seeded { ws, person, .. } = seed_everything(&pool).await;
            let first = requested(&pool, ws, person).await;
            let second = requested(&pool, ws, person).await;
            sqlx::query("UPDATE jobs SET state = 'dead', dead_at = now() WHERE unique_key = $1")
                .bind(format!("privacy.export:{}", first.id))
                .execute(&pool)
                .await
                .unwrap();

            let (page, more) = list_privacy_exports(&pool, ws, 1, None).await.unwrap();
            let (rest, more_after) =
                list_privacy_exports(&pool, ws, 1, Some((page[0].requested_at, page[0].id)))
                    .await
                    .unwrap();

            assert_eq!(page[0].id, second.id);
            assert!(more);
            assert_eq!(rest[0].id, first.id);
            assert_eq!(rest[0].status, ExportStatus::Failed);
            // The export the seed made is the third.
            assert!(more_after);
            let none = {
                let mut tx = begin(&pool).await.unwrap();
                request_export(&mut tx, ws, Uuid::now_v7(), None, &trace())
                    .await
                    .unwrap()
            };
            assert!(none.is_none());
        })
        .await;
    }
}
