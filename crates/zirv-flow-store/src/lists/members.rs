//! List memberships: adding and removing people, reading them, the history of every change,
//! and the move a merge makes. A change follows `merged_into`, is idempotent, keeps
//! `member_count` exact in its own transaction and leaves one history row and one
//! `list.membership` decision per real change. Decisions carry ids only.

use std::collections::HashMap;
use std::fmt;

use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::people::IdentifierKind;
use zirv_flow_core::{TraceContext, UserId, WorkspaceId};

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
}

impl ListSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ui => "ui",
            Self::Api => "api",
            Self::Import => "import",
            Self::Merge => "merge",
            Self::Flow => "flow",
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
}

/// Resolves each person to the one that stands for it now and locks those people, so a merge
/// cannot move memberships while this transaction writes them. Keyed by the id given; people
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
        let locked: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
            "SELECT id, merged_into FROM people \
             WHERE workspace_id = $1 AND id = ANY($2) ORDER BY id FOR SHARE",
        )
        .bind(workspace.as_uuid())
        .bind(&targets)
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        // A person merged away while the lock was awaited is resolved again.
        if locked.len() == targets.len() && locked.iter().all(|(_, merged)| merged.is_none()) {
            return Ok(rows.into_iter().collect());
        }
    }
    Err(StoreError::retryable_race())
}

/// The distinct current people to change, and an outcome that already counts every existing
/// person as unchanged and lists the missing ones. The list is locked for the rest of the
/// transaction, after the people, which is the order a merge locks in.
async fn prepare(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    list: Uuid,
    people: &[Uuid],
) -> Result<(Vec<Uuid>, MembershipOutcome), MembershipError> {
    if people.len() > MAX_PEOPLE_PER_CALL {
        return Err(MembershipError::TooMany);
    }
    let current = lock_current_people(connection, workspace, people).await?;
    let locked: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM lists WHERE workspace_id = $1 AND id = $2 FOR UPDATE")
            .bind(workspace.as_uuid())
            .bind(list)
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

/// The change being recorded: who, how and why.
struct Recorded<'a> {
    workspace: WorkspaceId,
    list: Uuid,
    change: Change,
    source: ListSource,
    actor: Option<UserId>,
    trace: &'a TraceContext,
}

/// Writes the history rows and decisions of `people` and moves the count.
async fn record_changes(
    connection: &mut PgConnection,
    recorded: &Recorded<'_>,
    people: &[Uuid],
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
    .bind(recorded.workspace.as_uuid())
    .bind(recorded.list)
    .bind(recorded.change.as_str())
    .bind(recorded.source.as_str())
    .bind(recorded.actor.map(|user| user.as_uuid()))
    .bind(recorded.trace.trace_id.as_uuid())
    .bind(&ids)
    .bind(people)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let count = i64::try_from(people.len()).unwrap_or(i64::MAX);
    let delta = match recorded.change {
        Change::Added => count,
        Change::Removed => -count,
    };
    sqlx::query(
        "UPDATE lists SET member_count = member_count + $3 WHERE workspace_id = $1 AND id = $2",
    )
    .bind(recorded.workspace.as_uuid())
    .bind(recorded.list)
    .bind(delta)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let decisions: Vec<NewDecision> = people
        .iter()
        .map(|person| NewDecision {
            workspace_id: recorded.workspace,
            trace: recorded.trace.clone(),
            action: "list.membership",
            outcome: recorded.change.as_str(),
            reason: Some(recorded.source.as_str()),
            subject_ids: vec![("list", recorded.list), ("person", *person)],
            job: None,
        })
        .collect();
    append_decisions(connection, &decisions).await
}

/// Adds `people` to a list, following merges. Adding a member again writes nothing. At most
/// [`MAX_PEOPLE_PER_CALL`] people; people that do not exist are reported, never created.
pub async fn add_members(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    list: Uuid,
    people: &[Uuid],
    source: ListSource,
    actor: Option<UserId>,
    trace: &TraceContext,
) -> Result<MembershipOutcome, MembershipError> {
    let (targets, mut outcome) = prepare(connection, workspace, list, people).await?;
    let added: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO list_members (workspace_id, list_id, person_id, source) \
         SELECT $1, $2, t.person_id, $4 FROM unnest($3::uuid[]) AS t(person_id) \
         ON CONFLICT DO NOTHING RETURNING person_id",
    )
    .bind(workspace.as_uuid())
    .bind(list)
    .bind(&targets)
    .bind(source.as_str())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let recorded = Recorded {
        workspace,
        list,
        change: Change::Added,
        source,
        actor,
        trace,
    };
    record_changes(connection, &recorded, &added).await?;
    outcome.changed = added.len();
    outcome.unchanged -= added.len();
    Ok(outcome)
}

/// Removes `people` from a list, following merges. Removing a non-member writes nothing.
pub async fn remove_members(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    list: Uuid,
    people: &[Uuid],
    source: ListSource,
    actor: Option<UserId>,
    trace: &TraceContext,
) -> Result<MembershipOutcome, MembershipError> {
    let (targets, mut outcome) = prepare(connection, workspace, list, people).await?;
    let removed: Vec<Uuid> = sqlx::query_scalar(
        "DELETE FROM list_members \
         WHERE workspace_id = $1 AND list_id = $2 AND person_id = ANY($3) RETURNING person_id",
    )
    .bind(workspace.as_uuid())
    .bind(list)
    .bind(&targets)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let recorded = Recorded {
        workspace,
        list,
        change: Change::Removed,
        source,
        actor,
        trace,
    };
    record_changes(connection, &recorded, &removed).await?;
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

/// Moves the memberships and history of `loser` to `survivor`, in the transaction of the
/// merge, which holds both people locked. The survivor ends up in the union of both people's
/// lists and keeps the earlier `added_at` of the two; counts follow.
pub(crate) async fn move_on_merge(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    loser: Uuid,
    survivor: Uuid,
    trace: &TraceContext,
) -> Result<(), StoreError> {
    let held: Vec<Uuid> = sqlx::query_scalar(
        "SELECT list_id FROM list_members \
         WHERE workspace_id = $1 AND person_id = $2 ORDER BY list_id",
    )
    .bind(workspace.as_uuid())
    .bind(loser)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if !held.is_empty() {
        sqlx::query(
            "SELECT id FROM lists WHERE workspace_id = $1 AND id = ANY($2) \
             ORDER BY id FOR UPDATE",
        )
        .bind(workspace.as_uuid())
        .bind(&held)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
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
        sqlx::query("DELETE FROM list_members WHERE workspace_id = $1 AND person_id = $2")
            .bind(workspace.as_uuid())
            .bind(loser)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
        sqlx::query(
            "UPDATE lists SET member_count = member_count - 1 \
             WHERE workspace_id = $1 AND id = ANY($2)",
        )
        .bind(workspace.as_uuid())
        .bind(&shared)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    }
    let moved = sqlx::query(
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
    if !held.is_empty() || moved > 0 {
        append_decision(
            connection,
            &NewDecision {
                workspace_id: workspace,
                trace: trace.clone(),
                action: "list.membership",
                outcome: "moved",
                reason: Some(ListSource::Merge.as_str()),
                subject_ids: vec![("survivor", survivor), ("merged", loser)],
                job: None,
            },
        )
        .await?;
    }
    Ok(())
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

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merge_leaves_the_survivor_in_the_union_of_both_peoples_lists(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
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
}
