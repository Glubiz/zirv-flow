//! Export v1 (architecture section 16.3): one gzip-compressed JSON document of everything held
//! about a person, kept for 24 hours.
//!
//! An export is the one place personal values leave the system, so the API shows it to
//! workspace admins only. Nothing here logs, labels or audits a value.

use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;
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

/// Where an export stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportStatus {
    Pending,
    Ready,
    /// The job ran out of attempts.
    Failed,
    /// The 24 hours are over and the content is gone.
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
        })
    }
}

/// The columns of [`PrivacyExport`]. A pending export whose job went dead reads as failed,
/// and a ready one past its expiry as expired, even before the hourly upkeep has run.
const EXPORT_COLUMNS: &str = "x.id, x.person_id, \
    CASE WHEN x.status = 'pending' AND EXISTS ( \
            SELECT 1 FROM jobs j WHERE j.state = 'dead' \
              AND j.unique_key = 'privacy.export:' || x.id::text) THEN 'failed' \
         WHEN x.status = 'ready' AND x.expires_at <= now() THEN 'expired' \
         ELSE x.status END AS status, \
    x.requested_by, x.requested_at, x.ready_at, x.expires_at, x.size_bytes, x.trace_id";

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
                   size_bytes, trace_id",
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
    /// The document was built and stored; it expires 24 hours later.
    Built { size_bytes: i64 },
    /// An earlier attempt built it, or it expired since.
    NotPending,
    /// The workspace has no such export, or its person is gone.
    NoSuchExport,
}

/// Builds the export in the caller's transaction and marks it ready.
pub async fn build_export(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    export: Uuid,
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
    let Some(document) = person_document(connection, workspace, person).await? else {
        return Ok(BuildOutcome::NoSuchExport);
    };
    let compressed = gzip(&document)?;
    let size_bytes = i64::try_from(compressed.len()).unwrap_or(i64::MAX);
    sqlx::query(
        "UPDATE privacy_exports SET status = 'ready', content = $3, size_bytes = $4, \
            ready_at = now(), expires_at = now() + interval '24 hours' \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(export)
    .bind(compressed)
    .bind(size_bytes)
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
    /// The export is past its 24 hours, or already marked expired.
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

/// The export document of the person's group (the survivor and the people merged into them),
/// or none when the person is gone.
async fn person_document(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Option<Value>, StoreError> {
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

    let mut section = Section {
        connection,
        workspace,
        people: &people,
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
            "SELECT c.channel, t.key AS topic, c.status, c.explicit, c.changed_at \
             FROM consent c LEFT JOIN topics t \
                ON t.workspace_id = c.workspace_id AND t.id = c.topic_id \
             WHERE c.workspace_id = $1 AND c.person_id = ANY($2) \
             ORDER BY c.channel, t.key NULLS FIRST",
        )
        .await?;
    let consent_history = section
        .rows(
            "SELECT c.id, c.channel, t.key AS topic, c.from_status, c.to_status, c.source, \
                    c.evidence, c.occurred_at, c.actor_user_id, c.trace_id \
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

    let generated_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|error| StoreError::query(sqlx::Error::Encode(error.into())))?;
    Ok(Some(json!({
        "format": EXPORT_FORMAT,
        "generated_at": generated_at,
        "person": {
            "id": current,
            "merged_ids": merged,
            "profile": profile,
            "identities": identities,
            "source_records": source_records,
            "events": events,
            "consent": consent,
            "consent_history": consent_history,
            "lists": lists,
            "list_history": list_history,
            "merges": merges,
            "identity_conflicts": identity_conflicts,
        }
    })))
}

/// Reads sections of one person's group.
struct Section<'a> {
    connection: &'a mut PgConnection,
    workspace: WorkspaceId,
    people: &'a [Uuid],
}

impl Section<'_> {
    /// Each row of `select` (which binds `$1` the workspace and `$2` the people) as JSON.
    async fn rows(&mut self, select: &str) -> Result<Vec<Value>, StoreError> {
        // The statements are constants of this module, never built from input.
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT to_jsonb(t) FROM ({select}) t"
        )))
        .bind(self.workspace.as_uuid())
        .bind(self.people)
        .fetch_all(&mut *self.connection)
        .await
        .map_err(StoreError::query)
    }

    /// Every event of every partition, oldest first, read in batches.
    async fn events(&mut self) -> Result<Vec<Value>, StoreError> {
        let mut events = Vec::new();
        let mut after: Option<(OffsetDateTime, Uuid)> = None;
        loop {
            let batch: Vec<(OffsetDateTime, Uuid, Value)> = sqlx::query_as(
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
            .fetch_all(&mut *self.connection)
            .await
            .map_err(StoreError::query)?;
            let Some((at, id, _)) = batch.last() else {
                return Ok(events);
            };
            after = Some((*at, *id));
            let last_batch = batch.len() < EVENT_BATCH as usize;
            events.extend(batch.into_iter().map(|(_, _, event)| event));
            if last_batch {
                return Ok(events);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use flate2::read::GzDecoder;

    use super::*;
    use crate::privacy::testing::{BYSTANDER_EMAIL, EMAIL, Seeded, seed_everything, trace};
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
        let mut tx = begin(pool).await.unwrap();
        let outcome = build_export(&mut tx, ws, export).await.unwrap();
        commit(tx).await.unwrap();
        outcome
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
        let Seeded {
            ws, person, merged, ..
        } = seed_everything(&pool).await;
        let export = requested(&pool, ws, person).await;
        assert_eq!(export.status, ExportStatus::Pending);

        let outcome = built(&pool, ws, export.id).await;

        assert!(matches!(outcome, BuildOutcome::Built { .. }), "{outcome:?}");
        let ExportDownload::Content(bytes) = download_export(&pool, ws, export.id).await.unwrap()
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
        assert_eq!(count("merges"), 1);
        assert_eq!(count("identity_conflicts"), 1);
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
        let stored = find_privacy_export(&pool, ws, export.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, ExportStatus::Ready);
        let window = stored.expires_at.unwrap() - stored.ready_at.unwrap();
        assert_eq!(window, time::Duration::hours(24));
        assert_eq!(stored.size_bytes, Some(i64::try_from(bytes.len()).unwrap()));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_pending_export_is_not_downloadable_and_a_second_build_does_nothing(pool: PgPool) {
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
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_export_past_its_time_is_gone_even_before_the_upkeep_and_the_upkeep_drops_it(
        pool: PgPool,
    ) {
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
        let content: Option<Vec<u8>> = sqlx::query_scalar("SELECT content FROM privacy_exports")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(content.is_none());
        assert_eq!(expire_exports(&pool).await.unwrap(), 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn exports_page_newest_first_and_a_dead_job_reads_as_failed(pool: PgPool) {
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
    }
}
