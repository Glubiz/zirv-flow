//! Erasure v1 (architecture section 16.3): the ledger, the transaction that carries a request
//! out, and the replay after a backup restore.
//!
//! The ledger is the one place that outlives an erased person. It keeps the person id and the
//! keyed hashes of their identifiers, never a value. Nothing here logs, labels or audits an
//! identifier, a trait or an event value.

use hashbrown::HashSet;

use serde_json::{Value, json};
use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::consent::SuppressionReason;
use zirv_flow_core::people::{Identifier, identifier_hash};
use zirv_flow_core::{HmacKey, TraceContext, WorkspaceId};

use super::delete::{DeleteReason, delete_person, purge_jobs};
use super::erasure_calls::{self, EraseTarget};
use crate::{
    Actor, NewAuditEntry, NewJob, NewSuppression, PgPool, StoreError, add_suppression,
    append_audit, begin, commit, enqueue,
};

/// The job kind that carries an erasure request out.
pub const ERASE_JOB_KIND: &str = "privacy.erase";

/// The `source` of the suppressions an erasure adds.
const SOURCE: &str = "erasure";

/// Where an erasure request stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErasureStatus {
    Pending,
    Completed,
    /// The job ran out of attempts.
    Failed,
}

impl ErasureStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    fn parse(text: &str) -> Result<Self, StoreError> {
        match text {
            "pending" => Ok(Self::Pending),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            _ => Err(StoreError::query(sqlx::Error::Decode(
                "unknown erasure status".into(),
            ))),
        }
    }
}

/// An erasure request as listed: never the identifier hashes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ErasureRequest {
    pub id: Uuid,
    pub person_id: Uuid,
    pub status: ErasureStatus,
    pub requested_by: Option<Uuid>,
    pub requested_at: OffsetDateTime,
    pub completed_at: Option<OffsetDateTime>,
    pub trace_id: Uuid,
}

#[derive(FromRow)]
struct RequestRow {
    id: Uuid,
    person_id: Uuid,
    status: String,
    requested_by: Option<Uuid>,
    requested_at: OffsetDateTime,
    completed_at: Option<OffsetDateTime>,
    trace_id: Uuid,
}

impl RequestRow {
    fn into_request(self) -> Result<ErasureRequest, StoreError> {
        Ok(ErasureRequest {
            id: self.id,
            person_id: self.person_id,
            status: ErasureStatus::parse(&self.status)?,
            requested_by: self.requested_by,
            requested_at: self.requested_at,
            completed_at: self.completed_at,
            trace_id: self.trace_id,
        })
    }
}

/// The columns of [`ErasureRequest`]. A pending request whose job went dead reads as failed.
const REQUEST_COLUMNS: &str = "r.id, r.person_id, \
    CASE WHEN r.status = 'pending' AND EXISTS ( \
            SELECT 1 FROM jobs j WHERE j.state = 'dead' \
              AND j.unique_key = 'privacy.erase:' || r.id::text) \
         THEN 'failed' ELSE r.status END AS status, \
    r.requested_by, r.requested_at, r.completed_at, r.trace_id";

/// Where the next page of requests starts: strictly before this `(requested_at, id)`.
pub type ErasureCursor = (OffsetDateTime, Uuid);

/// Records an erasure request for the person (or their survivor) and enqueues the job, in
/// the caller's transaction. The ledger keeps the keyed hash of every identity of the person
/// and of the people merged into them. `None` when the workspace has no such person.
pub async fn request_erasure(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    key: &HmacKey,
    requested_by: Option<Uuid>,
    trace: &TraceContext,
) -> Result<Option<ErasureRequest>, StoreError> {
    let Some(current) = crate::current_person(&mut *connection, workspace, person).await? else {
        return Ok(None);
    };
    let identities = group_identities(connection, workspace, current).await?;
    let mut hashes: Vec<Vec<u8>> = Vec::with_capacity(identities.len());
    let mut kinds: Vec<String> = Vec::with_capacity(identities.len());
    for (kind, value) in identities {
        // Stored identities are normalized already; one that no longer parses has no hash.
        if let Ok(identifier) = Identifier::parse(&kind, &value) {
            hashes.push(identifier_hash(key, &identifier).to_vec());
            kinds.push(kind);
        }
    }
    let id = Uuid::now_v7();
    let row: RequestRow = sqlx::query_as(
        "INSERT INTO erasure_requests \
            (id, workspace_id, person_id, identifier_hashes, identifier_kinds, requested_by, \
             trace_id, person_ids) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         RETURNING id, person_id, status, requested_by, requested_at, completed_at, trace_id",
    )
    .bind(id)
    .bind(workspace.as_uuid())
    .bind(current)
    .bind(&hashes)
    .bind(&kinds)
    .bind(requested_by)
    .bind(trace.trace_id.as_uuid())
    .bind(group_ids(connection, workspace, current).await?)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut job = NewJob::new(ERASE_JOB_KIND, trace.clone());
    job.workspace_id = Some(workspace);
    job.payload = json!({ "workspace_id": workspace.as_uuid(), "request_id": id });
    job.unique_key = Some(format!("{ERASE_JOB_KIND}:{id}"));
    enqueue(&mut *connection, &job).await?;
    row.into_request().map(Some)
}

/// One request of the workspace.
pub async fn find_erasure_request(
    pool: &PgPool,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<ErasureRequest>, StoreError> {
    let row: Option<RequestRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {REQUEST_COLUMNS} FROM erasure_requests r WHERE r.workspace_id = $1 AND r.id = $2"
    )))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(StoreError::query)?;
    row.map(RequestRow::into_request).transpose()
}

/// A page of the workspace's requests, newest first, at most `limit`, after the `before`
/// cursor. The flag is true when more exist.
pub async fn list_erasure_requests(
    pool: &PgPool,
    workspace: WorkspaceId,
    limit: u32,
    before: Option<ErasureCursor>,
) -> Result<(Vec<ErasureRequest>, bool), StoreError> {
    let mut rows: Vec<RequestRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {REQUEST_COLUMNS} FROM erasure_requests r \
         WHERE r.workspace_id = $1 \
           AND ($2::timestamptz IS NULL OR (r.requested_at, r.id) < ($2, $3)) \
         ORDER BY r.requested_at DESC, r.id DESC LIMIT $4"
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
    let requests = rows
        .into_iter()
        .map(RequestRow::into_request)
        .collect::<Result<_, _>>()?;
    Ok((requests, more))
}

