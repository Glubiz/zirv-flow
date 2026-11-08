//! List memberships: adding and removing people, reading them, the history of every change,
//! and the move a merge's rewrite job makes. A change follows `merged_into`, is idempotent, keeps
//! `member_count` exact in its own transaction and leaves one history row and one
//! `list.membership` decision per real change. Decisions carry ids only.
//!
//! The list row and its locks (ruling bfa8cd75):
//! - An add locks the list `FOR KEY SHARE`, which conflicts only with a delete of the list. Two
//!   transactions that add to one list do not wait for each other, and a delete waits for every
//!   open add and then takes what they added with the list; an add that comes after the delete
//!   finds no list.
//! - The count an add owes moves at the end of its transaction ([`ListCounts::apply`]): one
//!   `UPDATE` per list, in list-id order, the last statement before the commit. Its
//!   `FOR NO KEY UPDATE` does not conflict with `FOR KEY SHARE`, so two transactions that both
//!   added to a list cannot wait for each other's add lock at the end, and the id order keeps
//!   two that hold several lists from crossing.
//! - A removal, a merge's rewrite ([`move_on_merge`]) and an erasure lock the list
//!   `FOR NO KEY UPDATE`: they wait for each other and for a count update, never for an add.

use std::collections::BTreeMap;
use std::fmt;

use hashbrown::HashMap;

use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::decision_actions;
use zirv_flow_core::people::IdentifierKind;
use zirv_flow_core::segment::ChangeClasses;
use zirv_flow_core::{TraceContext, UserId, WorkspaceId};

use crate::changes::{mark_people, request_reevaluation};
use crate::people::{PersonLock, lock_people_in_order};
use crate::{NewDecision, StoreError, append_decision, append_decisions};

/// The most people one add or remove takes.
pub const MAX_PEOPLE_PER_CALL: usize = 1000;

/// Tries to resolve and lock the people before the race with a merge is reported.
const RESOLVE_ATTEMPTS: usize = 3;

/// What caused a membership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListSource {
    Ui,
    Api,
    Import,
    Merge,
    Flow,
    /// A sync run added the person: a mapped list column was truthy.
    Sync,
}

impl ListSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ui => "ui",
            Self::Api => "api",
            Self::Import => "import",
            Self::Merge => "merge",
            Self::Flow => "flow",
            Self::Sync => "sync",
        }
    }
}

/// Why a membership change was refused.
#[derive(Debug)]
pub enum MembershipError {
    /// No list with that id in the workspace.
    NoSuchList,
    /// More than [`MAX_PEOPLE_PER_CALL`] people in one call.
    TooMany,
    Store(StoreError),
}

impl fmt::Display for MembershipError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchList => formatter.write_str("no such list"),
            Self::TooMany => write!(
                formatter,
                "a call takes at most {MAX_PEOPLE_PER_CALL} people"
            ),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for MembershipError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::NoSuchList | Self::TooMany => None,
        }
    }
}

impl From<StoreError> for MembershipError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// What an add or remove did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MembershipOutcome {
    /// People added or removed.
    pub changed: usize,
    /// People already members (on add) or not members (on remove), duplicates included.
    pub unchanged: usize,
    /// Positions in the input of people that do not exist in the workspace.
    pub missing: Vec<usize>,
}

/// One membership write: the list it changes, what caused it and who, and its trace.
#[derive(Clone, Copy, Debug)]
pub struct ListWrite<'a> {
    pub workspace: WorkspaceId,
    pub list: Uuid,
    pub source: ListSource,
    pub actor: Option<UserId>,
    pub trace: &'a TraceContext,
}

/// The `member_count` changes a transaction's membership writes owe, by list, for
/// [`ListCounts::apply`] to make right before the transaction commits.
#[derive(Debug, Default)]
pub struct ListCounts {
    pending: Vec<(WorkspaceId, Uuid, i64)>,
}

impl ListCounts {
    fn record(&mut self, workspace: WorkspaceId, list: Uuid, delta: i64) {
        if delta != 0 {
            self.pending.push((workspace, list, delta));
        }
    }

    /// The position to hand to [`ListCounts::discard_after`], taken before a savepoint.
    pub fn mark(&self) -> usize {
        self.pending.len()
    }

    /// Drops what was recorded after `mark`, for a savepoint that rolled back.
    pub fn discard_after(&mut self, mark: usize) {
        self.pending.truncate(mark);
    }

    /// Each list's net change, in list-id order, leaving out the lists that net to nothing.
    fn totals(&self) -> Vec<(WorkspaceId, Uuid, i64)> {
        let mut totals: BTreeMap<Uuid, (WorkspaceId, i64)> = BTreeMap::new();
        for (workspace, list, delta) in &self.pending {
            let total = totals.entry(*list).or_insert((*workspace, 0));
            total.1 = total.1.saturating_add(*delta);
        }
        totals
            .into_iter()
            .filter(|(_, (_, delta))| *delta != 0)
            .map(|(list, (workspace, delta))| (workspace, list, delta))
            .collect()
    }

    /// Moves each list's count by its net change, one statement per list in list-id order, and
    /// empties the buffer. Call it last, right before the commit: the row lock each statement
    /// takes is held until then. The writes that recorded a change hold their list
    /// `FOR KEY SHARE`, so it cannot be gone; if it is, the error is retryable and the retry
    /// finds no list.
    pub async fn apply(&mut self, connection: &mut PgConnection) -> Result<(), StoreError> {
        for (workspace, list, delta) in self.totals() {
            let updated = sqlx::query(
                "UPDATE lists SET member_count = member_count + $3 \
                 WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace.as_uuid())
            .bind(list)
            .bind(delta)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?
            .rows_affected();
            if updated != 1 {
                return Err(StoreError::retryable_race());
            }
        }
        self.pending.clear();
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Change {
    Added,
    Removed,
}

impl Change {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
        }
    }

    const fn spec(self) -> &'static decision_actions::DecisionSpec {
        match self {
            Self::Added => &decision_actions::LIST_MEMBERSHIP_ADDED,
            Self::Removed => &decision_actions::LIST_MEMBERSHIP_REMOVED,
        }
    }

    /// How the change locks its list; see the module doc.
    const fn list_lock(self) -> &'static str {
        match self {
            Self::Added => "SELECT id FROM lists WHERE workspace_id = $1 AND id = $2 FOR KEY SHARE",
            Self::Removed => {
                "SELECT id FROM lists WHERE workspace_id = $1 AND id = $2 FOR NO KEY UPDATE"
            }
        }
    }
}

/// Resolves each person to the one that stands for it now and locks those people, so a merge
/// of them cannot commit while this transaction writes them. Keyed by the id given; people
/// that do not exist are absent.
async fn lock_current_people(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    people: &[Uuid],
) -> Result<HashMap<Uuid, Uuid>, StoreError> {
    for _ in 0..RESOLVE_ATTEMPTS {
        let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT id, COALESCE(merged_into, id) FROM people \
             WHERE workspace_id = $1 AND id = ANY($2)",
        )
        .bind(workspace.as_uuid())
        .bind(people)
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        let mut targets: Vec<Uuid> = rows.iter().map(|(_, current)| *current).collect();
        targets.sort_unstable();
        targets.dedup();
        let locked =
            lock_people_in_order(&mut *connection, workspace, &targets, PersonLock::Share).await?;
        // A person merged away while the lock was awaited is resolved again.
        if locked.len() == targets.len() && locked.iter().all(|(_, merged)| merged.is_none()) {
            return Ok(rows.into_iter().collect());
        }
    }
    Err(StoreError::retryable_race())
}

