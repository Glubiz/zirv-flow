//! Identity resolution and merges (architecture section 7.3).
//!
//! Given the identifiers of one record, find the person it belongs to, creating one when no
//! identifier is known. When identifiers belong to several people they are merged into the
//! oldest one, unless the identity rules forbid it: then the record attaches to the owner of
//! the strongest identifier and each other person is reported as a conflict for review.
//!
//! Every person a resolution touches is locked with `FOR UPDATE` in id order, so concurrent
//! resolutions and merges cannot deadlock on each other.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::people::{BlockReason, Identifier, IdentifierKind, merge_allowed, rule_for};
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::merge::merge_people;
use crate::StoreError;

/// How many times a record's resolution runs when identity inserts keep losing races.
const MAX_ATTEMPTS: u32 = 3;

/// An identifier that points at a person other than the one the record attached to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Conflict {
    pub other_person_id: Uuid,
    pub kind: String,
    /// The identity row of the shared identifier.
    pub identity_id: Uuid,
    /// The rule that forbade the merge: `unique_kind` or `max_per_person`.
    pub reason: &'static str,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Resolution {
    pub person_id: Uuid,
    pub created: bool,
    pub conflicts: Vec<Conflict>,
    /// The people merged into `person_id` by this resolution.
    pub merged: Vec<Uuid>,
    /// Identifiers left off the person because it holds the most of their kind it may.
    pub skipped: usize,
    /// The person the record would have attached to, when it became a new person instead.
    pub split_from: Option<Uuid>,
}

struct Owned {
    identifier_index: usize,
    person_id: Uuid,
    identity_id: Uuid,
}

struct LockedPerson {
    id: Uuid,
    merged_into: Option<Uuid>,
    created_at: OffsetDateTime,
}

/// Resolves the person for `identifiers` (normalized, sorted, non-empty). `existing` is the
/// person the record is already attached to, which wins ties and receives a record whose
/// identifiers are all unknown. An identity insert that loses a unique race, or a person that
/// was merged away while it was being locked, re-runs the resolution, up to three attempts
/// in all, and then fails with a retryable error.
pub(crate) async fn resolve_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    identifiers: &[Identifier],
    existing: Option<Uuid>,
    trace: &TraceContext,
) -> Result<Resolution, StoreError> {
    let mut attempt = 0;
    loop {
        let owners = lookup_owners(connection, workspace, identifiers).await?;
        let mut owner_people: Vec<Uuid> = owners.iter().map(|owned| owned.person_id).collect();
        owner_people.sort();
        owner_people.dedup();
        let mut to_lock = owner_people.clone();
        to_lock.extend(existing);
        let locked = lock_people(connection, workspace, &to_lock).await?;
        if locked.iter().any(|person| person.merged_into.is_some()) {
            // A merge committed after the lookup: the identities now name the survivor.
            attempt += 1;
            if attempt >= MAX_ATTEMPTS {
                return Err(StoreError::retryable_race());
            }
            continue;
        }

        let mut merge: Option<(Uuid, Vec<Uuid>)> = None;
        let mut blocked = None;
        if owner_people.len() > 1 {
            let mut union = load_identities(connection, workspace, &owner_people).await?;
            union.extend(identifiers.iter().cloned());
            match merge_allowed(&union) {
                Ok(()) => merge = Some(oldest_first(&locked, &owner_people)),
                Err(reason) => blocked = Some(reason.as_str()),
            }
        }
        let (mut person_id, mut created) = match &merge {
            Some((survivor, _)) => (*survivor, false),
            None => match choose(identifiers, &owners, existing).or(existing) {
                Some(person_id) => (person_id, false),
                None => (create_person(connection, workspace).await?, true),
            },
        };
        let mut missing: Vec<&Identifier> = identifiers
            .iter()
            .enumerate()
            .filter(|(index, _)| owners.iter().all(|owned| owned.identifier_index != *index))
            .map(|(_, identifier)| identifier)
            .collect();
        let mut skipped = 0;
        let mut split_from = None;
        if merge.is_none() && !created {
            // What the record would add must fit the person it attaches to.
            let held = load_identities(connection, workspace, &[person_id]).await?;
            match fit(&held, &missing) {
                Fit::Keep(keep) => {
                    skipped = missing.len() - keep.len();
                    missing = keep;
                }
                Fit::OtherPerson => {
                    // The record is not this person: it starts its own, and what it shares
                    // stays with its owners.
                    split_from = Some(person_id);
                    person_id = create_person(connection, workspace).await?;
                    created = true;
                    blocked = Some(BlockReason::UniqueKind.as_str());
                }
            }
        }
        let conflicts = blocked.map_or_else(Vec::new, |reason| {
            conflicts(identifiers, &owners, person_id, reason)
        });
        let inserted = insert_identities(connection, workspace, person_id, &missing).await?;
        if inserted < missing.len() {
            attempt += 1;
            if attempt >= MAX_ATTEMPTS {
                return Err(StoreError::retryable_race());
            }
            // Another transaction claimed an identifier first. Undo a person made only for
            // this pass, then look again: the winner's identity now resolves.
            if created {
                delete_new_person(connection, workspace, person_id).await?;
            }
            continue;
        }
        let merged = match merge {
            Some((survivor, losers)) => {
                merge_people(connection, workspace, survivor, &losers, trace).await?;
                losers
            }
            None => Vec::new(),
        };
        return Ok(Resolution {
            person_id,
            created,
            conflicts,
            merged,
            skipped,
            split_from,
        });
    }
}

