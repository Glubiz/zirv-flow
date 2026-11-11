//! Resolving the people of a run of events together, for the drain's batched event path
//! (architecture 7.1 and 7.3).
//!
//! [`Planned::load`] looks up every identifier of the run in one statement, locks every owner the
//! transaction does not hold yet in one more, in id order, and looks the identifiers up again
//! under those locks, as [`resolve_person`] does for one record; it reads the identities of the
//! owners a record would add to in one statement. [`Planned::resolve`] then resolves the records
//! one after another in memory, each seeing the people and identities the records before it made,
//! and [`Planned::write`] inserts the new people and then every claimed identity, each in one
//! statement.
//!
//! A record it cannot resolve the same way as [`resolve_person`] without a statement of its own
//! (identifiers of two people, a merge or a conflict; a second value of a kind its person holds
//! once, a split; an owner the transaction does not hold, or one merged away) is left to
//! [`resolve_person`]: [`Planned::resolve`] returns `None` and changes nothing.
//!
//! A claimed identity another transaction inserted first fails the batch as a retryable race, as
//! [`resolve_person`] does once its retries run out: the rollback takes the plan's people with it,
//! and the retry finds the winner's identity.
//!
//! [`resolve_person`]: super::resolve_person

use hashbrown::{HashMap, HashSet};
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::people::{Identifier, IdentifierKind};
use zirv_flow_core::segment::ChangeClasses;
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::{Fit, MAX_ATTEMPTS, Resolution, fit};
use crate::StoreError;
use crate::changes::MarkBuffer;
use crate::people::batch::{Held, ProfileBatch};
use crate::people::lock::{LockedRow, UPDATE_WITH_CREATED};

/// An identifier as `(kind, value)` text.
type Key = (String, String);

fn key(identifier: &Identifier) -> Key {
    (identifier.kind().as_text(), identifier.value().to_owned())
}

/// An identity a record of the plan claimed.
struct Claim {
    id: Uuid,
    person: Uuid,
    kind: String,
    value: String,
}

/// See the module documentation.
#[derive(Default)]
pub(crate) struct Planned {
    /// Every identifier of the run that has an owner, with the owner and the identity: as the
    /// lookup found them, and as the records resolved so far claimed them.
    owners: HashMap<Key, (Uuid, Uuid)>,
    /// What the people a record may add identifiers to hold: as read, plus what the plan added.
    identities: HashMap<Uuid, Vec<Identifier>>,
    /// The people the plan makes, in the order made.
    made: Vec<Uuid>,
    making: HashSet<Uuid>,
    claims: Vec<Claim>,
}