/// The distinct current people to change, and an outcome that already counts every existing
/// person as unchanged and lists the missing ones. The list is locked for the rest of the
/// transaction, after the people, which is the order a merge's rewrite locks in, and in the
/// change's mode (module doc). A removal and the rewrite that moves a merged person's rows of
/// this list take the same mode, so one waits for the other; an add waits for neither.
async fn prepare(
    connection: &mut PgConnection,
    write: &ListWrite<'_>,
    change: Change,
    people: &[Uuid],
) -> Result<(Vec<Uuid>, MembershipOutcome), MembershipError> {
    if people.len() > MAX_PEOPLE_PER_CALL {
        return Err(MembershipError::TooMany);
    }
    let current = lock_current_people(connection, write.workspace, people).await?;
    let locked: Option<Uuid> = sqlx::query_scalar(change.list_lock())
        .bind(write.workspace.as_uuid())
        .bind(write.list)
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    if locked.is_none() {
        return Err(MembershipError::NoSuchList);
    }
    let mut outcome = MembershipOutcome::default();
    let mut targets = Vec::with_capacity(people.len());
    for (position, person) in people.iter().enumerate() {
        match current.get(person) {
            Some(target) => targets.push(*target),
            None => outcome.missing.push(position),
        }
    }
    outcome.unchanged = targets.len();
    targets.sort_unstable();
    targets.dedup();
    Ok((targets, outcome))
}

/// Writes the history rows and decisions of `people` and records in `counts` that the count
/// moves by `rows`, the membership rows written: a removal also takes the rows of people merged
/// into them.
async fn record_changes(
    connection: &mut PgConnection,
    write: &ListWrite<'_>,
    change: Change,
    people: &[Uuid],
    rows: u64,
    counts: &mut ListCounts,
) -> Result<(), StoreError> {
    if people.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = people.iter().map(|_| Uuid::now_v7()).collect();
    sqlx::query(
        "INSERT INTO list_member_changes \
            (id, workspace_id, list_id, person_id, action, source, actor_user_id, trace_id) \
         SELECT t.id, $1, $2, t.person_id, $3, $4, $5, $6 \
         FROM unnest($7::uuid[], $8::uuid[]) AS t(id, person_id)",
    )
    .bind(write.workspace.as_uuid())
    .bind(write.list)
    .bind(change.as_str())
    .bind(write.source.as_str())
    .bind(write.actor.map(|user| user.as_uuid()))
    .bind(write.trace.trace_id.as_uuid())
    .bind(&ids)
    .bind(people)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let count = i64::try_from(rows).unwrap_or(i64::MAX);
    let delta = match change {
        Change::Added => count,
        Change::Removed => -count,
    };
    counts.record(write.workspace, write.list, delta);
    let decisions: Vec<NewDecision> = people
        .iter()
        .map(|person| NewDecision {
            graph: None,
            workspace_id: write.workspace,
            trace: write.trace.clone(),
            spec: change.spec(),
            detail: Some(write.source.as_str()),
            subject_ids: vec![("list", write.list), ("person", *person)],
            job: None,
        })
        .collect();
    mark_people(
        connection,
        write.workspace,
        people,
        ChangeClasses::LISTS,
        write.trace,
        None,
    )
    .await?;
    append_decisions(connection, &decisions).await
}

/// Adds `people` to a list, following merges. Adding a member again writes nothing, and so does
/// adding a survivor whose merged-away person is still a member until the merge's rewrite moves
/// that row. At most [`MAX_PEOPLE_PER_CALL`] people; people that do not exist are reported,
/// never created. The count moves in the last statement, so a caller that commits right after
/// holds the list row for its commit alone; one that goes on writing uses [`add_members_into`].
pub async fn add_members(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    list: Uuid,
    people: &[Uuid],
    source: ListSource,
    actor: Option<UserId>,
    trace: &TraceContext,
) -> Result<MembershipOutcome, MembershipError> {
    let write = ListWrite {
        workspace,
        list,
        source,
        actor,
        trace,
    };
    let mut counts = ListCounts::default();
    let outcome = add_members_into(connection, &write, people, &mut counts).await?;
    counts.apply(connection).await?;
    Ok(outcome)
}

/// [`add_members`] for a transaction that goes on writing, a batch above all: the count change
/// goes to `counts`, which the transaction applies right before it commits. Until then the list
/// is held only `FOR KEY SHARE`, so batches that add to one list run side by side.
pub async fn add_members_into(
    connection: &mut PgConnection,
    write: &ListWrite<'_>,
    people: &[Uuid],
    counts: &mut ListCounts,
) -> Result<MembershipOutcome, MembershipError> {
    let (workspace, list) = (write.workspace, write.list);
    crate::changes::cache_change_readers(connection, workspace).await?;
    let (targets, mut outcome) = prepare(connection, write, Change::Added, people).await?;
    let added: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO list_members (workspace_id, list_id, person_id, source) \
         SELECT $1, $2, t.person_id, $4 FROM unnest($3::uuid[]) AS t(person_id) \
         WHERE NOT EXISTS (SELECT 1 FROM people p JOIN list_members m \
                             ON m.workspace_id = p.workspace_id AND m.person_id = p.id \
                           WHERE p.workspace_id = $1 AND p.merged_into = t.person_id \
                             AND m.list_id = $2) \
         ON CONFLICT DO NOTHING RETURNING person_id",
    )
    .bind(workspace.as_uuid())
    .bind(list)
    .bind(&targets)
    .bind(write.source.as_str())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let rows = u64::try_from(added.len()).unwrap_or(u64::MAX);
    record_changes(connection, write, Change::Added, &added, rows, counts).await?;
    if !added.is_empty() {
        request_reevaluation(connection, workspace).await?;
    }
    outcome.changed = added.len();
    outcome.unchanged -= added.len();
    Ok(outcome)
}

/// Removes `people` from a list, following merges. Removing a non-member writes nothing. The
/// rows of the people merged into them go too: until the merge's rewrite moves such a row it is
/// the person's membership, and moving it afterwards would put the person back. The list is held
/// `FOR NO KEY UPDATE` from the start, so the count moves at once, in the last statement.
pub async fn remove_members(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    list: Uuid,
    people: &[Uuid],
    source: ListSource,
    actor: Option<UserId>,
    trace: &TraceContext,
) -> Result<MembershipOutcome, MembershipError> {
    crate::changes::cache_change_readers(connection, workspace).await?;
    let write = ListWrite {
        workspace,
        list,
        source,
        actor,
        trace,
    };
    let (targets, mut outcome) = prepare(connection, &write, Change::Removed, people).await?;
    // One row per membership removed, named by the person it now stands for.
    let gone: Vec<Uuid> = sqlx::query_scalar(
        "WITH gone AS ( \
             DELETE FROM list_members m \
             WHERE m.workspace_id = $1 AND m.list_id = $2 \
               AND (m.person_id = ANY($3) OR m.person_id IN ( \
                    SELECT p.id FROM people p \
                    WHERE p.workspace_id = $1 AND p.merged_into = ANY($3))) \
             RETURNING m.person_id) \
         SELECT COALESCE(p.merged_into, g.person_id) FROM gone g \
         JOIN people p ON p.id = g.person_id",
    )
    .bind(workspace.as_uuid())
    .bind(list)
    .bind(&targets)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let rows = u64::try_from(gone.len()).unwrap_or(u64::MAX);
    let mut removed = gone;
    removed.sort_unstable();
    removed.dedup();
    let mut counts = ListCounts::default();
    record_changes(
        connection,
        &write,
        Change::Removed,
        &removed,
        rows,
        &mut counts,
    )
    .await?;
    if !removed.is_empty() {
        request_reevaluation(connection, workspace).await?;
    }
    counts.apply(connection).await?;
    outcome.changed = removed.len();
    outcome.unchanged -= removed.len();
    Ok(outcome)
}