async fn lookup_owners(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    identifiers: &[Identifier],
) -> Result<Vec<Owned>, StoreError> {
    let kinds: Vec<String> = identifiers.iter().map(|i| i.kind().as_text()).collect();
    let values: Vec<&str> = identifiers.iter().map(Identifier::value).collect();
    let rows: Vec<(i64, Uuid, Uuid)> = sqlx::query_as(
        "SELECT t.ord, i.person_id, i.id \
         FROM unnest($2::text[], $3::text[]) WITH ORDINALITY AS t(kind, value, ord) \
         JOIN identities i ON i.workspace_id = $1 AND i.kind = t.kind AND i.value = t.value",
    )
    .bind(workspace.as_uuid())
    .bind(&kinds)
    .bind(&values)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(|(ordinal, person_id, identity_id)| Owned {
            identifier_index: usize::try_from(ordinal - 1).unwrap_or_default(),
            person_id,
            identity_id,
        })
        .collect())
}

/// Locks the people in id order, so two resolutions that share people never wait on each
/// other in a cycle.
async fn lock_people(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    people: &[Uuid],
) -> Result<Vec<LockedPerson>, StoreError> {
    if people.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(Uuid, Option<Uuid>, OffsetDateTime)> = sqlx::query_as(
        "SELECT id, merged_into, created_at FROM people \
         WHERE workspace_id = $1 AND id = ANY($2) ORDER BY id FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(people)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(|(id, merged_into, created_at)| LockedPerson {
            id,
            merged_into,
            created_at,
        })
        .collect())
}

async fn load_identities(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    people: &[Uuid],
) -> Result<Vec<Identifier>, StoreError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT kind, value FROM identities WHERE workspace_id = $1 AND person_id = ANY($2)",
    )
    .bind(workspace.as_uuid())
    .bind(people)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    rows.into_iter()
        .map(|(kind, value)| {
            let kind = IdentifierKind::parse(&kind).map_err(|_| {
                StoreError::query(sqlx::Error::Decode("unknown identity kind".into()))
            })?;
            Ok(Identifier::from_stored(kind, value))
        })
        .collect()
}

enum Fit<'a> {
    /// The identifiers that may be added; any others would exceed a maximum.
    Keep(Vec<&'a Identifier>),
    /// An added identifier would give the person a second value of a kind it holds once.
    OtherPerson,
}

/// Which of `missing` a person holding `held` may take on. Only what is added is judged:
/// what the person already holds is never questioned, even if it breaks a rule.
fn fit<'a>(held: &[Identifier], missing: &[&'a Identifier]) -> Fit<'a> {
    let mut values: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    for identity in held {
        values
            .entry(identity.kind().as_text())
            .or_default()
            .insert(identity.value());
    }
    let mut keep = Vec::new();
    for candidate in missing {
        let kind = candidate.kind().as_text();
        let rule = rule_for(candidate.kind());
        let current = values.entry(kind.clone()).or_default();
        if rule.unique_per_person && current.iter().any(|value| *value != candidate.value()) {
            return Fit::OtherPerson;
        }
        if current.len() >= rule.max_per_person {
            continue;
        }
        current.insert(candidate.value());
        keep.push(*candidate);
    }
    Fit::Keep(keep)
}

/// The survivor of a merge, the oldest of `people` by creation time then id, and the rest.
fn oldest_first(locked: &[LockedPerson], people: &[Uuid]) -> (Uuid, Vec<Uuid>) {
    let mut ordered: Vec<&LockedPerson> = locked
        .iter()
        .filter(|person| people.contains(&person.id))
        .collect();
    ordered.sort_by_key(|person| (person.created_at, person.id));
    let survivor = ordered[0].id;
    let mut losers: Vec<Uuid> = ordered.iter().skip(1).map(|person| person.id).collect();
    losers.sort();
    (survivor, losers)
}

