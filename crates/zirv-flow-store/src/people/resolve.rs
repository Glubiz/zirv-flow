//! Identity resolution without merges (architecture section 7.3).
//!
//! Given the identifiers of one record, find the person it belongs to, creating one when no
//! identifier is known. When identifiers belong to different people the record attaches to
//! the owner of the strongest identifier and the others are reported as conflicts, which a
//! later merge policy will replace.

use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::people::Identifier;

use crate::StoreError;

/// An identifier that points at a person other than the one the record attached to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Conflict {
    pub other_person_id: Uuid,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Resolution {
    pub person_id: Uuid,
    pub created: bool,
    pub conflicts: Vec<Conflict>,
}

struct Owned {
    identifier_index: usize,
    person_id: Uuid,
}

/// Resolves the person for `identifiers` (normalized, sorted, non-empty). `existing` is the
/// person the record is already attached to, which wins ties and receives a record whose
/// identifiers are all unknown. An identity insert that loses a unique race re-runs the
/// resolution once.
pub(crate) async fn resolve_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    identifiers: &[Identifier],
    existing: Option<Uuid>,
) -> Result<Resolution, StoreError> {
    let mut attempt = 0;
    loop {
        let owners = lookup_owners(connection, workspace, identifiers).await?;
        let (person_id, created) = match choose(identifiers, &owners, existing) {
            Some(person_id) => (person_id, false),
            None => (create_person(connection, workspace).await?, true),
        };
        let conflicts = conflicts(identifiers, &owners, person_id);
        let missing: Vec<&Identifier> = identifiers
            .iter()
            .enumerate()
            .filter(|(index, _)| owners.iter().all(|owned| owned.identifier_index != *index))
            .map(|(_, identifier)| identifier)
            .collect();
        let inserted = insert_identities(connection, workspace, person_id, &missing).await?;
        if inserted < missing.len() && attempt == 0 {
            // Another transaction claimed an identifier first. Undo a person made only for
            // this pass, then look again: the winner's identity now resolves.
            if created {
                delete_new_person(connection, workspace, person_id).await?;
            }
            attempt += 1;
            continue;
        }
        return Ok(Resolution {
            person_id,
            created,
            conflicts,
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
    let rows: Vec<(i64, Uuid)> = sqlx::query_as(
        "SELECT t.ord, i.person_id \
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
        .map(|(ordinal, person_id)| Owned {
            identifier_index: usize::try_from(ordinal - 1).unwrap_or_default(),
            person_id,
        })
        .collect())
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

fn conflicts(identifiers: &[Identifier], owners: &[Owned], chosen: Uuid) -> Vec<Conflict> {
    let mut found: Vec<Conflict> = Vec::new();
    for owned in owners.iter().filter(|owned| owned.person_id != chosen) {
        if found.iter().all(|c| c.other_person_id != owned.person_id) {
            found.push(Conflict {
                other_person_id: owned.person_id,
                kind: identifiers[owned.identifier_index].kind().as_text(),
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
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    fn identifier(kind: &str, value: &str) -> Identifier {
        Identifier::parse(kind, value).unwrap()
    }

    fn owned(index: usize, person: Uuid) -> Owned {
        Owned {
            identifier_index: index,
            person_id: person,
        }
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
            conflicts(&identifiers, &owners, email_owner),
            [Conflict {
                other_person_id: phone_owner,
                kind: "phone".into()
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

        let first = resolve_person(&mut connection, ws, std::slice::from_ref(&email), None)
            .await
            .unwrap();
        let second = resolve_person(&mut connection, ws, &[email, phone], None)
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
}