/// The identifier that best identifies a member.
#[derive(Clone, PartialEq)]
pub struct MemberIdentifier {
    pub kind: String,
    pub value: String,
}

// Identifier values are personal data: Debug shows the kind only.
impl fmt::Debug for MemberIdentifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemberIdentifier")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// A member of a list.
#[derive(Clone, Debug, PartialEq)]
pub struct ListMember {
    pub person_id: Uuid,
    pub added_at: OffsetDateTime,
    pub source: String,
    /// The strongest identifier the person holds; `None` for a person without any.
    pub identifier: Option<MemberIdentifier>,
}

#[derive(FromRow)]
struct MemberRow {
    person_id: Uuid,
    added_at: OffsetDateTime,
    source: String,
    kinds: Option<Vec<String>>,
    vals: Option<Vec<String>>,
}

impl From<MemberRow> for ListMember {
    fn from(row: MemberRow) -> Self {
        let strength = |kind: &str| IdentifierKind::parse(kind).map_or(0, |kind| kind.strength());
        let kinds = row.kinds.unwrap_or_default();
        let values = row.vals.unwrap_or_default();
        let mut best: Option<(u8, MemberIdentifier)> = None;
        for (kind, value) in kinds.into_iter().zip(values) {
            let rank = strength(&kind);
            if best.as_ref().is_none_or(|(held, _)| rank > *held) {
                best = Some((rank, MemberIdentifier { kind, value }));
            }
        }
        Self {
            person_id: row.person_id,
            added_at: row.added_at,
            source: row.source,
            identifier: best.map(|(_, identifier)| identifier),
        }
    }
}

/// A page of a list's members, newest first. `before` is the `(added_at, person_id)` of the
/// last member of the previous page. The flag says whether more follow.
pub async fn members<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    list: Uuid,
    limit: u32,
    before: Option<(OffsetDateTime, Uuid)>,
) -> Result<(Vec<ListMember>, bool), StoreError> {
    let mut rows: Vec<MemberRow> = sqlx::query_as(
        "SELECT m.person_id, m.added_at, m.source, \
            (SELECT array_agg(i.kind ORDER BY i.kind, i.value) FROM identities i \
              WHERE i.workspace_id = m.workspace_id AND i.person_id = m.person_id) AS kinds, \
            (SELECT array_agg(i.value ORDER BY i.kind, i.value) FROM identities i \
              WHERE i.workspace_id = m.workspace_id AND i.person_id = m.person_id) AS vals \
         FROM list_members m \
         WHERE m.workspace_id = $1 AND m.list_id = $2 \
           AND ($3::timestamptz IS NULL OR (m.added_at, m.person_id) < ($3, $4)) \
         ORDER BY m.added_at DESC, m.person_id DESC LIMIT $5",
    )
    .bind(workspace.as_uuid())
    .bind(list)
    .bind(before.map(|(added_at, _)| added_at))
    .bind(before.map(|(_, person)| person))
    .bind(i64::from(limit) + 1)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok((rows.into_iter().map(ListMember::from).collect(), more))
}

/// A list a person is in.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct PersonList {
    pub list_id: Uuid,
    pub name: String,
    pub added_at: OffsetDateTime,
    pub source: String,
}