/// The person to attach to: the owner of the strongest known identifier. Among owners of
/// equally strong identifiers the record's current person wins, then the smaller id.
fn choose(identifiers: &[Identifier], owners: &[Owned], existing: Option<Uuid>) -> Option<Uuid> {
    let strongest = owners
        .iter()
        .map(|owned| identifiers[owned.identifier_index].kind().strength())
        .max()?;
    let mut candidates: Vec<Uuid> = owners
        .iter()
        .filter(|owned| identifiers[owned.identifier_index].kind().strength() == strongest)
        .map(|owned| owned.person_id)
        .collect();
    candidates.sort();
    candidates.dedup();
    match existing {
        Some(person) if candidates.contains(&person) => Some(person),
        _ => candidates.first().copied(),
    }
}

fn conflicts(
    identifiers: &[Identifier],
    owners: &[Owned],
    chosen: Uuid,
    reason: &'static str,
) -> Vec<Conflict> {
    let mut found: Vec<Conflict> = Vec::new();
    for owned in owners.iter().filter(|owned| owned.person_id != chosen) {
        if found.iter().all(|c| c.other_person_id != owned.person_id) {
            found.push(Conflict {
                other_person_id: owned.person_id,
                kind: identifiers[owned.identifier_index].kind().as_text(),
                identity_id: owned.identity_id,
                reason,
            });
        }
    }
    found.sort_by_key(|conflict| conflict.other_person_id);
    found
}

async fn create_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Uuid, StoreError> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
        .bind(id)
        .bind(workspace.as_uuid())
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(id)
}

async fn delete_new_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM identities WHERE workspace_id = $1 AND person_id = $2")
        .bind(workspace.as_uuid())
        .bind(person)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    sqlx::query("DELETE FROM people WHERE workspace_id = $1 AND id = $2")
        .bind(workspace.as_uuid())
        .bind(person)
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

