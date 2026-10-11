//! Reviewing identity conflicts (architecture section 7.3): the list a marketer works through,
//! and the two ways to close one. There is no unmerge: a conflict is kept separate, or its
//! identifier moves to one of the two people.

use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::people::{BlockReason, Identifier, IdentifierKind, merge_allowed};
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::apply::record_decision;
use super::merge::current_person;
use super::profiles::{IdentityConflict, conflict_columns};
use crate::StoreError;

/// How a conflict is closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictResolution {
    /// The two people are different people; the shared identifier stays where it is.
    KeepSeparate,
    /// The shared identifier belongs to `to_person_id`, one of the two people.
    MoveIdentifier { to_person_id: Uuid },
}

/// Why a conflict could not be closed.
#[derive(Debug)]
pub enum ResolveError {
    NotFound,
    AlreadyResolved,
    /// The target is neither of the two people.
    NotAParty,
    /// One of the people, or the target the caller named, was merged away since the caller
    /// looked: the caller should reload.
    Changed,
    /// The conflict predates merges and does not name its identifier.
    NoIdentifier,
    /// The identifier already belongs to the target.
    AlreadyThere,
    /// Moving the identifier would break an identity rule for the target.
    Blocked(BlockReason),
    Store(StoreError),
}

impl From<StoreError> for ResolveError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// A page of conflicts, newest first: those still open, or those already resolved. `before`
/// is the `(created_at, id)` of the last conflict of the previous page. The flag says
/// whether more follow.
pub async fn list_conflicts<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    resolved: bool,
    limit: u32,
    before: Option<(OffsetDateTime, Uuid)>,
) -> Result<(Vec<IdentityConflict>, bool), StoreError> {
    let mut rows: Vec<IdentityConflict> = sqlx::query_as(concat!(
        "SELECT ",
        conflict_columns!(),
        " FROM identity_conflicts \
          WHERE workspace_id = $1 AND (resolved_at IS NOT NULL) = $2 \
            AND ($3::timestamptz IS NULL OR (created_at, id) < ($3, $4)) \
          ORDER BY created_at DESC, id DESC LIMIT $5"
    ))
    .bind(workspace.as_uuid())
    .bind(resolved)
    .bind(before.map(|(created_at, _)| created_at))
    .bind(before.map(|(_, id)| id))
    .bind(i64::from(limit) + 1)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok((rows, more))
}

pub async fn find_conflict<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<IdentityConflict>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        conflict_columns!(),
        " FROM identity_conflicts WHERE workspace_id = $1 AND id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Closes an open conflict on the caller's transaction and appends an