/// A ledger row as the erasure reads it: the person, the identifier hashes and their kinds,
/// the ids of the group and the status.
type LedgerRow = (Uuid, Vec<Vec<u8>>, Vec<String>, Vec<Uuid>, String);

/// A completed ledger row as the replay reads it: the person, the hashes with their kinds and
/// the ids of the group.
type ReplayRow = (Uuid, Vec<Vec<u8>>, Vec<String>, Vec<Uuid>);

/// What [`erase_person`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EraseOutcome {
    /// The request is complete. `person_found` is false when the person was already gone.
    Completed { person_found: bool, inbox_rows: u64 },
    /// An earlier attempt completed it.
    AlreadyCompleted,
    /// The workspace has no such request.
    NoSuchRequest,
}

/// Carries an erasure request out, in the caller's transaction. It first locks the workspace's
/// pending inbox rows and then the group's people, the order a drain uses; an identity insert
/// takes a key-share lock on its person, so a concurrent attach waits for this transaction and
/// then fails on the foreign key. Then:
///
/// 1. collects the person's group (the survivor and every person merged into them) and its
///    identities as they are now, and adds their hashes, with their kinds, to the ledger row
///    and the group's ids to its `person_ids`, so identities gained since the request count;
/// 2. adds an `erased` suppression for every email and phone hash of the ledger row;
/// 3. deletes the workspace's pending inbox rows with an identifier that hashes to any hash
///    of the ledger row, locked `FOR UPDATE` so a concurrent drain has either applied them
///    already (and the deletion below covers what it wrote) or never sees them;
/// 4. deletes the group, and any other id the ledger names that still exists, and the
///    workspace's test-send log rows whose recipient hash is one of the ledger's;
/// 5. completes the ledger row and audits `privacy.erased` with the request id only.
///
/// Before the delete it plans the erase calls ([`erasure_calls`]): for each of `targets` that
/// handled the group (a message, a forwarding item or a sync outcome) it resolves the external
/// key from the group's rows, writes a pending `erasure_destination_calls` row and enqueues one
/// `destination.erase` job. The caller decides which destinations declare erase; the store reads
/// no manifest.
///
/// When the person is already gone, steps 2 and 3 still run from the ledger's hashes. Inbox
/// rows are matched by hash, so the identifiers are parsed and hashed exactly as the
/// suppressions are. A failure rolls everything back, so the request stays pending for the
/// job's retry.
pub async fn erase_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    request: Uuid,
    key: &HmacKey,
    targets: &[EraseTarget],
    trace: &TraceContext,
) -> Result<EraseOutcome, StoreError> {
    let row: Option<LedgerRow> = sqlx::query_as(
        "SELECT person_id, identifier_hashes, identifier_kinds, person_ids, status \
         FROM erasure_requests WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(request)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let Some((person, mut hashes, mut kinds, mut people, status)) = row else {
        return Ok(EraseOutcome::NoSuchRequest);
    };
    if status == ErasureStatus::Completed.as_str() {
        return Ok(EraseOutcome::AlreadyCompleted);
    }

    // Lock in the drain's order: the pending inbox rows, then the people. An identity insert takes
    // a key-share lock on its person, so once the group is locked a concurrent attach waits, and
    // after this commits the drain finds its locked person gone, which is a retryable race, and the
    // batch retries into a new person.
    let mut values: Vec<String> = Vec::new();
    // The call lock comes before every row lock: a sync that holds it shared across its call, and
    // then waits for the person row, must never meet an erasure that holds the row and waits for
    // the lock. Once the erasure has it, no sync is between its call and its outcome for these
    // people, so the evidence `erasure_calls::plan` reads is complete.
    let mut known = people.clone();
    known.push(person);
    if let Some(current) = crate::current_person(&mut *connection, workspace, person).await? {
        known.extend(group_ids(connection, workspace, current).await?);
    }
    erasure_calls::lock_person_calls_exclusive(connection, workspace, &known).await?;
    let inbox = lock_inbox(connection, workspace, key).await?;
    let mut survivor: Option<Uuid> = None;
    if let Some(group) = lock_group(connection, workspace, person).await? {
        survivor = Some(group.current);
        // A merge that committed meanwhile may have added members; a bucket for one is tried, never
        // waited for, now that row locks are held.
        erasure_calls::lock_person_calls_late(connection, workspace, &group.ids).await?;
        for &id in &group.ids {
            if !people.contains(&id) {
                people.push(id);
            }
        }
        for (kind, value) in group_identities(connection, workspace, group.current).await? {
            values.push(value.clone());
            // Stored identities are normalized already; one that no longer parses has no hash.
            if let Ok(identifier) = Identifier::parse(&kind, &value) {
                let hash = identifier_hash(key, &identifier).to_vec();
                if !hashes.contains(&hash) {
                    hashes.push(hash);
                    kinds.push(kind);
                }
            }
        }
        sqlx::query(
            "UPDATE erasure_requests SET identifier_hashes = $3, identifier_kinds = $4, \
                person_ids = $5 WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace.as_uuid())
        .bind(request)
        .bind(&hashes)
        .bind(&kinds)
        .bind(&people)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    }

    suppress(connection, workspace, &hashes, &kinds).await?;
    // The erase calls are planned while the person's rows are still there: which destinations
    // handled them, and the key each was given, are read from those rows.
    if let Some(current) = survivor {
        erasure_calls::plan(
            connection,
            workspace,
            request,
            (current, &people),
            targets,
            trace,
        )
        .await?;
    }
    // The test-send log names no person: it is found by the recipient hash, like a suppression.
    // So is an allowlist entry, which holds the address itself, sealed.
    crate::delete_test_sends_by_hash(connection, workspace, &hashes).await?;
    crate::delete_allowlist_by_hash(connection, workspace, &hashes).await?;
    let wanted: HashSet<Vec<u8>> = hashes.iter().cloned().collect();
    let inbox_rows =
        delete_inbox_rows(connection, workspace, &matching_rows(&inbox, &wanted)).await?;
    // Transcripts are free text, so they are found by the person's ids and identifier values.
    let needles: Vec<String> = std::iter::once(person)
        .chain(people.iter().copied())
        .map(|id| id.to_string())
        .chain(values)
        .collect();
    crate::ai::erase_ai_transcripts(connection, workspace, &needles).await?;
    let mut person_found = false;
    for id in std::iter::once(person).chain(people) {
        person_found |=
            delete_person(connection, workspace, id, DeleteReason::Erasure, trace).await?;
    }
    sqlx::query(
        "UPDATE erasure_requests SET status = 'completed', completed_at = now() \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(request)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    append_audit(
        &mut *connection,
        &NewAuditEntry {
            workspace_id: Some(workspace),
            actor: Actor::system(),
            action: "privacy.erased",
            object_kind: Some("erasure_request"),
            object_id: Some(request),
            request_id: None,
            summary: json!({}),
        },
    )
    .await?;
    Ok(EraseOutcome::Completed {
        person_found,
        inbox_rows,
    })
}

/// What [`replay_erasures`] did, as counts only.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReplayReport {
    /// Completed ledger entries read.
    pub requests: u64,
    /// Suppressions added or upgraded to `erased`.
    pub suppressions: u64,
    /// Restored people erased again.
    pub people: u64,
    /// Pending inbox rows removed; only when a key was given.
    pub inbox_rows: u64,
    /// Ledger entries merged in from files before the replay; set by the caller, not here.
    pub merged: u64,
    /// Ledger files skipped because their workspace does not exist here; set by the caller.
    pub skipped_workspaces: u64,
}