/// The lists a person (or its survivor, when it was merged) is in, ordered by name.
pub async fn lists_for<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Vec<PersonList>, StoreError> {
    sqlx::query_as(
        "SELECT l.id AS list_id, l.name, m.added_at, m.source \
         FROM list_members m JOIN lists l ON l.id = m.list_id \
         WHERE m.workspace_id = $1 \
           AND m.person_id = COALESCE((SELECT merged_into FROM people \
                                       WHERE workspace_id = $1 AND id = $2), $2) \
         ORDER BY lower(l.name), l.id",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// One change to a person's memberships.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct MembershipChange {
    pub id: Uuid,
    pub list_id: Uuid,
    pub list_name: String,
    pub person_id: Uuid,
    /// `added` or `removed`.
    pub action: String,
    pub source: String,
    pub occurred_at: OffsetDateTime,
    pub actor_user_id: Option<Uuid>,
    pub trace_id: Uuid,
}

/// A page of a person's membership changes (or those of its survivor), newest first.
/// `before` is the `(occurred_at, id)` of the last change of the previous page. The flag says
/// whether more follow.
pub async fn membership_history<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    person: Uuid,
    limit: u32,
    before: Option<(OffsetDateTime, Uuid)>,
) -> Result<(Vec<MembershipChange>, bool), StoreError> {
    let mut rows: Vec<MembershipChange> = sqlx::query_as(
        "SELECT c.id, c.list_id, l.name AS list_name, c.person_id, c.action, c.source, \
                c.occurred_at, c.actor_user_id, c.trace_id \
         FROM list_member_changes c JOIN lists l ON l.id = c.list_id \
         WHERE c.workspace_id = $1 \
           AND c.person_id = COALESCE((SELECT merged_into FROM people \
                                       WHERE workspace_id = $1 AND id = $2), $2) \
           AND ($3::timestamptz IS NULL OR (c.occurred_at, c.id) < ($3, $4)) \
         ORDER BY c.occurred_at DESC, c.id DESC LIMIT $5",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .bind(before.map(|(occurred_at, _)| occurred_at))
    .bind(before.map(|(_, id)| id))
    .bind(i64::from(limit) + 1)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok((rows, more))
}

/// Moves the memberships and history of `loser` to `survivor`, in the transaction of the merge's
/// rewrite job, which holds both people `FOR KEY SHARE` and the person-change guard. The loser's
/// lists are locked `FOR NO KEY UPDATE` first, in id order and in one statement, after the
/// people as every list writer locks. A removal takes the same lock, so a removal of the
/// survivor waits for the move or the move for it. An add holds a list only `FOR KEY SHARE` and
/// neither waits: it writes nothing for the survivor while the loser holds the list, and a
/// merged-away person gains no membership, so the set cannot grow before the move. A
/// `FOR UPDATE` here would wait for a moment with no add open on the list, which a busy list
/// may never have. The survivor ends up in the union of both people's lists and keeps the
/// earlier `added_at` of the two; counts follow. A moved membership marks the survivor in the
/// lists class, so the segments that read lists evaluate it on its new memberships. Returns the
/// rows moved (memberships and history); a second run moves none.
pub(crate) async fn move_on_merge(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    loser: Uuid,
    survivor: Uuid,
    trace: &TraceContext,
) -> Result<u64, StoreError> {
    sqlx::query(
        "SELECT id FROM lists WHERE workspace_id = $1 AND id IN \
            (SELECT list_id FROM list_members WHERE workspace_id = $1 AND person_id = $2) \
         ORDER BY id FOR NO KEY UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(loser)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let held: Vec<Uuid> = sqlx::query_scalar(
        "SELECT list_id FROM list_members \
         WHERE workspace_id = $1 AND person_id = $2 ORDER BY list_id",
    )
    .bind(workspace.as_uuid())
    .bind(loser)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut moved = 0;
    if !held.is_empty() {
        // Lists both are in: the survivor keeps the earlier time, and one member is gone.
        let shared: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE list_members s SET added_at = LEAST(s.added_at, l.added_at) \
             FROM list_members l \
             WHERE s.workspace_id = $1 AND s.person_id = $3 \
               AND l.workspace_id = $1 AND l.person_id = $2 AND l.list_id = s.list_id \
             RETURNING s.list_id",
        )
        .bind(workspace.as_uuid())
        .bind(loser)
        .bind(survivor)
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        sqlx::query(
            "INSERT INTO list_members (workspace_id, list_id, person_id, added_at, source) \
             SELECT workspace_id, list_id, $3, added_at, 'merge' FROM list_members \
             WHERE workspace_id = $1 AND person_id = $2 AND NOT (list_id = ANY($4))",
        )
        .bind(workspace.as_uuid())
        .bind(loser)
        .bind(survivor)
        .bind(&shared)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        moved += sqlx::query("DELETE FROM list_members WHERE workspace_id = $1 AND person_id = $2")
            .bind(workspace.as_uuid())
            .bind(loser)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?
            .rows_affected();
        sqlx::query(
            "UPDATE lists SET member_count = member_count - 1 \
             WHERE workspace_id = $1 AND id = ANY($2)",
        )
        .bind(workspace.as_uuid())
        .bind(&shared)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        mark_people(
            connection,
            workspace,
            &[survivor],
            ChangeClasses::LISTS,
            trace,
            None,
        )
        .await?;
        request_reevaluation(connection, workspace).await?;
    }
    let history = sqlx::query(
        "UPDATE list_member_changes SET person_id = $3 \
         WHERE workspace_id = $1 AND person_id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(loser)
    .bind(survivor)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?
    .rows_affected();
    moved += history;
    if !held.is_empty() || history > 0 {
        append_decision(
            connection,
            &NewDecision {
                graph: None,
                workspace_id: workspace,
                trace: trace.clone(),
                spec: &decision_actions::LIST_MEMBERSHIP_MOVED,
                detail: None,
                subject_ids: vec![
                    ("person", survivor),
                    ("survivor", survivor),
                    ("merged", loser),
                ],
                job: None,
            },
        )
        .await?;
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_batch};
    use crate::{begin, commit, create_list, create_workspace, find_list, list_trace};

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn trace() -> TraceContext {
        TraceContext::root(Cause::api("test"))
    }

    fn record(key: &str, identifiers: &[(&str, &str)]) -> ProfileRecord {
        ProfileRecord {
            source: "crm".into(),
            stream: "s".into(),
            record_key: key.into(),
            identifiers: identifiers
                .iter()
                .map(|(kind, value)| ((*kind).into(), (*value).into()))
                .collect(),
            traits: json!({}).as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: trace(),
        }
    }

    async fn person(
        pool: &PgPool,
        ws: WorkspaceId,
        key: &str,
        identifiers: &[(&str, &str)],
    ) -> Uuid {
        let outcomes = apply_profile_batch(pool, ws, &[record(key, identifiers)])
            .await
            .unwrap();
        match &outcomes[0] {
            ProfileOutcome::Applied { person_id, .. }
            | ProfileOutcome::Unchanged { person_id, .. } => *person_id,
            ProfileOutcome::Rejected { reason } => panic!("rejected: {reason}"),
        }
    }

    async fn add(
        pool: &PgPool,
        ws: WorkspaceId,
        list: Uuid,
        people: &[Uuid],
    ) -> Result<MembershipOutcome, MembershipError> {
        let mut tx = begin(pool).await.unwrap();
        let outcome =
            add_members(&mut tx, ws, list, people, ListSource::Api, None, &trace()).await?;
        commit(tx).await.unwrap();
        Ok(outcome)
    }

    async fn remove(
        pool: &PgPool,
        ws: WorkspaceId,
        list: Uuid,
        people: &[Uuid],
    ) -> Result<MembershipOutcome, MembershipError> {
        let mut tx = begin(pool).await.unwrap();
        let outcome =
            remove_members(&mut tx, ws, list, people, ListSource::Api, None, &trace()).await?;
        commit(tx).await.unwrap();
        Ok(outcome)
    }

    async fn count(pool: &PgPool, ws: WorkspaceId, list: Uuid) -> i64 {
        find_list(pool, ws, list)
            .await
            .unwrap()
            .unwrap()
            .member_count
    }

    async fn rows(pool: &PgPool, table: &str, list: Uuid) -> i64 {
        // The table name is a literal of this module.
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE list_id = $1"
        )))
        .bind(list)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn adding_and_removing_are_idempotent_and_keep_the_count_exact(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let list = create_list(&pool, ws, "VIPs", "", None).await.unwrap().id;
        let ada = person(&pool, ws, "1", &[("email", "ada@example.com")]).await;
        let bob = person(&pool, ws, "2", &[("email", "bob@example.com")]).await;
        let ghost = Uuid::now_v7();

        let first = add(&pool, ws, list, &[ada, bob, ada, ghost]).await.unwrap();
        let again = add(&pool, ws, list, &[ada, bob]).await.unwrap();

        assert_eq!(
            first,
            MembershipOutcome {
                changed: 2,
                unchanged: 1,
                missing: vec![3]
            }
        );
        assert_eq!(
            again,
            MembershipOutcome {
                changed: 0,
                unchanged: 2,
                missing: vec![]
            }
        );
        assert_eq!(count(&pool, ws, list).await, 2);
        assert_eq!(rows(&pool, "list_member_changes", list).await, 2);

        let removed = remove(&pool, ws, list, &[ada, ada, ghost]).await.unwrap();
        let removed_again = remove(&pool, ws, list, &[ada]).await.unwrap();

        assert_eq!((removed.changed, removed.unchanged), (1, 1));
        assert_eq!(removed.missing, [2]);
        assert_eq!((removed_again.changed, removed_again.unchanged), (0, 1));
        assert_eq!(count(&pool, ws, list).await, 1);
        assert_eq!(rows(&pool, "list_members", list).await, 1);
        assert_eq!(rows(&pool, "list_member_changes", list).await, 3);
        let actions: Vec<String> = sqlx::query_scalar(
            "SELECT action FROM list_member_changes WHERE person_id = $1 \
             ORDER BY occurred_at, id",
        )
        .bind(ada)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(actions, ["added", "removed"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn decisions_are_written_for_real_changes_only_and_hold_ids(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let list = create_list(&pool, ws, "VIPs", "", None).await.unwrap().id;
        let ada = person(&pool, ws, "1", &[("email", "ada@example.com")]).await;
        let trace = trace();
        let mut tx = begin(&pool).await.unwrap();
        add_members(&mut tx, ws, list, &[ada], ListSource::Ui, None, &trace)
            .await
            .unwrap();
        add_members(&mut tx, ws, list, &[ada], ListSource::Ui, None, &trace)
            .await
            .unwrap();
        remove_members(&mut tx, ws, list, &[ada], ListSource::Ui, None, &trace)
            .await
            .unwrap();
        remove_members(&mut tx, ws, list, &[ada], ListSource::Ui, None, &trace)
            .await
            .unwrap();
        commit(tx).await.unwrap();

        let (decisions, _) = list_trace(&pool, ws, trace.trace_id, 10).await.unwrap();

        // One transaction: the records share a timestamp, so their order is not defined.
        let mut outcomes: Vec<(&str, &str)> = decisions
            .iter()
            .map(|d| (d.action.as_str(), d.outcome.as_str()))
            .collect();
        outcomes.sort_unstable();
        assert_eq!(
            outcomes,
            [("list.membership", "added"), ("list.membership", "removed")]
        );
        for decision in &decisions {
            assert_eq!(
                decision.subject_ids,
                json!({"list": list.to_string(), "person": ada.to_string()})
            );
            assert_eq!(decision.reason.as_deref(), Some("ui"));
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_missing_list_a_foreign_list_and_too_many_people_are_refused(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let list = create_list(&pool, one, "VIPs", "", None).await.unwrap().id;
        let ada = person(&pool, one, "1", &[("email", "ada@example.com")]).await;

        let missing = add(&pool, one, Uuid::now_v7(), &[ada]).await.unwrap_err();
        let foreign = add(&pool, two, list, &[ada]).await.unwrap_err();
        let many: Vec<Uuid> = (0..=MAX_PEOPLE_PER_CALL).map(|_| Uuid::now_v7()).collect();
        let too_many = add(&pool, one, list, &many).await.unwrap_err();
        let exact: Vec<Uuid> = many[..MAX_PEOPLE_PER_CALL].to_vec();
        let at_the_limit = add(&pool, one, list, &exact).await.unwrap();

        assert!(matches!(missing, MembershipError::NoSuchList));
        assert!(matches!(foreign, MembershipError::NoSuchList));
        assert!(matches!(too_many, MembershipError::TooMany));
        assert_eq!(at_the_limit.missing.len(), MAX_PEOPLE_PER_CALL);
        assert_eq!(count(&pool, one, list).await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merged_person_is_added_and_removed_as_its_survivor(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let list = create_list(&pool, ws, "VIPs", "", None).await.unwrap().id;
        let survivor = person(&pool, ws, "1", &[("external:crm", "1")]).await;
        let loser = person(&pool, ws, "2", &[("anonymous_id", "a")]).await;
        let joined = person(
            &pool,
            ws,
            "3",
            &[("external:crm", "1"), ("anonymous_id", "a")],
        )
        .await;
        assert_eq!(joined, survivor);

        let added = add(&pool, ws, list, &[loser]).await.unwrap();
        let again = add(&pool, ws, list, &[survivor]).await.unwrap();

        assert_eq!(added.changed, 1);
        assert_eq!(again.changed, 0);
        let member: Uuid =
            sqlx::query_scalar("SELECT person_id FROM list_members WHERE list_id = $1")
                .bind(list)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(member, survivor);
        let removed = remove(&pool, ws, list, &[loser]).await.unwrap();
        assert_eq!(removed.changed, 1);
        assert_eq!(count(&pool, ws, list).await, 0);
    }

    /// Runs the merge's rewrite job for `merged` on `trace`, as the job does, and returns the
    /// rows it moved.
    async fn rewrite(pool: &PgPool, ws: WorkspaceId, merged: Uuid, trace: &TraceContext) -> u64 {
        let mut tx = begin(pool).await.unwrap();
        let moved = crate::rewrite_merged_person(&mut tx, ws, merged, trace, None)
            .await
            .unwrap();
        commit(tx).await.unwrap();
        moved
    }

    /// The `(person, list)` membership rows of `people`, sorted.
    async fn memberships(pool: &PgPool, people: &[Uuid]) -> Vec<(Uuid, Uuid)> {
        sqlx::query_as(
            "SELECT person_id, list_id FROM list_members WHERE person_id = ANY($1) ORDER BY 1, 2",
        )
        .bind(people)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merge_leaves_the_survivor_in_the_union_of_both_peoples_lists(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        crate::segments::tests_support::segment_reading(&pool, ws, ChangeClasses::LISTS).await;
        let a = create_list(&pool, ws, "A", "", None).await.unwrap().id;
        let b = create_list(&pool, ws, "B", "", None).await.unwrap().id;
        let c = create_list(&pool, ws, "C", "", None).await.unwrap().id;
        let p1 = person(&pool, ws, "1", &[("external:crm", "1")]).await;
        let p2 = person(&pool, ws, "2", &[("anonymous_id", "a")]).await;
        add(&pool, ws, a, &[p1]).await.unwrap();
        add(&pool, ws, b, &[p2]).await.unwrap();
        add(&pool, ws, c, &[p2]).await.unwrap();
        // P1 joins B after P2 did, so the merge must keep P2's earlier time.
        add(&pool, ws, b, &[p1]).await.unwrap();
        let p2_added_at: OffsetDateTime = sqlx::query_scalar(
            "SELECT added_at FROM list_members WHERE list_id = $1 AND person_id = $2",
        )
        .bind(b)
        .bind(p2)
        .fetch_one(&pool)
        .await
        .unwrap();
        let p1_added_at: OffsetDateTime = sqlx::query_scalar(
            "SELECT added_at FROM list_members WHERE list_id = $1 AND person_id = $2",
        )
        .bind(b)
        .bind(p1)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(p2_added_at < p1_added_at);
        let merge_trace = trace();
        let mut joined = record("3", &[("external:crm", "1"), ("anonymous_id", "a")]);
        joined.trace = merge_trace.clone();

        let outcomes = apply_profile_batch(&pool, ws, &[joined]).await.unwrap();

        assert!(
            matches!(&outcomes[0], ProfileOutcome::Applied { person_id, .. } if *person_id == p1)
        );
        // The merge leaves the memberships to its rewrite job: until it runs, the loser's rows
        // stay and B counts both.
        let mut before = vec![(p1, a), (p1, b), (p2, b), (p2, c)];
        before.sort_unstable();
        assert_eq!(memberships(&pool, &[p1, p2]).await, before);
        assert_eq!(count(&pool, ws, b).await, 2);
        sqlx::query("DELETE FROM person_changes")
            .execute(&pool)
            .await
            .unwrap();

        let first = rewrite(&pool, ws, p2, &merge_trace).await;
        let second = rewrite(&pool, ws, p2, &merge_trace).await;

        // Two memberships and their two history rows moved; the second run finds none.
        assert_eq!((first, second), (4, 0));
        let marks: Vec<(Uuid, i16)> =
            sqlx::query_as("SELECT person_id, classes FROM person_changes")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            marks,
            [(p1, 4)],
            "the survivor is marked in the lists class"
        );
        let lists = lists_for(&pool, ws, p1).await.unwrap();
        let names: Vec<&str> = lists.iter().map(|list| list.name.as_str()).collect();
        assert_eq!(names, ["A", "B", "C"]);
        assert_eq!(lists[1].added_at, p2_added_at);
        assert_eq!(lists[2].source, "merge");
        assert_eq!(lists_for(&pool, ws, p2).await.unwrap(), lists);
        for list in [a, b, c] {
            assert_eq!(count(&pool, ws, list).await, 1);
            assert_eq!(rows(&pool, "list_members", list).await, 1);
        }
        let left_on_loser: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM list_members WHERE person_id = $1) \
                  + (SELECT count(*) FROM list_member_changes WHERE person_id = $1)",
        )
        .bind(p2)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(left_on_loser, 0);
        let (history, more) = membership_history(&pool, ws, p1, 10, None).await.unwrap();
        assert!(!more);
        assert_eq!(history.len(), 4);
        assert!(history.iter().all(|change| change.person_id == p1));
        let (decisions, _) = list_trace(&pool, ws, merge_trace.trace_id, 20)
            .await
            .unwrap();
        assert!(
            decisions
                .iter()
                .any(|d| d.action == "list.membership" && d.outcome == "moved")
        );
        assert!(
            decisions
                .iter()
                .filter(|d| d.action == "list.membership")
                .all(|d| d.subject_ids["person"] == p1.to_string())
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merge_of_several_people_moves_every_membership_across_overlapping_lists(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let mut lists = Vec::new();
        for name in ["A", "B", "C", "D"] {
            lists.push(create_list(&pool, ws, name, "", None).await.unwrap().id);
        }
        let [a, b, c, d] = [lists[0], lists[1], lists[2], lists[3]];
        let survivor = person(&pool, ws, "1", &[("external:crm", "1")]).await;
        let first = person(&pool, ws, "2", &[("anonymous_id", "a1")]).await;
        let second = person(&pool, ws, "3", &[("anonymous_id", "a2")]).await;
        // The losers hold overlapping lists in opposite orders.
        add(&pool, ws, a, &[survivor]).await.unwrap();
        add(&pool, ws, a, &[first]).await.unwrap();
        add(&pool, ws, b, &[first]).await.unwrap();
        add(&pool, ws, c, &[first]).await.unwrap();
        add(&pool, ws, c, &[second]).await.unwrap();
        add(&pool, ws, b, &[second]).await.unwrap();
        add(&pool, ws, d, &[second]).await.unwrap();
        let joined = record(
            "9",
            &[
                ("external:crm", "1"),
                ("anonymous_id", "a1"),
                ("anonymous_id", "a2"),
            ],
        );

        let outcomes = apply_profile_batch(&pool, ws, &[joined]).await.unwrap();
        let rewrites = trace();
        for loser in [second, first] {
            rewrite(&pool, ws, loser, &rewrites).await;
        }

        assert!(
            matches!(&outcomes[0], ProfileOutcome::Applied { person_id, .. } if *person_id == survivor)
        );
        let names: Vec<String> = lists_for(&pool, ws, survivor)
            .await
            .unwrap()
            .into_iter()
            .map(|list| list.name)
            .collect();
        assert_eq!(names, ["A", "B", "C", "D"]);
        for list in [a, b, c, d] {
            assert_eq!(count(&pool, ws, list).await, 1);
            assert_eq!(rows(&pool, "list_members", list).await, 1);
        }
        let left_on_losers: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM list_members WHERE person_id = ANY($1)) \
                  + (SELECT count(*) FROM list_member_changes WHERE person_id = ANY($1))",
        )
        .bind([first, second].as_slice())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(left_on_losers, 0);
        let (history, _) = membership_history(&pool, ws, survivor, 20, None)
            .await
            .unwrap();
        assert_eq!(history.len(), 7);
    }

    async fn added_at(pool: &PgPool, list: Uuid, person: Uuid) -> OffsetDateTime {
        sqlx::query_scalar(
            "SELECT added_at FROM list_members WHERE list_id = $1 AND person_id = $2",
        )
        .bind(list)
        .bind(person)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_chained_merge_leaves_the_last_survivor_with_each_membership_once(pool: PgPool) {
        // Both orders the two rewrite jobs can run in.
        for (round, b_first) in [(0, true), (1, false)] {
            let ws = workspace(&pool, &format!("ws{round}")).await;
            let x = create_list(&pool, ws, "X", "", None).await.unwrap().id;
            let y = create_list(&pool, ws, "Y", "", None).await.unwrap().id;
            let z = create_list(&pool, ws, "Z", "", None).await.unwrap().id;
            // The oldest person survives: C, then B, then A.
            let c = person(&pool, ws, "c", &[("external:crm", "c")]).await;
            let b = person(&pool, ws, "b", &[("email", "b@example.com")]).await;
            let a = person(&pool, ws, "a", &[("anonymous_id", "a")]).await;
            add(&pool, ws, x, &[a]).await.unwrap();
            add(&pool, ws, y, &[a]).await.unwrap();
            add(&pool, ws, x, &[b]).await.unwrap();
            add(&pool, ws, z, &[b]).await.unwrap();
            add(&pool, ws, x, &[c]).await.unwrap();
            let first_in_x = added_at(&pool, x, a).await;
            // A into B, then B into C, before either rewrite job ran.
            let ab = person(
                &pool,
                ws,
                "ab",
                &[("email", "b@example.com"), ("anonymous_id", "a")],
            );
            assert_eq!(ab.await, b);
            let bc = person(
                &pool,
                ws,
                "bc",
                &[("external:crm", "c"), ("email", "b@example.com")],
            );
            assert_eq!(bc.await, c);
            let into: Vec<Option<Uuid>> =
                sqlx::query_scalar("SELECT merged_into FROM people WHERE id = ANY($1) ORDER BY id")
                    .bind([a, b].as_slice())
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert_eq!(into, [Some(c), Some(c)], "the chain is compressed");
            assert_eq!(memberships(&pool, &[a, b]).await.len(), 4);
            assert_eq!(count(&pool, ws, x).await, 3);
            let trace = trace();

            let order = if b_first { [b, a] } else { [a, b] };
            for merged in order {
                rewrite(&pool, ws, merged, &trace).await;
            }
            let again = rewrite(&pool, ws, a, &trace).await + rewrite(&pool, ws, b, &trace).await;

            assert_eq!(again, 0, "a second run moves nothing");
            let mut expected = vec![(c, x), (c, y), (c, z)];
            expected.sort_unstable();
            assert_eq!(memberships(&pool, &[a, b, c]).await, expected);
            for list in [x, y, z] {
                assert_eq!(count(&pool, ws, list).await, 1);
                assert_eq!(rows(&pool, "list_members", list).await, 1);
            }
            assert_eq!(added_at(&pool, x, c).await, first_in_x);
            let left: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM list_member_changes WHERE person_id = ANY($1)",
            )
            .bind([a, b].as_slice())
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(left, 0);
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn between_a_merge_and_its_rewrite_adding_or_removing_the_survivor_follows_the_loser(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let kept = create_list(&pool, ws, "Kept", "", None).await.unwrap().id;
        let left = create_list(&pool, ws, "Left", "", None).await.unwrap().id;
        let survivor = person(&pool, ws, "1", &[("external:crm", "1")]).await;
        let loser = person(&pool, ws, "2", &[("anonymous_id", "a")]).await;
        add(&pool, ws, kept, &[loser]).await.unwrap();
        add(&pool, ws, left, &[loser]).await.unwrap();
        let joined = person(
            &pool,
            ws,
            "3",
            &[("external:crm", "1"), ("anonymous_id", "a")],
        );
        assert_eq!(joined.await, survivor);

        // The rewrite has not run: the loser's rows are the person's memberships.
        let added = add(&pool, ws, kept, &[survivor]).await.unwrap();
        let removed = remove(&pool, ws, left, &[survivor]).await.unwrap();

        assert_eq!((added.changed, added.unchanged), (0, 1));
        assert_eq!((removed.changed, removed.unchanged), (1, 0));
        assert_eq!(
            (count(&pool, ws, kept).await, count(&pool, ws, left).await),
            (1, 0)
        );
        rewrite(&pool, ws, loser, &trace()).await;
        assert_eq!(
            memberships(&pool, &[survivor, loser]).await,
            [(survivor, kept)],
            "a removed membership does not come back with the rewrite"
        );
        assert_eq!(
            (count(&pool, ws, kept).await, count(&pool, ws, left).await),
            (1, 0)
        );
        let (history, _) = membership_history(&pool, ws, survivor, 10, None)
            .await
            .unwrap();
        let on_left: Vec<&str> = history
            .iter()
            .filter(|change| change.list_id == left)
            .map(|change| change.action.as_str())
            .collect();
        assert_eq!(on_left, ["removed", "added"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn members_page_newest_first_with_the_strongest_identifier(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let list = create_list(&pool, ws, "VIPs", "", None).await.unwrap().id;
        let rich = person(
            &pool,
            ws,
            "1",
            &[("email", "ada@example.com"), ("anonymous_id", "a1")],
        )
        .await;
        let anonymous = person(&pool, ws, "2", &[("anonymous_id", "a2")]).await;
        let external = person(
            &pool,
            ws,
            "3",
            &[("email", "bob@example.com"), ("external:crm", "9")],
        )
        .await;
        for person in [rich, anonymous, external] {
            add(&pool, ws, list, &[person]).await.unwrap();
        }

        let (first, more) = members(&pool, ws, list, 2, None).await.unwrap();
        let last = first.last().unwrap();
        let (second, more_after) =
            members(&pool, ws, list, 2, Some((last.added_at, last.person_id)))
                .await
                .unwrap();

        assert!(more);
        assert!(!more_after);
        let ids: Vec<Uuid> = first.iter().chain(&second).map(|m| m.person_id).collect();
        assert_eq!(ids, [external, anonymous, rich]);
        let kinds: Vec<&str> = first
            .iter()
            .chain(&second)
            .map(|m| m.identifier.as_ref().unwrap().kind.as_str())
            .collect();
        assert_eq!(kinds, ["external:crm", "anonymous_id", "email"]);
        assert_eq!(first[0].identifier.as_ref().unwrap().value, "9");
        assert!(!format!("{:?}", first[0]).contains("\"9\""));
        assert_eq!(first[0].source, "api");
        assert!(
            members(&pool, ws, Uuid::now_v7(), 10, None)
                .await
                .unwrap()
                .0
                .is_empty()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_persons_lists_and_history_page_and_stay_in_their_workspace(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let first = create_list(&pool, one, "First", "", None).await.unwrap().id;
        let second = create_list(&pool, one, "Second", "", None)
            .await
            .unwrap()
            .id;
        let ada = person(&pool, one, "1", &[("email", "ada@example.com")]).await;
        add(&pool, one, first, &[ada]).await.unwrap();
        add(&pool, one, second, &[ada]).await.unwrap();
        remove(&pool, one, first, &[ada]).await.unwrap();

        let (newest, more) = membership_history(&pool, one, ada, 2, None).await.unwrap();
        let cursor = newest.last().map(|c| (c.occurred_at, c.id));
        let (rest, more_after) = membership_history(&pool, one, ada, 2, cursor)
            .await
            .unwrap();

        assert!(more);
        assert!(!more_after);
        let story: Vec<(&str, &str)> = newest
            .iter()
            .chain(&rest)
            .map(|c| (c.list_name.as_str(), c.action.as_str()))
            .collect();
        assert_eq!(
            story,
            [
                ("First", "removed"),
                ("Second", "added"),
                ("First", "added")
            ]
        );
        let lists = lists_for(&pool, one, ada).await.unwrap();
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].name, "Second");
        assert!(lists_for(&pool, two, ada).await.unwrap().is_empty());
        assert!(
            membership_history(&pool, two, ada, 10, None)
                .await
                .unwrap()
                .0
                .is_empty()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn overlapping_concurrent_adds_keep_the_count_exact(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let list = create_list(&pool, ws, "VIPs", "", None).await.unwrap().id;
        let mut people = Vec::new();
        for n in 0..6 {
            let anonymous = format!("a{n}");
            people.push(person(&pool, ws, &n.to_string(), &[("anonymous_id", &anonymous)]).await);
        }
        let run = |slice: Vec<Uuid>| {
            let pool = pool.clone();
            tokio::spawn(async move { add(&pool, ws, list, &slice).await.unwrap() })
        };

        let (left, right) = tokio::join!(run(people[..4].to_vec()), run(people[2..].to_vec()));

        let (left, right) = (left.unwrap(), right.unwrap());
        assert_eq!(left.changed + right.changed, 6);
        assert_eq!(count(&pool, ws, list).await, 6);
        assert_eq!(rows(&pool, "list_members", list).await, 6);
        assert_eq!(rows(&pool, "list_member_changes", list).await, 6);
    }

    #[test]
    fn the_counts_net_per_list_in_id_order_and_a_rolled_back_savepoint_drops_its_own() {
        let ws = WorkspaceId::new();
        let (low, high) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let mut counts = ListCounts::default();
        counts.record(ws, high, 2);
        counts.record(ws, low, 1);
        counts.record(ws, low, 0);
        let mark = counts.mark();
        counts.record(ws, low, 5);
        counts.record(ws, high, -2);

        counts.discard_after(mark);
        assert_eq!(counts.totals(), [(ws, low, 1), (ws, high, 2)]);
        counts.record(ws, high, -2);
        assert_eq!(
            counts.totals(),
            [(ws, low, 1)],
            "a list that nets to nothing is left alone"
        );
    }

    /// The updates the transaction on `connection` has made to `lists` so far.
    async fn list_updates(connection: &mut PgConnection) -> i64 {
        sqlx::query_scalar(
            "SELECT COALESCE(sum(n_tup_upd), 0)::bigint FROM pg_stat_xact_user_tables \
             WHERE relname = 'lists'",
        )
        .fetch_one(connection)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_transactions_adds_move_each_count_once_when_it_applies_them(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let first = create_list(&pool, ws, "First", "", None).await.unwrap().id;
        let second = create_list(&pool, ws, "Second", "", None).await.unwrap().id;
        let ada = person(&pool, ws, "1", &[("email", "ada@example.com")]).await;
        let bob = person(&pool, ws, "2", &[("email", "bob@example.com")]).await;
        let trace = trace();
        let write = |list| ListWrite {
            workspace: ws,
            list,
            source: ListSource::Sync,
            actor: None,
            trace: &trace,
        };
        let mut tx = begin(&pool).await.unwrap();
        let mut counts = ListCounts::default();

        for (list, person) in [(second, ada), (first, ada), (first, bob), (first, bob)] {
            add_members_into(&mut tx, &write(list), &[person], &mut counts)
                .await
                .unwrap();
        }

        assert_eq!(
            list_updates(&mut tx).await,
            0,
            "the adds leave the list rows alone"
        );
        counts.apply(&mut tx).await.unwrap();
        assert_eq!(list_updates(&mut tx).await, 2, "one update per list");
        counts.apply(&mut tx).await.unwrap();
        assert_eq!(
            list_updates(&mut tx).await,
            2,
            "applying empties the buffer"
        );
        commit(tx).await.unwrap();
        assert_eq!(
            (
                count(&pool, ws, first).await,
                count(&pool, ws, second).await
            ),
            (2, 1)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_open_add_holds_its_list_for_key_share_only_so_adds_removals_moves_and_erasures_go_on(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let list = create_list(&pool, ws, "VIPs", "", None).await.unwrap().id;
        let ada = person(&pool, ws, "1", &[("email", "ada@example.com")]).await;
        let bob = person(&pool, ws, "2", &[("email", "bob@example.com")]).await;
        let cyd = person(&pool, ws, "3", &[("email", "cyd@example.com")]).await;
        let survivor = person(&pool, ws, "4", &[("external:crm", "1")]).await;
        let loser = person(&pool, ws, "5", &[("anonymous_id", "a")]).await;
        let dan = person(&pool, ws, "7", &[("email", "dan@example.com")]).await;
        add(&pool, ws, list, &[cyd, loser, dan]).await.unwrap();
        let joined = person(
            &pool,
            ws,
            "6",
            &[("external:crm", "1"), ("anonymous_id", "a")],
        );
        assert_eq!(joined.await, survivor);
        let trace = trace();
        let write = ListWrite {
            workspace: ws,
            list,
            source: ListSource::Sync,
            actor: None,
            trace: &trace,
        };
        // A batch that has added Ada and goes on.
        let mut open = begin(&pool).await.unwrap();
        let mut counts = ListCounts::default();
        add_members_into(&mut open, &write, &[ada], &mut counts)
            .await
            .unwrap();

        let beside = std::time::Duration::from_secs(5);
        let added = tokio::time::timeout(beside, add(&pool, ws, list, &[bob])).await;
        let removed = tokio::time::timeout(beside, remove(&pool, ws, list, &[cyd])).await;
        let moved = tokio::time::timeout(beside, rewrite(&pool, ws, loser, &trace)).await;
        let erased = tokio::time::timeout(beside, async {
            let mut tx = begin(&pool).await.unwrap();
            crate::delete_person(&mut tx, ws, dan, crate::DeleteReason::Erasure, &trace)
                .await
                .unwrap();
            commit(tx).await.unwrap();
        })
        .await;

        assert_eq!(
            [
                added.is_ok(),
                removed.is_ok(),
                moved.is_ok(),
                erased.is_ok()
            ],
            [true; 4],
            "an add, a removal, a merge's move and an erasure, each while the batch is open"
        );
        counts.apply(&mut open).await.unwrap();
        commit(open).await.unwrap();
        let mut expected = vec![(ada, list), (bob, list), (survivor, list)];
        expected.sort_unstable();
        assert_eq!(
            memberships(&pool, &[ada, bob, cyd, survivor, loser, dan]).await,
            expected
        );
        assert_eq!(count(&pool, ws, list).await, 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_a_list_removes_its_members_and_history(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let list = create_list(&pool, ws, "VIPs", "", None).await.unwrap().id;
        let ada = person(&pool, ws, "1", &[("email", "ada@example.com")]).await;
        add(&pool, ws, list, &[ada]).await.unwrap();
        let mut connection = pool.acquire().await.unwrap();

        crate::delete_list(&mut connection, ws, list).await.unwrap();

        assert_eq!(rows(&pool, "list_members", list).await, 0);
        assert_eq!(rows(&pool, "list_member_changes", list).await, 0);
        assert!(lists_for(&pool, ws, ada).await.unwrap().is_empty());
    }

    #[test]
    fn a_source_has_a_stable_name_and_an_error_says_what_went_wrong() {
        assert_eq!(ListSource::Merge.as_str(), "merge");
        assert_eq!(ListSource::Flow.as_str(), "flow");
        assert_eq!(ListSource::Sync.as_str(), "sync");
        assert_eq!(
            MembershipError::TooMany.to_string(),
            "a call takes at most 1000 people"
        );
        assert_eq!(MembershipError::NoSuchList.to_string(), "no such list");
    }

    #[test]
    fn the_strongest_identifier_wins_and_the_first_of_equals_is_kept() {
        let row = MemberRow {
            person_id: Uuid::now_v7(),
            added_at: datetime!(2026-01-01 00:00 UTC),
            source: "api".into(),
            kinds: Some(vec!["anonymous_id".into(), "email".into(), "email".into()]),
            vals: Some(vec![
                "a".into(),
                "x@example.com".into(),
                "y@example.com".into(),
            ]),
        };

        let member = ListMember::from(row);

        assert_eq!(member.identifier.unwrap().value, "x@example.com");
        let none = ListMember::from(MemberRow {
            person_id: Uuid::now_v7(),
            added_at: datetime!(2026-01-01 00:00 UTC),
            source: "api".into(),
            kinds: None,
            vals: None,
        });
        assert!(none.identifier.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_real_change_marks_the_person_in_the_lists_class_and_a_no_op_marks_nobody(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        crate::segments::tests_support::segment_reading(
            &pool,
            ws,
            zirv_flow_core::segment::ChangeClasses::LISTS,
        )
        .await;
        let list = create_list(&pool, ws, "VIPs", "", None).await.unwrap().id;
        let ada = person(&pool, ws, "1", &[("email", "ada@example.com")]).await;
        let bob = person(&pool, ws, "2", &[("email", "bob@example.com")]).await;
        add(&pool, ws, list, &[bob]).await.unwrap();
        let clear = || sqlx::query("DELETE FROM person_changes").execute(&pool);
        clear().await.unwrap();

        add(&pool, ws, list, &[ada, bob]).await.unwrap();

        let marks: Vec<(Uuid, i16, Option<String>, Option<String>)> =
            sqlx::query_as("SELECT person_id, classes, cause_kind, cause_id FROM person_changes")
                .fetch_all(&pool)
                .await
                .unwrap();
        // Bob was a member already, so only Ada changed.
        assert_eq!(marks, [(ada, 4, Some("api".into()), Some("test".into()))]);
        clear().await.unwrap();

        remove(&pool, ws, list, &[ada]).await.unwrap();
        remove(&pool, ws, list, &[ada]).await.unwrap();

        let marks: Vec<(Uuid, i16)> =
            sqlx::query_as("SELECT person_id, classes FROM person_changes")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(marks, [(ada, 4)]);
    }
}
