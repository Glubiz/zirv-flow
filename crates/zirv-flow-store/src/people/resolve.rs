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
use zirv_flow_core::segment::ChangeClasses;
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::batch::{Held, ProfileBatch};
use super::merge::merge_people;
use crate::StoreError;
use crate::changes::mark_person;

/// How many times a record's resolution runs when identity inserts keep losing races.
const MAX_ATTEMPTS: u32 = 3;

/// The longest chain of merges a lookup follows.
pub(super) const MAX_MERGE_HOPS: u32 = 8;

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
/// in all, and then fails with a retryable error. A person deleted while it was being locked
/// fails with a retryable error at once, for the caller to run the batch again.
///
/// The people `batch` holds already are neither locked again nor looked up a second time: an
/// identity of a person this transaction holds cannot move or go under it, since every writer
/// that moves or deletes one locks its person first.
pub(crate) async fn resolve_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    identifiers: &[Identifier],
    existing: Option<Uuid>,
    trace: &TraceContext,
    batch: &mut ProfileBatch,
) -> Result<Resolution, StoreError> {
    // After the segment guard, take the slot lock before a merge or a new person writes traits.
    batch.guard(connection, workspace).await?;
    let mut attempt = 0;
    loop {
        let owners = lookup_owners(connection, workspace, identifiers).await?;
        let mut owner_people: Vec<Uuid> = owners.iter().map(|owned| owned.person_id).collect();
        owner_people.sort();
        owner_people.dedup();
        let mut to_lock = owner_people.clone();
        to_lock.extend(existing);
        to_lock.sort();
        to_lock.dedup();
        // Every one held since before the lookup: what it found cannot have changed.
        let held: Option<Vec<LockedPerson>> = to_lock
            .iter()
            .map(|id| {
                batch.held(*id).map(|held| LockedPerson {
                    id: *id,
                    merged_into: held.merged_into,
                    created_at: held.created_at,
                })
            })
            .collect();
        let settled = held.is_some();
        let locked = match held {
            Some(locked) => locked,
            None => {
                let locked = lock_people(connection, workspace, &to_lock).await?;
                for person in &locked {
                    batch.hold(
                        person.id,
                        Held {
                            merged_into: person.merged_into,
                            created_at: person.created_at,
                        },
                    );
                }
                locked
            }
        };
        if locked.len() < to_lock.len() {
            // A person was deleted (erased) while this waited for the lock. Retrying the
            // batch looks the identifiers up again and resolves into a new person.
            return Err(StoreError::retryable_race());
        }
        // A merge committed after the lookup (the identities now name the survivor), or an
        // identity was deleted or moved while the people were being locked: look again.
        if locked.iter().any(|person| person.merged_into.is_some())
            || (!settled
                && !owners.is_empty()
                && !same_owners(&owners, connection, workspace, identifiers).await?)
        {
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
                // A new person's row is inserted once its identities are claimed.
                None => (Uuid::now_v7(), true),
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
        // With nothing to add, `fit` keeps nothing and judges nothing, so the read is skipped.
        if merge.is_none() && !created && !missing.is_empty() {
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
                    person_id = Uuid::now_v7();
                    created = true;
                    blocked = Some(BlockReason::UniqueKind.as_str());
                }
            }
        }
        let conflicts = blocked.map_or_else(Vec::new, |reason| {
            conflicts(identifiers, &owners, person_id, reason)
        });
        // A new person's identities are claimed before its row exists, their foreign key checked
        // once the row is in (or they are gone): a pass that loses one never made a person to
        // delete, a delete that checks every table referencing `people`.
        let claiming = created && !missing.is_empty();
        if claiming {
            defer_identity_owners(connection, true).await?;
        }
        let inserted = insert_identities(connection, workspace, person_id, &missing).await?;
        let claimed = inserted == missing.len();
        if created && claimed {
            create_person(connection, workspace, person_id, batch).await?;
        } else if created && inserted > 0 {
            release_identities(connection, workspace, person_id).await?;
        }
        if claiming {
            defer_identity_owners(connection, false).await?;
        }
        if !claimed {
            // Another transaction claimed an identifier first: look again, the winner's identity
            // now resolves.
            attempt += 1;
            if attempt >= MAX_ATTEMPTS {
                return Err(StoreError::retryable_race());
            }
            continue;
        }
        if inserted > 0 {
            mark_person(
                connection,
                workspace,
                person_id,
                ChangeClasses::IDENTIFIERS,
                trace,
                None,
            )
            .await?;
        }
        let merged = match merge {
            Some((survivor, losers)) => {
                merge_people(
                    connection,
                    workspace,
                    survivor,
                    &losers,
                    trace,
                    &mut batch.definitions,
                )
                .await?;
                batch.merged(survivor, &losers);
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

/// The person `identifiers` already belong to, chosen like [`resolve_person`] chooses and
/// followed through `merged_into`; nothing is created or attached.
pub(crate) async fn find_known_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    identifiers: &[Identifier],
) -> Result<Option<Uuid>, StoreError> {
    let owners = lookup_owners(connection, workspace, identifiers).await?;
    let Some(mut person) = choose(identifiers, &owners, None) else {
        return Ok(None);
    };
    for _ in 0..MAX_MERGE_HOPS {
        let next: Option<Option<Uuid>> = sqlx::query_scalar(
            "SELECT merged_into FROM people WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace.as_uuid())
        .bind(person)
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        match next.flatten() {
            Some(target) => person = target,
            None => break,
        }
    }
    Ok(Some(person))
}

/// Whether a lookup under the people's locks finds the same identities with the same owners
/// as the lookup made before them.
async fn same_owners(
    before: &[Owned],
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    identifiers: &[Identifier],
) -> Result<bool, StoreError> {
    let key = |owned: &Owned| (owned.identifier_index, owned.person_id, owned.identity_id);
    let mut then: Vec<_> = before.iter().map(key).collect();
    let mut now: Vec<_> = lookup_owners(connection, workspace, identifiers)
        .await?
        .iter()
        .map(key)
        .collect();
    then.sort();
    now.sort();
    Ok(then == now)
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

/// Inserts the new person `id`, whose identities this transaction has claimed, and holds it: no
/// other transaction can see it before the commit.
async fn create_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    batch: &mut ProfileBatch,
) -> Result<(), StoreError> {
    let created_at: OffsetDateTime = sqlx::query_scalar(
        "INSERT INTO people (id, workspace_id) VALUES ($1, $2) RETURNING created_at",
    )
    .bind(id)
    .bind(workspace.as_uuid())
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    batch.hold(
        id,
        Held {
            merged_into: None,
            created_at,
        },
    );
    Ok(())
}

/// Deletes the identities a pass claimed for a person it did not get to create.
async fn release_identities(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM identities WHERE workspace_id = $1 AND person_id = $2")
        .bind(workspace.as_uuid())
        .bind(person)
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

/// Defers this transaction's check of the identities' foreign key to their person (deferrable
/// since migration 0092), or, with `deferred` false, checks at each statement again and checks
/// now what was deferred. Every other writer keeps the immediate check, whose key-share lock on
/// the person an erasure waits for.
async fn defer_identity_owners(
    connection: &mut PgConnection,
    deferred: bool,
) -> Result<(), StoreError> {
    let statement = if deferred {
        "SET CONSTRAINTS identities_person_id_fkey DEFERRED"
    } else {
        "SET CONSTRAINTS identities_person_id_fkey IMMEDIATE"
    };
    sqlx::query(statement)
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
        let mut connection = crate::begin(&pool).await.unwrap();
        let email = identifier("email", "a@example.com");
        let phone = identifier("phone", "+4512345678");

        let first = resolve_person(
            &mut connection,
            ws,
            std::slice::from_ref(&email),
            None,
            &trace(),
            &mut ProfileBatch::default(),
        )
        .await
        .unwrap();
        let second = resolve_person(
            &mut connection,
            ws,
            &[email, phone],
            None,
            &trace(),
            &mut ProfileBatch::default(),
        )
        .await
        .unwrap();
        crate::commit(connection).await.unwrap();

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
                let resolution = resolve_person(
                    &mut tx,
                    ws,
                    &[email],
                    None,
                    &trace(),
                    &mut ProfileBatch::default(),
                )
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
                let resolution = resolve_person(
                    &mut tx,
                    ws,
                    &[email],
                    None,
                    &trace(),
                    &mut ProfileBatch::default(),
                )
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

    /// The second record shares the email the first is claiming and brings a phone: it claims
    /// the phone for a person of its own, loses the email, gives the phone back and resolves to
    /// the first person, without ever inserting or deleting a `people` row.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_record_that_loses_an_identity_race_makes_no_person_to_delete(pool: PgPool) {
        use std::time::Duration;

        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let email = identifier("email", "race@example.com");
        let phone = identifier("phone", "+4520000000");

        let mut first = pool.begin().await.unwrap();
        let winner = resolve_person(
            &mut first,
            ws,
            std::slice::from_ref(&email),
            None,
            &trace(),
            &mut ProfileBatch::default(),
        )
        .await
        .unwrap();
        let second = {
            let pool = pool.clone();
            tokio::spawn(async move {
                let mut tx = pool.begin().await.unwrap();
                let resolution = resolve_person(
                    &mut tx,
                    ws,
                    &[email, phone],
                    None,
                    &trace(),
                    &mut ProfileBatch::default(),
                )
                .await
                .unwrap();
                // What this transaction wrote, by table: inserted and deleted rows.
                let written: Vec<(String, i64, i64)> = sqlx::query_as(
                    "SELECT relname::text, n_tup_ins, n_tup_del FROM pg_stat_xact_user_tables \
                     WHERE relname IN ('people', 'identities') ORDER BY relname",
                )
                .fetch_all(&mut *tx)
                .await
                .unwrap();
                tx.commit().await.unwrap();
                (resolution, written)
            })
        };
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
        assert!(blocked, "the second record never waited for the email");
        first.commit().await.unwrap();
        let (resolution, written) = second.await.unwrap();

        assert!(winner.created);
        assert!(!resolution.created);
        assert_eq!(resolution.person_id, winner.person_id);
        // The phone twice (claimed, given back, attached to the winner), and no person at all.
        assert_eq!(
            written,
            [("identities".to_owned(), 2, 1), ("people".to_owned(), 0, 0)]
        );
        let owners: Vec<(String, Uuid)> =
            sqlx::query_as("SELECT kind, person_id FROM identities ORDER BY kind")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            owners,
            [
                ("email".to_owned(), winner.person_id),
                ("phone".to_owned(), winner.person_id)
            ]
        );
        let people: i64 = sqlx::query_scalar("SELECT count(*) FROM people")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(people, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_record_with_no_known_identifier_stays_with_its_existing_person(pool: PgPool) {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let mut connection = crate::begin(&pool).await.unwrap();
        let first = resolve_person(
            &mut connection,
            ws,
            &[identifier("email", "old@example.com")],
            None,
            &trace(),
            &mut ProfileBatch::default(),
        )
        .await
        .unwrap();

        let again = resolve_person(
            &mut connection,
            ws,
            &[identifier("email", "new@example.com")],
            Some(first.person_id),
            &trace(),
            &mut ProfileBatch::default(),
        )
        .await
        .unwrap();

        assert!(!again.created);
        assert_eq!(again.person_id, first.person_id);
    }
}