impl Planned {
    /// Looks up, locks and verifies the owners of every identifier in `sets` (each a record's
    /// normalized identifiers), as [`super::resolve_person`] does for one record, and reads the
    /// identities of each owner a record would add an identifier to. The people stay locked, and
    /// `batch` holds them, until the transaction ends.
    pub(crate) async fn load(
        connection: &mut PgConnection,
        workspace: WorkspaceId,
        sets: &[&[Identifier]],
        batch: &mut ProfileBatch,
    ) -> Result<Self, StoreError> {
        let mut pairs: Vec<Key> = sets.iter().flat_map(|set| set.iter().map(key)).collect();
        pairs.sort_unstable();
        pairs.dedup();
        if pairs.is_empty() {
            return Ok(Self::default());
        }
        // After the segment guard, take the slot lock before a new person writes traits.
        batch.guard(connection, workspace).await?;
        let mut attempt = 0;
        let owners = loop {
            let owners = lookup(connection, workspace, &pairs).await?;
            let mut fresh: Vec<Uuid> = owners
                .values()
                .map(|(person, _)| *person)
                .filter(|person| batch.held(*person).is_none())
                .collect();
            fresh.sort_unstable();
            fresh.dedup();
            // Every owner held since before the lookup: what it found cannot have changed.
            if fresh.is_empty() {
                break owners;
            }
            let locked: Vec<LockedRow> = sqlx::query_as(UPDATE_WITH_CREATED)
                .bind(workspace.as_uuid())
                .bind(&fresh)
                .fetch_all(&mut *connection)
                .await
                .map_err(StoreError::query)?;
            if locked.len() < fresh.len() {
                // A person was deleted (erased) while this waited for the lock.
                return Err(StoreError::retryable_race());
            }
            batch.hold_rows(locked);
            // An identity moved (a merge) or went while the people were being locked: look again.
            if lookup(connection, workspace, &pairs).await? == owners {
                break owners;
            }
            attempt += 1;
            if attempt >= MAX_ATTEMPTS {
                return Err(StoreError::retryable_race());
            }
        };
        // The one owner of each record that also names an identifier nobody owns: the record may
        // add it, and what it adds must fit what the owner holds.
        let mut adding: Vec<Uuid> = sets
            .iter()
            .filter_map(|set| {
                let mut found = set
                    .iter()
                    .filter_map(|identifier| owners.get(&key(identifier)));
                let (person, _) = *found.next()?;
                let one = found.all(|(other, _)| *other == person);
                let missing = set
                    .iter()
                    .any(|identifier| !owners.contains_key(&key(identifier)));
                (one && missing).then_some(person)
            })
            .collect();
        adding.sort_unstable();
        adding.dedup();
        let identities = held_identities(connection, workspace, &adding).await?;
        Ok(Self {
            owners,
            identities,
            ..Self::default()
        })
    }

    /// Resolves one record's `identifiers` (normalized, sorted, non-empty) after the records
    /// resolved before it, as [`super::resolve_person`] would with no person the record is already
    /// attached to: the one owner of its identifiers, else a new person; what it would add must
    /// fit the person. A mark in the `identifiers` class goes to `marks` when it adds any. `None`
    /// when the record needs [`super::resolve_person`]; nothing is changed then.
    pub(crate) fn resolve(
        &mut self,
        identifiers: &[Identifier],
        batch: &ProfileBatch,
        trace: &TraceContext,
        marks: &mut MarkBuffer,
    ) -> Option<Resolution> {
        let keys: Vec<Key> = identifiers.iter().map(key).collect();
        let mut owner = None;
        let mut missing: Vec<&Identifier> = Vec::new();
        for (identifier, key) in identifiers.iter().zip(&keys) {
            match (self.owners.get(key), owner) {
                (Some((person, _)), None) => owner = Some(*person),
                (Some((person, _)), Some(chosen)) if *person != chosen => return None,
                (Some(_), Some(_)) => {}
                (None, _) => missing.push(identifier),
            }
        }
        let (person_id, created) = match owner {
            Some(person) => {
                let held = self.making.contains(&person)
                    || batch
                        .held(person)
                        .is_some_and(|held| held.merged_into.is_none());
                if !held {
                    return None;
                }
                (person, false)
            }
            None => (Uuid::now_v7(), true),
        };
        let mut skipped = 0;
        if !created && !missing.is_empty() {
            match fit(self.identities.get(&person_id)?, &missing) {
                Fit::Keep(keep) => {
                    skipped = missing.len() - keep.len();
                    missing = keep;
                }
                Fit::OtherPerson => return None,
            }
        }
        if created {
            self.made.push(person_id);
            self.making.insert(person_id);
        }
        if !missing.is_empty() {
            // A new person holds what it claims; an owner's identities were read, or `fit` above
            // returned early.
            let held = self.identities.entry(person_id).or_default();
            for identifier in &missing {
                let id = Uuid::now_v7();
                let (kind, value) = key(identifier);
                self.owners
                    .insert((kind.clone(), value.clone()), (person_id, id));
                held.push((*identifier).clone());
                self.claims.push(Claim {
                    id,
                    person: person_id,
                    kind,
                    value,
                });
            }
            marks.push(person_id, ChangeClasses::IDENTIFIERS, trace, None);
        }
        Some(Resolution {
            person_id,
            created,
            conflicts: Vec::new(),
            merged: Vec::new(),
            skipped,
            split_from: None,
        })
    }