/// `identity.conflict.resolved` decision (ids only). Moving the identifier recomputes
/// nothing: identities carry no traits. Returns the closed conflict.
pub async fn resolve_conflict(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    resolution: ConflictResolution,
    trace: &TraceContext,
) -> Result<IdentityConflict, ResolveError> {
    let conflict: IdentityConflict = sqlx::query_as(concat!(
        "SELECT ",
        conflict_columns!(),
        " FROM identity_conflicts WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?
    .ok_or(ResolveError::NotFound)?;
    if conflict.resolved_at.is_some() {
        return Err(ResolveError::AlreadyResolved);
    }
    let outcome = match resolution {
        ConflictResolution::KeepSeparate => "kept_separate",
        ConflictResolution::MoveIdentifier { to_person_id } => {
            move_identifier(connection, workspace, &conflict, to_person_id).await?;
            "identifier_moved"
        }
    };
    let closed: IdentityConflict = sqlx::query_as(concat!(
        "UPDATE identity_conflicts SET resolved_at = now(), resolution = $3 \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        conflict_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(outcome)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    record_decision(
        connection,
        workspace,
        trace,
        "identity.conflict.resolved",
        outcome,
        None,
        vec![
            ("conflict", id),
            ("person", conflict.person_id),
            ("other_person", conflict.other_person_id),
        ],
    )
    .await?;
    Ok(closed)
}

async fn move_identifier(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    conflict: &IdentityConflict,
    to_person_id: Uuid,
) -> Result<(), ResolveError> {
    let identity_id = conflict.identity_id.ok_or(ResolveError::NoIdentifier)?;
    // The people the conflict names may have been merged since; the survivors stand for them.
    let mut parties = Vec::with_capacity(2);
    for person in [conflict.person_id, conflict.other_person_id] {
        let current = current_person(&mut *connection, workspace, person)
            .await?
            .ok_or(ResolveError::NotAParty)?;
        parties.push(current);
    }
    let target = current_person(&mut *connection, workspace, to_person_id)
        .await?
        .filter(|target| parties.contains(target))
        .ok_or(ResolveError::NotAParty)?;

    let mut to_lock = parties.clone();
    to_lock.sort();
    to_lock.dedup();
    to_lock.push(to_person_id);
    to_lock.sort();
    to_lock.dedup();
    let locked: Vec<Option<Uuid>> = sqlx::query_scalar(
        "SELECT merged_into FROM people \
         WHERE workspace_id = $1 AND id = ANY($2) ORDER BY id FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(&to_lock)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    // Merged away after the caller looked, or between reading the parties and locking them.
    if locked.iter().any(Option::is_some) {
        return Err(ResolveError::Changed);
    }

    let identity: Option<(Uuid, String, String)> = sqlx::query_as(
        "SELECT person_id, kind, value FROM identities \
         WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(identity_id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let (owner, kind, value) = identity.ok_or(ResolveError::NoIdentifier)?;
    if !parties.contains(&owner) {
        return Err(ResolveError::NotAParty);
    }
    if owner == target {
        return Err(ResolveError::AlreadyThere);
    }

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT kind, value FROM identities WHERE workspace_id = $1 AND person_id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(target)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut union = Vec::with_capacity(rows.len() + 1);
    for (kind, value) in rows.into_iter().chain([(kind, value)]) {
        let kind = IdentifierKind::parse(&kind)
            .map_err(|_| StoreError::query(sqlx::Error::Decode("unknown identity kind".into())))?;
        union.push(Identifier::from_stored(kind, value));
    }
    merge_allowed(&union).map_err(ResolveError::Blocked)?;

    sqlx::query("UPDATE identities SET person_id = $3 WHERE workspace_id = $1 AND id = $2")
        .bind(workspace.as_uuid())
        .bind(identity_id)
        .bind(target)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_batch, list_identities};
    use crate::{begin, commit, create_workspace};

    fn trace() -> TraceContext {
        TraceContext::root(Cause::api("test"))
    }

    async fn workspace(pool: &PgPool) -> WorkspaceId {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn record(key: &str, identifiers: &[(&str, &str)]) -> ProfileRecord {
        ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
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

    fn person_of(outcome: &ProfileOutcome) -> Uuid {
        match outcome {
            ProfileOutcome::Applied { person_id, .. }
            | ProfileOutcome::Unchanged { person_id, .. } => *person_id,
            ProfileOutcome::Rejected { reason } => panic!("rejected: {reason}"),
        }
    }

    /// P1 holds `{external:crm 1, email x}`, P2 holds `{external:crm 2}`; a record with
    /// `{external:crm 2, email x}` leaves one open `unique_kind` conflict about the email.
    async fn blocked(pool: &PgPool, ws: WorkspaceId) -> (Uuid, Uuid, IdentityConflict) {
        let outcomes = apply_profile_batch(
            pool,
            ws,
            &[
                record("1", &[("external:crm", "1"), ("email", "x@example.com")]),
                record("2", &[("external:crm", "2")]),
            ],
        )
        .await
        .unwrap();
        let (one, two) = (person_of(&outcomes[0]), person_of(&outcomes[1]));
        apply_profile_batch(
            pool,
            ws,
            &[record(
                "3",
                &[("external:crm", "2"), ("email", "x@example.com")],
            )],
        )
        .await
        .unwrap();
        let (open, _) = list_conflicts(pool, ws, false, 10, None).await.unwrap();
        assert_eq!(open.len(), 1);
        (one, two, open.into_iter().next().unwrap())
    }

    async fn resolve(
        pool: &PgPool,
        ws: WorkspaceId,
        id: Uuid,
        resolution: ConflictResolution,
    ) -> Result<IdentityConflict, ResolveError> {
        let mut tx = begin(pool).await.unwrap();
        let closed = resolve_conflict(&mut tx, ws, id, resolution, &trace()).await?;
        commit(tx).await.unwrap();
        Ok(closed)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn keeping_two_people_separate_closes_the_conflict_and_changes_nothing_else(
        pool: PgPool,
    ) {
        let ws = workspace(&pool).await;
        let (one, two, conflict) = blocked(&pool, ws).await;

        let closed = resolve(&pool, ws, conflict.id, ConflictResolution::KeepSeparate)
            .await
            .unwrap();

        assert!(closed.resolved_at.is_some());
        assert_eq!(closed.resolution.as_deref(), Some("kept_separate"));
        assert_eq!(list_identities(&pool, ws, one).await.unwrap().len(), 2);
        assert_eq!(list_identities(&pool, ws, two).await.unwrap().len(), 1);
        let (open, _) = list_conflicts(&pool, ws, false, 10, None).await.unwrap();
        let (resolved, _) = list_conflicts(&pool, ws, true, 10, None).await.unwrap();
        assert!(open.is_empty());
        assert_eq!(resolved.len(), 1);
        assert!(matches!(
            resolve(&pool, ws, conflict.id, ConflictResolution::KeepSeparate).await,
            Err(ResolveError::AlreadyResolved)
        ));
        let decisions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records \
             WHERE action = 'identity.conflict.resolved' AND outcome = 'kept_separate'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(decisions, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn moving_the_identifier_gives_it_to_the_chosen_person(pool: PgPool) {
        let ws = workspace(&pool).await;
        let (one, two, conflict) = blocked(&pool, ws).await;
        assert_eq!(conflict.person_id, two);
        assert_eq!(conflict.other_person_id, one);

        let closed = resolve(
            &pool,
            ws,
            conflict.id,
            ConflictResolution::MoveIdentifier { to_person_id: two },
        )
        .await;

        // P2 already holds external:crm 2 and the email is not external, so it may take it.
        let closed = closed.unwrap();
        assert_eq!(closed.resolution.as_deref(), Some("identifier_moved"));
        let kinds = |identities: Vec<crate::StoredIdentity>| {
            identities
                .into_iter()
                .map(|i| i.kind)
                .collect::<Vec<String>>()
        };
        assert_eq!(
            kinds(list_identities(&pool, ws, two).await.unwrap()),
            ["email", "external:crm"]
        );
        assert_eq!(
            kinds(list_identities(&pool, ws, one).await.unwrap()),
            ["external:crm"]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_move_that_breaks_a_rule_a_stranger_target_and_a_repeat_are_refused(pool: PgPool) {
        let ws = workspace(&pool).await;
        let (one, two, conflict) = blocked(&pool, ws).await;
        // Fill P2 with ten emails, so the shared one would be an eleventh.
        let ten: Vec<(String, String)> = (0..10)
            .map(|n| ("email".to_owned(), format!("e{n}@example.com")))
            .collect();
        let mut fill = record("fill", &[("external:crm", "2")]);
        fill.identifiers.extend(ten);
        apply_profile_batch(&pool, ws, &[fill]).await.unwrap();

        let over = resolve(
            &pool,
            ws,
            conflict.id,
            ConflictResolution::MoveIdentifier { to_person_id: two },
        )
        .await;
        let stranger = resolve(
            &pool,
            ws,
            conflict.id,
            ConflictResolution::MoveIdentifier {
                to_person_id: Uuid::now_v7(),
            },
        )
        .await;
        let already = resolve(
            &pool,
            ws,
            conflict.id,
            ConflictResolution::MoveIdentifier { to_person_id: one },
        )
        .await;

        assert!(matches!(
            over,
            Err(ResolveError::Blocked(BlockReason::MaxPerPerson))
        ));
        assert!(matches!(stranger, Err(ResolveError::NotAParty)));
        assert!(matches!(already, Err(ResolveError::AlreadyThere)));
        let still = find_conflict(&pool, ws, conflict.id)
            .await
            .unwrap()
            .unwrap();
        assert!(still.resolved_at.is_none());
        assert!(matches!(
            resolve(&pool, ws, Uuid::now_v7(), ConflictResolution::KeepSeparate).await,
            Err(ResolveError::NotFound)
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_list_pages_newest_first_by_keyset(pool: PgPool) {
        let ws = workspace(&pool).await;
        let (_, _, first) = blocked(&pool, ws).await;
        for _ in 0..4 {
            // Another open conflict about the same identifier, from another person.
            let extra = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO people (id, workspace_id) \
                 SELECT $1, workspace_id FROM identity_conflicts WHERE id = $2",
            )
            .bind(extra)
            .bind(first.id)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO identity_conflicts \
                    (id, workspace_id, person_id, other_person_id, kind, source_record_id, reason) \
                 SELECT $1, workspace_id, $3, other_person_id, kind, source_record_id, \
                        reason FROM identity_conflicts WHERE id = $2",
            )
            .bind(Uuid::now_v7())
            .bind(first.id)
            .bind(extra)
            .execute(&pool)
            .await
            .unwrap();
        }

        let (page_one, more) = list_conflicts(&pool, ws, false, 2, None).await.unwrap();
        let cursor = page_one.last().map(|c| (c.created_at, c.id));
        let (page_two, more_two) = list_conflicts(&pool, ws, false, 2, cursor).await.unwrap();
        let cursor = page_two.last().map(|c| (c.created_at, c.id));
        let (page_three, more_three) = list_conflicts(&pool, ws, false, 2, cursor).await.unwrap();

        assert!(more && more_two && !more_three);
        let ids: Vec<(OffsetDateTime, Uuid)> = page_one
            .iter()
            .chain(&page_two)
            .chain(&page_three)
            .map(|c| (c.created_at, c.id))
            .collect();
        assert_eq!(ids.len(), 5);
        assert!(ids.windows(2).all(|pair| pair[0] > pair[1]));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_move_to_a_person_merged_away_since_is_refused_and_moves_nothing(pool: PgPool) {
        let ws = workspace(&pool).await;
        let (one, _two, conflict) = blocked(&pool, ws).await;
        // `one` holds the shared email; a merge elsewhere folds it into a newcomer.
        let survivor = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(survivor)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE people SET merged_into = $1 WHERE id = $2")
            .bind(survivor)
            .bind(one)
            .execute(&pool)
            .await
            .unwrap();

        let stale = resolve(
            &pool,
            ws,
            conflict.id,
            ConflictResolution::MoveIdentifier { to_person_id: one },
        )
        .await;

        assert!(matches!(stale, Err(ResolveError::Changed)));
        let owner: Uuid =
            sqlx::query_scalar("SELECT person_id FROM identities WHERE kind = 'email'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(owner, one);
        let still = find_conflict(&pool, ws, conflict.id)
            .await
            .unwrap()
            .unwrap();
        assert!(still.resolved_at.is_none());
    }
}