/// Inserts the identities nobody owned; returns how many were actually inserted.
async fn insert_identities(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    identifiers: &[&Identifier],
) -> Result<usize, StoreError> {
    if identifiers.is_empty() {
        return Ok(0);
    }
    let ids: Vec<Uuid> = identifiers.iter().map(|_| Uuid::now_v7()).collect();
    let kinds: Vec<String> = identifiers.iter().map(|i| i.kind().as_text()).collect();
    let values: Vec<&str> = identifiers.iter().map(|i| i.value()).collect();
    let result = sqlx::query(
        "INSERT INTO identities (id, workspace_id, person_id, kind, value) \
         SELECT t.id, $2, $3, t.kind, t.value \
         FROM unnest($1::uuid[], $4::text[], $5::text[]) WITH ORDINALITY AS t(id, kind, value, ord) \
         ORDER BY t.ord \
         ON CONFLICT (workspace_id, kind, value) DO NOTHING",
    )
    .bind(&ids)
    .bind(workspace.as_uuid())
    .bind(person)
    .bind(&kinds)
    .bind(&values)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(usize::try_from(result.rows_affected()).unwrap_or(usize::MAX))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    fn identifier(kind: &str, value: &str) -> Identifier {
        Identifier::parse(kind, value).unwrap()
    }

    fn owned(index: usize, person: Uuid) -> Owned {
        Owned {
            identifier_index: index,
            person_id: person,
            identity_id: Uuid::from_u128(99),
        }
    }

    fn trace() -> TraceContext {
        TraceContext::root(Cause::api("test"))
    }

    #[test]
    fn a_second_value_of_a_unique_kind_means_another_person_and_the_excess_of_a_kind_is_skipped() {
        let held = [
            identifier("email", "a@example.com"),
            identifier("external:crm", "1"),
        ];
        let second_crm = identifier("external:crm", "2");
        let second_email = identifier("email", "b@example.com");
        let full: Vec<Identifier> = (0..10)
            .map(|n| identifier("email", &format!("e{n}@example.com")))
            .collect();
        let extra = identifier("email", "extra@example.com");
        let phone = identifier("phone", "+4512345678");

        assert!(matches!(fit(&held, &[&second_crm]), Fit::OtherPerson));
        let doubled = [
            identifier("external:crm", "1"),
            identifier("external:crm", "2"),
        ];
        assert!(matches!(fit(&doubled, &[]), Fit::Keep(keep) if keep.is_empty()));
        assert!(matches!(fit(&doubled, &[&second_email]), Fit::Keep(keep) if keep.len() == 1));
        let other_crm = identifier("external:crm", "3");
        assert!(matches!(
            fit(&[], &[&second_crm, &other_crm]),
            Fit::OtherPerson
        ));
        assert!(matches!(fit(&held, &[&second_email]), Fit::Keep(keep) if keep.len() == 1));
        assert!(matches!(fit(&full, &[&extra, &phone]), Fit::Keep(keep) if keep == [&phone]));
    }

    #[test]
    fn the_strongest_known_identifier_picks_the_person() {
        let (email_owner, phone_owner) = (Uuid::from_u128(2), Uuid::from_u128(1));
        let identifiers = [
            identifier("email", "a@example.com"),
            identifier("phone", "+4512345678"),
        ];
        let owners = [owned(0, email_owner), owned(1, phone_owner)];

        assert_eq!(choose(&identifiers, &owners, None), Some(email_owner));
        assert_eq!(
            conflicts(&identifiers, &owners, email_owner, "unique_kind"),
            [Conflict {
                other_person_id: phone_owner,
                kind: "phone".into(),
                identity_id: Uuid::from_u128(99),
                reason: "unique_kind",
            }]
        );
    }

    #[test]
    fn equal_strength_prefers_the_current_person_then_the_smaller_id() {
        let (low, high) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let identifiers = [
            identifier("email", "a@example.com"),
            identifier("email", "b@example.com"),
        ];
        let owners = [owned(0, high), owned(1, low)];

        assert_eq!(choose(&identifiers, &owners, None), Some(low));
        assert_eq!(choose(&identifiers, &owners, Some(high)), Some(high));
        assert_eq!(choose(&identifiers, &[], Some(high)), None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn unknown_identifiers_create_a_person_and_known_ones_attach(pool: PgPool) {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let mut connection = pool.acquire().await.unwrap();
        let email = identifier("email", "a@example.com");
        let phone = identifier("phone", "+4512345678");

        let first = resolve_person(
            &mut connection,
            ws,
            std::slice::from_ref(&email),
            None,
            &trace(),
        )
        .await
        .unwrap();
        let second = resolve_person(&mut connection, ws, &[email, phone], None, &trace())
            .await
            .unwrap();

        assert!(first.created && !second.created);
        assert_eq!(first.person_id, second.person_id);
        assert!(second.conflicts.is_empty());
        let identities: i64 = sqlx::query_scalar("SELECT count(*) FROM identities")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(identities, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_truly_concurrent_identity_race_resolves_to_one_person(pool: PgPool) {
        use std::sync::Arc;
        use std::time::Duration;

        use tokio::sync::Barrier;

        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let email = identifier("email", "race@example.com");
        let barrier = Arc::new(Barrier::new(2));

        // The first transaction inserts the identity and holds it uncommitted until the
        // second one is blocked waiting on that row.
        let first = {
            let (pool, barrier, email) = (pool.clone(), barrier.clone(), email.clone());
            tokio::spawn(async move {
                let mut tx = pool.begin().await.unwrap();
                let resolution = resolve_person(&mut tx, ws, &[email], None, &trace())
                    .await
                    .unwrap();
                barrier.wait().await;
                let mut blocked = false;
                for _ in 0..400 {
                    let waiting: i64 = sqlx::query_scalar(
                        "SELECT count(*) FROM pg_stat_activity \
                         WHERE datname = current_database() AND wait_event_type = 'Lock'",
                    )
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                    if waiting > 0 {
                        blocked = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                assert!(
                    blocked,
                    "the second transaction never blocked on the identity"
                );
                tx.commit().await.unwrap();
                resolution
            })
        };
        let second = {
            let (pool, barrier, email) = (pool.clone(), barrier.clone(), email.clone());
            tokio::spawn(async move {
                barrier.wait().await;
                let mut tx = pool.begin().await.unwrap();
                let resolution = resolve_person(&mut tx, ws, &[email], None, &trace())
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                resolution
            })
        };

        let (first, second) = (first.await.unwrap(), second.await.unwrap());

        assert!(first.created);
        assert!(!second.created);
        assert_eq!(first.person_id, second.person_id);
        let identities: i64 = sqlx::query_scalar("SELECT count(*) FROM identities")
            .fetch_one(&pool)
            .await
            .unwrap();
        let people: i64 = sqlx::query_scalar("SELECT count(*) FROM people")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((identities, people), (1, 1));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_record_with_no_known_identifier_stays_with_its_existing_person(pool: PgPool) {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let mut connection = pool.acquire().await.unwrap();
        let first = resolve_person(
            &mut connection,
            ws,
            &[identifier("email", "old@example.com")],
            None,
            &trace(),
        )
        .await
        .unwrap();

        let again = resolve_person(
            &mut connection,
            ws,
            &[identifier("email", "new@example.com")],
            Some(first.person_id),
            &trace(),
        )
        .await
        .unwrap();

        assert!(!again.created);
        assert_eq!(again.person_id, first.person_id);
    }
}