/// Re-applies the completed ledger entries of one workspace, or of all of them, after a
/// backup restore: re-adds the `erased` suppressions from the stored hashes, erases any
/// ledger person that exists again, and, when `key` is given, removes pending inbox rows
/// whose identifiers hash to a ledger hash. One transaction per workspace.
pub async fn replay_erasures(
    pool: &PgPool,
    workspace: Option<WorkspaceId>,
    key: Option<&HmacKey>,
    trace: &TraceContext,
) -> Result<ReplayReport, StoreError> {
    let workspaces: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT workspace_id FROM erasure_requests \
         WHERE status = 'completed' AND ($1::uuid IS NULL OR workspace_id = $1) \
         ORDER BY workspace_id",
    )
    .bind(workspace.map(|id| id.as_uuid()))
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    let mut report = ReplayReport::default();
    for id in workspaces {
        let ws = WorkspaceId::from_uuid(id);
        let mut tx = begin(pool).await?;
        let entries: Vec<ReplayRow> = sqlx::query_as(
            "SELECT person_id, identifier_hashes, identifier_kinds, person_ids FROM erasure_requests \
             WHERE workspace_id = $1 AND status = 'completed' ORDER BY requested_at, id",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::query)?;
        let mut all_hashes: HashSet<Vec<u8>> = HashSet::new();
        for (person, hashes, kinds, people) in entries {
            report.requests += 1;
            report.suppressions += suppress(&mut tx, ws, &hashes, &kinds).await?;
            // Every id the erased group had: a backup from before a merge restores them apart.
            let ids: Vec<Uuid> = std::iter::once(person).chain(people).collect();
            for &id in &ids {
                if delete_person(&mut tx, ws, id, DeleteReason::Erasure, trace).await? {
                    report.people += 1;
                }
            }
            // A job restored from a backup can name a ledger person whose row is not there, and
            // `delete_person` purges jobs only for a person it found.
            purge_jobs(&mut tx, ws, &ids, &[]).await?;
            crate::delete_test_sends_by_hash(&mut tx, ws, &hashes).await?;
            crate::delete_allowlist_by_hash(&mut tx, ws, &hashes).await?;
            all_hashes.extend(hashes);
        }
        if let Some(key) = key {
            report.inbox_rows += delete_inbox_by_hash(&mut tx, ws, key, &all_hashes).await?;
        }
        commit(tx).await?;
    }
    Ok(report)
}

/// Adds an `erased` suppression for each email and phone hash. Returns how many were added or
/// upgraded from another reason.
async fn suppress(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    hashes: &[Vec<u8>],
    kinds: &[String],
) -> Result<u64, StoreError> {
    let mut changed = 0;
    for (hash, kind) in hashes.iter().zip(kinds) {
        if !crate::SUPPRESSION_KINDS.contains(&kind.as_str()) {
            continue;
        }
        let identifier_hash: [u8; 32] = hash.as_slice().try_into().map_err(|_| {
            StoreError::query(sqlx::Error::Decode("a ledger hash is not 32 bytes".into()))
        })?;
        let outcome = add_suppression(
            &mut *connection,
            workspace,
            &NewSuppression {
                identifier_hash,
                kind,
                reason: SuppressionReason::Erased,
                source: SOURCE,
                note: None,
                created_by: None,
            },
        )
        .await?;
        if outcome != crate::AddOutcome::Existing {
            changed += 1;
        }
    }
    Ok(changed)
}