    /// Inserts the people the plan made, then every identity it claimed, sorted by kind and value
    /// so two batches claiming the same identities take them in one order. `batch` holds the new
    /// people from then on. An identity another transaction claimed first fails as a retryable
    /// race. The plan is spent.
    pub(crate) async fn write(
        self,
        connection: &mut PgConnection,
        workspace: WorkspaceId,
        batch: &mut ProfileBatch,
    ) -> Result<(), StoreError> {
        let Self {
            made, mut claims, ..
        } = self;
        if !made.is_empty() {
            let rows: Vec<(Uuid, OffsetDateTime)> = sqlx::query_as(
                "INSERT INTO people (id, workspace_id) \
                 SELECT p.id, $1 FROM unnest($2::uuid[]) AS p(id) ORDER BY p.id \
                 RETURNING id, created_at",
            )
            .bind(workspace.as_uuid())
            .bind(&made)
            .fetch_all(&mut *connection)
            .await
            .map_err(StoreError::query)?;
            for (id, created_at) in rows {
                batch.hold(
                    id,
                    Held {
                        merged_into: None,
                        created_at,
                    },
                );
            }
        }
        if claims.is_empty() {
            return Ok(());
        }
        claims.sort_by(|a, b| (&a.kind, &a.value).cmp(&(&b.kind, &b.value)));
        let ids: Vec<Uuid> = claims.iter().map(|claim| claim.id).collect();
        let people: Vec<Uuid> = claims.iter().map(|claim| claim.person).collect();
        let kinds: Vec<&str> = claims.iter().map(|claim| claim.kind.as_str()).collect();
        let values: Vec<&str> = claims.iter().map(|claim| claim.value.as_str()).collect();
        let inserted = sqlx::query(
            "INSERT INTO identities (id, workspace_id, person_id, kind, value) \
             SELECT t.id, $1, t.person_id, t.kind, t.value \
             FROM unnest($2::uuid[], $3::uuid[], $4::text[], $5::text[]) \
                 AS t(id, person_id, kind, value) \
             ORDER BY t.kind, t.value \
             ON CONFLICT (workspace_id, kind, value) DO NOTHING",
        )
        .bind(workspace.as_uuid())
        .bind(&ids)
        .bind(&people)
        .bind(&kinds)
        .bind(&values)
        .execute(connection)
        .await
        .map_err(StoreError::query)?
        .rows_affected();
        if usize::try_from(inserted).unwrap_or(usize::MAX) < claims.len() {
            return Err(StoreError::retryable_race());
        }
        Ok(())
    }
}

/// The owner and the identity of each of `pairs` that has one.
async fn lookup(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    pairs: &[Key],
) -> Result<HashMap<Key, (Uuid, Uuid)>, StoreError> {
    let kinds: Vec<&str> = pairs.iter().map(|(kind, _)| kind.as_str()).collect();
    let values: Vec<&str> = pairs.iter().map(|(_, value)| value.as_str()).collect();
    let rows: Vec<(String, String, Uuid, Uuid)> = sqlx::query_as(
        "SELECT t.kind, t.value, i.person_id, i.id \
         FROM unnest($2::text[], $3::text[]) AS t(kind, value) \
         JOIN identities i ON i.workspace_id = $1 AND i.kind = t.kind AND i.value = t.value",
    )
    .bind(workspace.as_uuid())
    .bind(&kinds)
    .bind(&values)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(|(kind, value, person, identity)| ((kind, value), (person, identity)))
        .collect())
}

/// The identities each of `people` holds.
async fn held_identities(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    people: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Identifier>>, StoreError> {
    let mut held: HashMap<Uuid, Vec<Identifier>> = HashMap::new();
    if people.is_empty() {
        return Ok(held);
    }
    let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT person_id, kind, value FROM identities \
         WHERE workspace_id = $1 AND person_id = ANY($2)",
    )
    .bind(workspace.as_uuid())
    .bind(people)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    for person in people {
        held.insert(*person, Vec::new());
    }
    for (person, kind, value) in rows {
        let kind = IdentifierKind::parse(&kind)
            .map_err(|_| StoreError::query(sqlx::Error::Decode("unknown identity kind".into())))?;
        held.entry(person)
            .or_default()
            .push(Identifier::from_stored(kind, value));
    }
    Ok(held)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::people::{normalize_identifiers, resolve_person};
    use crate::{begin, commit, create_workspace};

    fn identifiers(pairs: &[(&str, &str)]) -> Vec<Identifier> {
        let raw: Vec<(String, String)> = pairs
            .iter()
            .map(|(kind, value)| ((*kind).to_owned(), (*value).to_owned()))
            .collect();
        normalize_identifiers(&raw).unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_plan_resolves_a_run_as_its_records_one_after_another(pool: PgPool) {
        let name = WorkspaceName::parse("plan").unwrap();
        let slug = Slug::parse("plan").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let trace = TraceContext::root(Cause::api("test"));
        let known = identifiers(&[("email", "ada@example.com")]);
        let other = identifiers(&[("phone", "+4512345678")]);
        let mut tx = begin(&pool).await.unwrap();
        let mut batch = ProfileBatch::default();
        let ada = resolve_person(&mut tx, ws, &known, None, &trace, &mut batch)
            .await
            .unwrap()
            .person_id;
        let bob = resolve_person(&mut tx, ws, &other, None, &trace, &mut batch)
            .await
            .unwrap()
            .person_id;
        commit(tx).await.unwrap();

        let new = identifiers(&[("anonymous_id", "a1")]);
        let grown = identifiers(&[("anonymous_id", "a1"), ("email", "new@example.com")]);
        let both = identifiers(&[("email", "ada@example.com"), ("phone", "+4512345678")]);
        let sets: Vec<&[Identifier]> = vec![&known, &new, &grown, &both];
        let mut tx = begin(&pool).await.unwrap();
        let mut batch = ProfileBatch::default();
        let mut marks = MarkBuffer::default();
        let mut plan = Planned::load(&mut tx, ws, &sets, &mut batch).await.unwrap();

        let first = plan.resolve(&known, &batch, &trace, &mut marks).unwrap();
        assert_eq!((first.person_id, first.created), (ada, false));
        let made = plan.resolve(&new, &batch, &trace, &mut marks).unwrap();
        assert!(made.created);
        // The person the run made a record ago, gaining an identifier.
        let again = plan.resolve(&grown, &batch, &trace, &mut marks).unwrap();
        assert_eq!((again.person_id, again.created), (made.person_id, false));
        // Two people's identifiers need a merge: left to `resolve_person`, and nothing changes.
        assert!(plan.resolve(&both, &batch, &trace, &mut marks).is_none());
        plan.write(&mut tx, ws, &mut batch).await.unwrap();
        commit(tx).await.unwrap();

        let held: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT person_id, kind || ':' || value FROM identities \
             WHERE workspace_id = $1 ORDER BY kind, value",
        )
        .bind(ws.as_uuid())
        .fetch_all(&pool)
        .await
        .unwrap();
        let person = made.person_id;
        assert_eq!(
            held,
            [
                (person, "anonymous_id:a1".to_owned()),
                (ada, "email:ada@example.com".to_owned()),
                (person, "email:new@example.com".to_owned()),
                (bob, "phone:+4512345678".to_owned()),
            ]
        );
        let people: i64 = sqlx::query_scalar("SELECT count(*) FROM people WHERE workspace_id = $1")
            .bind(ws.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(people, 3);
    }
}