/// The ids of the person and of the people merged into them.
async fn group_ids(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Vec<Uuid>, StoreError> {
    sqlx::query_scalar(
        "SELECT id FROM people WHERE workspace_id = $1 AND (id = $2 OR merged_into = $2) \
         ORDER BY id",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)
}

/// The `(kind, value)` of every identity of the person and of the people merged into them.
async fn group_identities(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Vec<(String, String)>, StoreError> {
    sqlx::query_as(
        "SELECT kind, value FROM identities WHERE workspace_id = $1 AND person_id IN \
            (SELECT id FROM people WHERE workspace_id = $1 AND (id = $2 OR merged_into = $2)) \
         ORDER BY kind, value",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)
}

/// A locked pending inbox row and the hashes of the identifiers it names.
struct LockedRow {
    id: i64,
    hashes: Vec<Vec<u8>>,
}

/// Locks the workspace's pending inbox rows `FOR UPDATE`, in batches, and hashes the
/// identifiers of each with core's normaliser. A concurrent drain skips locked rows.
async fn lock_inbox(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &HmacKey,
) -> Result<Vec<LockedRow>, StoreError> {
    const BATCH: i64 = 1000;
    let mut after = 0_i64;
    let mut locked = Vec::new();
    loop {
        let rows: Vec<(i64, Value)> = sqlx::query_as(
            "SELECT id, item FROM ingest_inbox WHERE workspace_id = $1 AND id > $2 \
             ORDER BY id LIMIT $3 FOR UPDATE",
        )
        .bind(workspace.as_uuid())
        .bind(after)
        .bind(BATCH)
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        let Some((last, _)) = rows.last() else {
            return Ok(locked);
        };
        after = *last;
        locked.extend(rows.iter().map(|(id, item)| {
            LockedRow {
                id: *id,
                hashes: item_identifiers(item)
                    .into_iter()
                    .filter_map(|(kind, value)| Identifier::parse(&kind, &value).ok())
                    .map(|identifier| identifier_hash(key, &identifier).to_vec())
                    .collect(),
            }
        }));
    }
}

/// The ids of the locked rows that name any of `hashes`.
fn matching_rows(rows: &[LockedRow], hashes: &HashSet<Vec<u8>>) -> Vec<i64> {
    rows.iter()
        .filter(|row| row.hashes.iter().any(|hash| hashes.contains(hash)))
        .map(|row| row.id)
        .collect()
}

/// The person's current group, locked.
struct LockedGroup {
    /// The survivor.
    current: Uuid,
    /// The survivor and every person merged into them, in id order.
    ids: Vec<Uuid>,
}

/// Locks the group of `person` (the survivor and the people merged into them) `FOR UPDATE` in
/// id order, then looks again: a merge that committed meanwhile may have changed the survivor
/// or added members, which are locked in turn. Settles when two passes agree; a group that
/// keeps changing is a retryable race. `None` when the person is gone.
async fn lock_group(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Option<LockedGroup>, StoreError> {
    const PASSES: usize = 4;
    let mut previous: Option<Vec<Uuid>> = None;
    for _ in 0..PASSES {
        let Some(current) = crate::current_person(&mut *connection, workspace, person).await?
        else {
            return Ok(None);
        };
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM people WHERE workspace_id = $1 AND (id = $2 OR merged_into = $2) \
             ORDER BY id FOR UPDATE",
        )
        .bind(workspace.as_uuid())
        .bind(current)
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        if previous.as_ref() == Some(&ids) {
            return Ok(Some(LockedGroup { current, ids }));
        }
        previous = Some(ids);
    }
    Err(StoreError::retryable_race())
}

/// Locks the workspace's pending inbox and deletes the rows with an identifier whose hash is
/// one of `hashes`.
async fn delete_inbox_by_hash(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &HmacKey,
    hashes: &HashSet<Vec<u8>>,
) -> Result<u64, StoreError> {
    if hashes.is_empty() {
        return Ok(0);
    }
    let rows = lock_inbox(connection, workspace, key).await?;
    delete_inbox_rows(connection, workspace, &matching_rows(&rows, hashes)).await
}

async fn delete_inbox_rows(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    ids: &[i64],
) -> Result<u64, StoreError> {
    if ids.is_empty() {
        return Ok(0);
    }
    let done = sqlx::query("DELETE FROM ingest_inbox WHERE workspace_id = $1 AND id = ANY($2)")
        .bind(workspace.as_uuid())
        .bind(ids)
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(done.rows_affected())
}

/// The `(kind, value)` pairs of an inbox item, empty when it has none.
fn item_identifiers(item: &Value) -> Vec<(String, String)> {
    item.get("identifiers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|pair| {
            let pair = pair.as_array()?;
            Some((
                pair.first()?.as_str()?.to_owned(),
                pair.get(1)?.as_str()?.to_owned(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::people::erasure::rows_referencing_person;
    use crate::privacy::testing::{
        BYSTANDER_EMAIL, EMAIL, Seeded, identifier_key, person_of, profile, rows_naming_records,
        seed_everything, trace, workspace,
    };
    use crate::{is_suppressed, remove_suppression};

    fn hash(value: &str) -> [u8; 32] {
        identifier_hash(
            &identifier_key(),
            &Identifier::parse("email", value).unwrap(),
        )
    }

    async fn request(pool: &PgPool, ws: WorkspaceId, person: Uuid) -> ErasureRequest {
        let mut tx = begin(pool).await.unwrap();
        let request = request_erasure(&mut tx, ws, person, &identifier_key(), None, &trace())
            .await
            .unwrap()
            .unwrap();
        commit(tx).await.unwrap();
        request
    }

    async fn erase(pool: &PgPool, ws: WorkspaceId, request: Uuid) -> EraseOutcome {
        let mut tx = begin(pool).await.unwrap();
        let outcome = erase_person(&mut tx, ws, request, &identifier_key(), &[], &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();
        outcome
    }

    async fn inbox_messages(pool: &PgPool) -> Vec<String> {
        sqlx::query_scalar("SELECT message_id FROM ingest_inbox ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[test]
    fn a_status_round_trips_and_an_unknown_one_is_refused() {
        for status in [
            ErasureStatus::Pending,
            ErasureStatus::Completed,
            ErasureStatus::Failed,
        ] {
            assert_eq!(ErasureStatus::parse(status.as_str()).unwrap(), status);
        }
        assert!(ErasureStatus::parse("gone").is_err());
    }

    #[test]
    fn inbox_identifiers_are_read_from_the_stored_item() {
        let item = json!({"identifiers": [["email", "a@b.c"], ["bad"], [1, 2]], "traits": {}});

        assert_eq!(
            item_identifiers(&item),
            [("email".to_owned(), "a@b.c".to_owned())]
        );
        assert!(item_identifiers(&json!({"identifiers": 5})).is_empty());
        assert!(item_identifiers(&json!({})).is_empty());
    }

    /// Plants a test-send log row for the recipient hash.
    async fn test_send_to(pool: &PgPool, ws: WorkspaceId, recipient: &[u8; 32]) {
        sqlx::query(
            "INSERT INTO test_sends (id, workspace_id, template_id, destination_id, \
                sender_identity_id, recipient_hash, render_context) \
             VALUES ($1, $2, $1, $1, $1, $3, 'sample')",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(recipient.as_slice())
        .execute(pool)
        .await
        .unwrap();
    }

    /// Seals `address` into an allowlist entry for its hash, as the API does.
    async fn allowlist(
        pool: &PgPool,
        keys: &zirv_flow_core::Keyring,
        ws: WorkspaceId,
        address: &str,
    ) {
        let id = Uuid::now_v7();
        let sealed = zirv_flow_core::seal(
            keys,
            address.as_bytes(),
            &zirv_flow_core::secret_aad(
                ws,
                zirv_flow_core::SecretId::from_uuid(id),
                zirv_flow_core::SecretPurpose::TestSendAddress,
            ),
        );
        let mut tx = begin(pool).await.unwrap();
        crate::add_allowlist_entry(
            &mut tx,
            &keys.key_ids(),
            ws,
            id,
            &hash(address),
            "a***@x",
            &sealed,
        )
        .await
        .unwrap()
        .unwrap();
        commit(tx).await.unwrap();
    }

    /// A keyring and the HMAC root it wraps, so sealed secrets can be written.
    async fn keyring(pool: &PgPool) -> zirv_flow_core::Keyring {
        let keys =
            zirv_flow_core::Keyring::new(zirv_flow_core::MasterKey::generate(), None).unwrap();
        crate::secrets::insert_hmac_root_if_absent(
            pool,
            &zirv_flow_core::seal(&keys, b"root", b"aad"),
        )
        .await
        .unwrap();
        keys
    }

    /// The erasure rule of each test-send table, by what the table holds. `test_sends` holds a
    /// recipient hash; `test_send_allowlist` holds an address hash and, through its entry id, the
    /// address sealed; both are found by the hashes of the erased person's identifiers.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn erasing_a_person_deletes_the_test_sends_and_allowlist_entries_of_their_hashes(
        pool: PgPool,
    ) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let keys = keyring(&pool).await;
        test_send_to(&pool, ws, &hash(EMAIL)).await;
        test_send_to(&pool, ws, &hash(EMAIL)).await;
        test_send_to(&pool, ws, &hash(BYSTANDER_EMAIL)).await;
        allowlist(&pool, &keys, ws, EMAIL).await;
        allowlist(&pool, &keys, ws, BYSTANDER_EMAIL).await;

        let request = request(&pool, ws, person).await;
        erase(&pool, ws, request.id).await;

        let remaining: Vec<Vec<u8>> = sqlx::query_scalar("SELECT recipient_hash FROM test_sends")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, [hash(BYSTANDER_EMAIL).to_vec()]);
        let allowed: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT address_hash FROM test_send_allowlist")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(allowed, [hash(BYSTANDER_EMAIL).to_vec()]);
        let sealed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM encrypted_secrets WHERE purpose = 'test_send_address'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(sealed, 1, "the erased address is not kept sealed either");
        assert!(rows_referencing_person(&pool, person).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn erasing_a_person_purges_the_test_send_jobs_that_render_them(pool: PgPool) {
        let Seeded {
            ws,
            person,
            bystander,
            ..
        } = seed_everything(&pool).await;
        for (who, key) in [(person, "erased"), (bystander, "kept")] {
            sqlx::query(
                "INSERT INTO jobs (id, workspace_id, kind, payload, unique_key, cause_kind, \
                    cause_id, trace_id) \
                 VALUES (gen_random_uuid(), $1, 'template.test_send', \
                    jsonb_build_object('person_id', $2::text), $3, 'api', 't', gen_random_uuid())",
            )
            .bind(ws.as_uuid())
            .bind(who)
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        }

        let request = request(&pool, ws, person).await;
        erase(&pool, ws, request.id).await;

        let left: Vec<String> =
            sqlx::query_scalar("SELECT unique_key FROM jobs WHERE kind = 'template.test_send'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(left, ["kept"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn replay_deletes_test_sends_and_allowlist_entries_by_the_ledger_hashes(pool: PgPool) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let keys = keyring(&pool).await;
        let request = request(&pool, ws, person).await;
        erase(&pool, ws, request.id).await;
        // A backup from before the erasure brings the rows back.
        test_send_to(&pool, ws, &hash(EMAIL)).await;
        test_send_to(&pool, ws, &hash(BYSTANDER_EMAIL)).await;
        allowlist(&pool, &keys, ws, EMAIL).await;
        allowlist(&pool, &keys, ws, BYSTANDER_EMAIL).await;

        replay_erasures(&pool, Some(ws), None, &trace())
            .await
            .unwrap();

        let sends: Vec<Vec<u8>> = sqlx::query_scalar("SELECT recipient_hash FROM test_sends")
            .fetch_all(&pool)
            .await
            .unwrap();
        let allowed: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT address_hash FROM test_send_allowlist")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(sends, [hash(BYSTANDER_EMAIL).to_vec()]);
        assert_eq!(allowed, [hash(BYSTANDER_EMAIL).to_vec()]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn replay_purges_a_restored_test_send_job_whose_person_is_gone(pool: PgPool) {
        let Seeded {
            ws,
            person,
            bystander,
            ..
        } = seed_everything(&pool).await;
        let request = request(&pool, ws, person).await;
        erase(&pool, ws, request.id).await;
        // A backup brings the jobs back; the erased person's row is not among the restored rows.
        for (who, key) in [(person, "erased"), (bystander, "kept")] {
            sqlx::query(
                "INSERT INTO jobs (id, workspace_id, kind, payload, unique_key, cause_kind, \
                    cause_id, trace_id) \
                 VALUES (gen_random_uuid(), $1, 'template.test_send', \
                    jsonb_build_object('person_id', $2::text), $3, 'api', 't', gen_random_uuid())",
            )
            .bind(ws.as_uuid())
            .bind(who)
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        }
        let gone: i64 = sqlx::query_scalar("SELECT count(*) FROM people WHERE id = $1")
            .bind(person)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(gone, 0, "the person is not there to delete");

        replay_erasures(&pool, Some(ws), None, &trace())
            .await
            .unwrap();

        let left: Vec<String> =
            sqlx::query_scalar("SELECT unique_key FROM jobs WHERE kind = 'template.test_send'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(left, ["kept"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn erasing_a_person_deletes_the_ai_transcripts_that_name_them_and_no_others(
        pool: PgPool,
    ) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let other = crate::create_workspace(
            &pool,
            &zirv_flow_core::WorkspaceName::parse("elsewhere").unwrap(),
            &zirv_flow_core::Slug::parse("elsewhere").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let later = time::OffsetDateTime::now_utc() + time::Duration::days(7);
        let mut connection = pool.acquire().await.unwrap();
        for (workspace, content) in [
            (
                ws,
                json!({"prompt": format!("map {}", EMAIL.to_uppercase())}),
            ),
            (ws, json!({"prompt": format!("person {person}")})),
            (ws, json!({"prompt": format!("also {BYSTANDER_EMAIL}")})),
            (other, json!({"prompt": format!("map {EMAIL}")})),
        ] {
            let call = Uuid::now_v7();
            crate::record_ai_call(
                &mut connection,
                &crate::NewAiCall {
                    id: call,
                    workspace_id: workspace,
                    user_id: None,
                    feature: "mapping",
                    provider_id: "p",
                    model: "m",
                    input_tokens: 1,
                    output_tokens: 1,
                    estimated_cost_micros: 1,
                    latency_ms: 1,
                    outcome: crate::CallOutcome::Ok,
                    draft_id: None,
                },
            )
            .await
            .unwrap();
            crate::insert_ai_transcript(&mut connection, workspace, call, None, &content, later)
                .await
                .unwrap();
        }
        drop(connection);
        let request = request(&pool, ws, person).await;

        erase(&pool, ws, request.id).await;

        let left: Vec<(Uuid, Value)> =
            sqlx::query_as("SELECT workspace_id, content FROM ai_transcripts ORDER BY created_at")
                .fetch_all(&pool)
                .await
                .unwrap();
        let texts: Vec<String> = left
            .iter()
            .map(|(_, content)| content.to_string())
            .collect();
        assert_eq!(left.len(), 2, "{texts:?}");
        assert!(texts.iter().any(|text| text.contains(BYSTANDER_EMAIL)));
        assert!(
            left.iter()
                .any(|(workspace, _)| *workspace == other.as_uuid())
        );
        assert!(texts.iter().all(|text| !text.contains(&person.to_string())));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_request_records_the_hashes_of_every_identity_and_enqueues_the_job(pool: PgPool) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;

        let request = request(&pool, ws, person).await;

        assert_eq!(request.status, ErasureStatus::Pending);
        assert_eq!(request.person_id, person);
        let (hashes, kinds): (Vec<Vec<u8>>, Vec<String>) = sqlx::query_as(
            "SELECT identifier_hashes, identifier_kinds FROM erasure_requests WHERE id = $1",
        )
        .bind(request.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        // The survivor holds the email, the external id and the merged person's anonymous id.
        assert_eq!(hashes.len(), 3);
        assert!(hashes.contains(&hash(EMAIL).to_vec()));
        assert_eq!(kinds, ["anonymous_id", "email", "external:crm"]);
        let jobs: Vec<(String, Option<String>, Value)> =
            sqlx::query_as("SELECT kind, unique_key, payload FROM jobs WHERE kind = $1")
                .bind(ERASE_JOB_KIND)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].1.as_deref(),
            Some(format!("privacy.erase:{}", request.id).as_str())
        );
        assert_eq!(jobs[0].2["request_id"], json!(request.id));
        let unknown = {
            let mut tx = begin(&pool).await.unwrap();
            request_erasure(
                &mut tx,
                ws,
                Uuid::now_v7(),
                &identifier_key(),
                None,
                &trace(),
            )
            .await
            .unwrap()
        };
        assert!(unknown.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn erasing_a_person_with_every_data_class_leaves_only_the_ledger(pool: PgPool) {
        let Seeded {
            ws,
            person,
            merged,
            bystander,
            ..
        } = seed_everything(&pool).await;
        let request = request(&pool, ws, person).await;
        // The records of the two people, and the rows that name them before the erasure: one
        // event carries a record link.
        let records: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM source_records WHERE workspace_id = $1 AND person_id = ANY($2)",
        )
        .bind(ws.as_uuid())
        .bind(vec![person, merged])
        .fetch_all(&pool)
        .await
        .unwrap();
        let before = rows_naming_records(&pool, &records).await;
        let linked = |scan: &[(String, i64)], table: &str| {
            scan.iter()
                .find(|(name, _)| name == table)
                .map_or(0, |(_, count)| *count)
        };
        assert_eq!(linked(&before, "events"), 1, "{before:?}");

        let outcome = erase(&pool, ws, request.id).await;

        assert_eq!(
            outcome,
            EraseOutcome::Completed {
                person_found: true,
                inbox_rows: 1
            }
        );
        assert_eq!(rows_referencing_person(&pool, person).await, []);
        assert_eq!(rows_referencing_person(&pool, merged).await, []);
        // No table with a `source_record_id` column still names a record of the erased people.
        let after = rows_naming_records(&pool, &records).await;
        assert!(
            after.iter().all(|(_, count)| *count == 0) && after.len() == before.len(),
            "{after:?}"
        );
        // Only the ledger names the person.
        let ledger: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM erasure_requests WHERE person_id = $1 AND status = 'completed'",
        )
        .bind(person)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ledger, 1);
        // The email is suppressed as erased; the bystander is untouched.
        let suppression = crate::find_suppression(&pool, ws, &hash(EMAIL))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(suppression.reason, SuppressionReason::Erased);
        assert_eq!(suppression.source, "erasure");
        assert!(
            !is_suppressed(&pool, ws, &hash(BYSTANDER_EMAIL))
                .await
                .unwrap()
        );
        assert!(
            rows_referencing_person(&pool, bystander)
                .await
                .iter()
                .any(|(place, _)| place == "people.id")
        );
        // The pending row naming the person is gone; the bystander's is kept.
        assert_eq!(inbox_messages(&pool).await, ["i-2"]);
        let audit = crate::list_audit(&pool, 5).await.unwrap();
        let erased = audit
            .iter()
            .find(|row| row.action == "privacy.erased")
            .unwrap();
        assert_eq!(erased.object_id, Some(request.id));
        assert_eq!(erased.summary, json!({}));
        let stored = find_erasure_request(&pool, ws, request.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, ErasureStatus::Completed);
        assert!(stored.completed_at.is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn erasing_a_person_leaves_no_planted_value_in_a_slot_column(pool: PgPool) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        // `plan` is "pro" for the survivor; promote it to a slot and fill the column.
        let mut tx = crate::begin(&pool).await.unwrap();
        let assigned = crate::assign_trait_slot(&mut tx, ws, "plan").await.unwrap();
        crate::commit(tx).await.unwrap();
        let crate::Assigned::New(slot, generation) = assigned else {
            panic!("not assigned");
        };
        loop {
            let mut tx = crate::begin(&pool).await.unwrap();
            let step = crate::backfill_slot_batch(&mut tx, ws, "plan", slot, generation, None)
                .await
                .unwrap();
            crate::commit(tx).await.unwrap();
            if step == crate::SlotStep::Finish {
                break;
            }
        }
        let planted = |pool: &PgPool| {
            let sql = format!(
                "SELECT count(*) FROM people WHERE workspace_id = $1 AND {} = 'pro'",
                slot.as_str()
            );
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql))
                    .bind(ws.as_uuid())
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(planted(&pool).await, 1, "the survivor holds the marker");

        let request = request(&pool, ws, person).await;
        erase(&pool, ws, request.id).await;

        assert_eq!(planted(&pool).await, 0, "the marker left with the row");
    }
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_pending_row_cannot_recreate_the_person_and_the_new_person_stays_suppressed(
        pool: PgPool,
    ) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let request = request(&pool, ws, person).await;
        erase(&pool, ws, request.id).await;

        // The drain sees only the bystander's row.
        let key = zirv_flow_core::HmacRoot::bootstrap_from(
            &zirv_flow_core::MasterKey::parse("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=")
                .unwrap(),
        )
        .hmac_key(zirv_flow_core::HmacPurpose::Redaction);
        let drained = crate::drain_batch(&pool, ws, crate::Shard::ALL, &key, 100)
            .await
            .unwrap();
        assert_eq!(drained.rows, 1);
        let again = crate::people::find_person_by_identifier(
            &pool,
            ws,
            &Identifier::parse("email", EMAIL).unwrap(),
        )
        .await
        .unwrap();
        assert!(again.is_none());

        // A later ingest of the same email makes a new person, who is still suppressed.
        let outcomes = crate::apply_profile_batch(
            &pool,
            ws,
            &[profile("crm", "77", &[("email", EMAIL)], json!({}))],
        )
        .await
        .unwrap();
        let reborn = person_of(&outcomes[0]);
        assert_ne!(reborn, person);
        assert!(is_suppressed(&pool, ws, &hash(EMAIL)).await.unwrap());
        assert_eq!(
            remove_suppression(&pool, ws, &hash(EMAIL)).await.unwrap(),
            crate::RemoveOutcome::Erased
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_run_of_a_completed_request_does_nothing(pool: PgPool) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let request = request(&pool, ws, person).await;
        erase(&pool, ws, request.id).await;

        let again = erase(&pool, ws, request.id).await;
        let missing = erase(&pool, ws, Uuid::now_v7()).await;

        assert_eq!(again, EraseOutcome::AlreadyCompleted);
        assert_eq!(missing, EraseOutcome::NoSuchRequest);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failure_midway_rolls_everything_back_and_leaves_the_request_pending(pool: PgPool) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let request = request(&pool, ws, person).await;
        // A hash of the wrong length makes the suppression step fail after the inbox step.
        sqlx::query(
            "UPDATE erasure_requests SET identifier_hashes = ARRAY['\\x00'::bytea], \
                identifier_kinds = ARRAY['email'] WHERE id = $1",
        )
        .bind(request.id)
        .execute(&pool)
        .await
        .unwrap();

        let mut tx = begin(&pool).await.unwrap();
        let result = erase_person(&mut tx, ws, request.id, &identifier_key(), &[], &trace()).await;
        drop(tx);

        assert!(result.is_err());
        assert_eq!(inbox_messages(&pool).await, ["i-1", "i-2"]);
        assert!(!rows_referencing_person(&pool, person).await.is_empty());
        let stored = find_erasure_request(&pool, ws, request.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, ErasureStatus::Pending);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_request_whose_job_went_dead_reads_as_failed_and_pages_newest_first(pool: PgPool) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let first = request(&pool, ws, person).await;
        let second = request(&pool, ws, person).await;
        sqlx::query("UPDATE jobs SET state = 'dead', dead_at = now() WHERE unique_key = $1")
            .bind(format!("privacy.erase:{}", first.id))
            .execute(&pool)
            .await
            .unwrap();

        let (page, more) = list_erasure_requests(&pool, ws, 1, None).await.unwrap();
        let (rest, more_after) =
            list_erasure_requests(&pool, ws, 1, Some((page[0].requested_at, page[0].id)))
                .await
                .unwrap();

        assert_eq!(page[0].id, second.id);
        assert_eq!(page[0].status, ErasureStatus::Pending);
        assert!(more);
        assert_eq!(rest[0].id, first.id);
        assert_eq!(rest[0].status, ErasureStatus::Failed);
        assert!(!more_after);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn replay_erases_a_restored_person_again_and_restores_the_suppressions(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let outcomes = crate::apply_profile_batch(
            &pool,
            ws,
            &[profile(
                "crm",
                "1",
                &[("email", EMAIL), ("external:crm", "1")],
                json!({"plan": "pro"}),
            )],
        )
        .await
        .unwrap();
        let person = person_of(&outcomes[0]);
        let request = request(&pool, ws, person).await;
        erase(&pool, ws, request.id).await;
        // Simulate a restore from a backup older than the erasure: the person is back, and
        // the suppression is not there.
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO identities (id, workspace_id, person_id, kind, value) VALUES ($1, $2, $3, 'email', $4)")
            .bind(Uuid::now_v7())
            .bind(ws.as_uuid())
            .bind(person)
            .bind(EMAIL)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM suppressions")
            .execute(&pool)
            .await
            .unwrap();
        // The restored inbox still holds a row naming the person.
        crate::append_inbox(
            &pool,
            ws,
            Uuid::now_v7(),
            &trace(),
            &[crate::IngestItem {
                identifiers: vec![("email".into(), EMAIL.into())],
                timestamp: None,
                message_id: Some("i-9".into()),
                payload: crate::IngestPayload::Identify {
                    traits: serde_json::Map::new(),
                },
            }],
        )
        .await
        .unwrap();

        let report = replay_erasures(&pool, Some(ws), Some(&identifier_key()), &trace())
            .await
            .unwrap();
        let second = replay_erasures(&pool, None, None, &trace()).await.unwrap();

        assert_eq!(
            report,
            ReplayReport {
                requests: 1,
                suppressions: 1,
                people: 1,
                inbox_rows: 1,
                ..ReplayReport::default()
            }
        );
        assert_eq!(rows_referencing_person(&pool, person).await, []);
        assert!(is_suppressed(&pool, ws, &hash(EMAIL)).await.unwrap());
        assert!(inbox_messages(&pool).await.is_empty());
        // Replaying again finds nothing left to do but the ledger.
        assert_eq!(
            second,
            ReplayReport {
                requests: 1,
                ..ReplayReport::default()
            }
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn replay_leaves_pending_and_other_workspaces_requests_alone(pool: PgPool) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        request(&pool, ws, person).await;
        let other = workspace(&pool, "two").await;

        let report = replay_erasures(&pool, Some(other), None, &trace())
            .await
            .unwrap();
        let all = replay_erasures(&pool, None, None, &trace()).await.unwrap();

        assert_eq!(report, ReplayReport::default());
        assert_eq!(all, ReplayReport::default());
        assert!(!rows_referencing_person(&pool, person).await.is_empty());
    }

    async fn append(pool: &PgPool, ws: WorkspaceId, kind: &str, value: &str, message_id: &str) {
        crate::append_inbox(
            pool,
            ws,
            Uuid::now_v7(),
            &trace(),
            &[crate::IngestItem {
                identifiers: vec![(kind.into(), value.into())],
                timestamp: None,
                message_id: Some(message_id.into()),
                payload: crate::IngestPayload::Identify {
                    traits: serde_json::Map::new(),
                },
            }],
        )
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn pending_rows_are_matched_by_hash_so_padding_and_non_ascii_case_do_not_hide_them(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let outcomes = crate::apply_profile_batch(
            &pool,
            ws,
            &[
                profile("crm", "1", &[("email", EMAIL)], json!({})),
                profile("crm", "2", &[("email", "élodie@example.com")], json!({})),
            ],
        )
        .await
        .unwrap();
        let person = person_of(&outcomes[0]);
        let other = person_of(&outcomes[1]);
        for (email, id) in [
            ("\tAda@Example.com\n", "tab"),
            ("\u{a0}ada@example.com\u{a0}", "nbsp"),
            ("ÉLODIE@example.com", "case"),
            (BYSTANDER_EMAIL, "keep"),
        ] {
            append(&pool, ws, "email", email, id).await;
        }
        let first = request(&pool, ws, person).await;
        let second = request(&pool, ws, other).await;

        erase(&pool, ws, first.id).await;
        erase(&pool, ws, second.id).await;

        assert_eq!(inbox_messages(&pool).await, ["keep"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_identity_gained_after_the_request_is_suppressed_and_its_pending_rows_deleted(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let outcomes = crate::apply_profile_batch(
            &pool,
            ws,
            &[profile("crm", "1", &[("email", EMAIL)], json!({}))],
        )
        .await
        .unwrap();
        let person = person_of(&outcomes[0]);
        let request = request(&pool, ws, person).await;
        // After the request the person gains an address, and a row names it.
        crate::apply_profile_batch(
            &pool,
            ws,
            &[profile(
                "crm",
                "1",
                &[("email", EMAIL), ("phone", "+45 1234 5678")],
                json!({}),
            )],
        )
        .await
        .unwrap();
        append(&pool, ws, "phone", "0045 1234 5678", "gained").await;
        append(&pool, ws, "email", BYSTANDER_EMAIL, "keep").await;

        erase(&pool, ws, request.id).await;

        assert_eq!(inbox_messages(&pool).await, ["keep"]);
        let phone = identifier_hash(
            &identifier_key(),
            &Identifier::parse("phone", "+4512345678").unwrap(),
        );
        assert!(is_suppressed(&pool, ws, &phone).await.unwrap());
        assert!(is_suppressed(&pool, ws, &hash(EMAIL)).await.unwrap());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn erasing_a_person_deleted_before_the_job_still_clears_their_pending_rows(pool: PgPool) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let request = request(&pool, ws, person).await;
        let mut tx = begin(&pool).await.unwrap();
        delete_person(&mut tx, ws, person, DeleteReason::Requested, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();
        append(&pool, ws, "email", EMAIL, "late").await;

        let outcome = erase(&pool, ws, request.id).await;

        assert_eq!(
            outcome,
            EraseOutcome::Completed {
                person_found: false,
                inbox_rows: 2
            }
        );
        assert_eq!(inbox_messages(&pool).await, ["i-2"]);
        assert!(is_suppressed(&pool, ws, &hash(EMAIL)).await.unwrap());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn replay_erases_every_id_the_group_had_when_a_backup_predates_the_merge(pool: PgPool) {
        let Seeded {
            ws, person, merged, ..
        } = seed_everything(&pool).await;
        let request = request(&pool, ws, person).await;
        erase(&pool, ws, request.id).await;
        // The backup is from before the merge: both people are back, apart.
        for id in [person, merged] {
            sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
                .bind(id)
                .bind(ws.as_uuid())
                .execute(&pool)
                .await
                .unwrap();
        }

        let report = replay_erasures(&pool, Some(ws), None, &trace())
            .await
            .unwrap();

        assert_eq!(report.people, 2);
        assert_eq!(rows_referencing_person(&pool, person).await, []);
        assert_eq!(rows_referencing_person(&pool, merged).await, []);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_identity_attached_while_the_erasure_runs_is_ledgered_and_suppressed(pool: PgPool) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let request = request(&pool, ws, person).await;
        // A concurrent ingest has attached a new address and not committed yet: its insert
        // holds a key-share lock on the person.
        let mut ingest = begin(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO identities (id, workspace_id, person_id, kind, value) \
             VALUES ($1, $2, $3, 'email', 'b@example.com')",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(person)
        .execute(&mut *ingest)
        .await
        .unwrap();
        let erasure = tokio::spawn({
            let pool = pool.clone();
            async move {
                let mut tx = begin(&pool).await.unwrap();
                erase_person(&mut tx, ws, request.id, &identifier_key(), &[], &trace())
                    .await
                    .unwrap();
                commit(tx).await.unwrap();
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            !erasure.is_finished(),
            "the erasure did not wait for the person"
        );

        commit(ingest).await.unwrap();
        erasure.await.unwrap();

        assert!(
            is_suppressed(&pool, ws, &hash("b@example.com"))
                .await
                .unwrap()
        );
        let gone: i64 =
            sqlx::query_scalar("SELECT count(*) FROM identities WHERE value = 'b@example.com'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(gone, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_erasure_locks_the_inbox_before_the_people(pool: PgPool) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let request = request(&pool, ws, person).await;
        // A drain holds the person; the erasure must already hold the inbox rows by then.
        let mut drain = begin(&pool).await.unwrap();
        sqlx::query("SELECT id FROM people WHERE id = $1 FOR UPDATE")
            .bind(person)
            .execute(&mut *drain)
            .await
            .unwrap();
        let erasure = tokio::spawn({
            let pool = pool.clone();
            async move {
                let mut tx = begin(&pool).await.unwrap();
                erase_person(&mut tx, ws, request.id, &identifier_key(), &[], &trace())
                    .await
                    .unwrap();
                commit(tx).await.unwrap();
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let mut probe = begin(&pool).await.unwrap();
        let inbox = sqlx::query("SELECT id FROM ingest_inbox FOR UPDATE NOWAIT")
            .execute(&mut *probe)
            .await;
        drop(probe);
        commit(drain).await.unwrap();
        erasure.await.unwrap();

        assert!(
            inbox.is_err(),
            "the erasure waited on the people without holding the inbox"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_resolution_that_waited_on_an_erased_person_retries_instead_of_failing(pool: PgPool) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let request = request(&pool, ws, person).await;
        // The erasure has run but not committed; a drain has found the owner and waits for it.
        let mut eraser = begin(&pool).await.unwrap();
        erase_person(
            &mut eraser,
            ws,
            request.id,
            &identifier_key(),
            &[],
            &trace(),
        )
        .await
        .unwrap();
        let drain = tokio::spawn({
            let pool = pool.clone();
            async move {
                let mut tx = begin(&pool).await.unwrap();
                let identifiers = [Identifier::parse("email", EMAIL).unwrap()];
                crate::people::resolve_person(&mut tx, ws, &identifiers, None, &trace()).await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            !drain.is_finished(),
            "the drain did not wait for the person"
        );

        commit(eraser).await.unwrap();
        let outcome = drain.await.unwrap();

        assert!(
            matches!(&outcome, Err(error) if error.is_retryable()),
            "{outcome:?}"
        );
    }
}
