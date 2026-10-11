//! Reads of people, their identities and their identity conflicts. Every read is scoped to
//! one workspace.

use std::fmt;

use serde_json::Value;
use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::people::Identifier;

use crate::StoreError;

/// A person with the traits materialized from the ledger.
#[derive(Clone, FromRow, PartialEq)]
pub struct Person {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub traits: Value,
    pub trait_sources: Value,
    pub merged_into: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

// Trait values are personal data: Debug shows ids only.
impl fmt::Debug for Person {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Person")
            .field("id", &self.id)
            .field("workspace_id", &self.workspace_id)
            .finish_non_exhaustive()
    }
}

/// One identifier of a person, as stored.
#[derive(Clone, FromRow, PartialEq)]
pub struct StoredIdentity {
    pub id: Uuid,
    pub person_id: Uuid,
    pub kind: String,
    pub value: String,
}

// Identifier values are personal data: Debug shows the kind only.
impl fmt::Debug for StoredIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredIdentity")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// A recorded identifier collision between two people.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct IdentityConflict {
    pub id: Uuid,
    pub person_id: Uuid,
    pub other_person_id: Uuid,
    pub kind: String,
    /// Null for a conflict found while storing an event.
    pub source_record_id: Option<Uuid>,
    pub reason: String,
    /// The shared identity; absent on conflicts recorded before merges existed.
    pub identity_id: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub resolved_at: Option<OffsetDateTime>,
    /// `merged`, `kept_separate` or `identifier_moved`; set with `resolved_at`.
    pub resolution: Option<String>,
}

/// The columns of [`IdentityConflict`].
macro_rules! conflict_columns {
    () => {
        "id, person_id, other_person_id, kind, source_record_id, reason, identity_id, \
         created_at, resolved_at, resolution"
    };
}
pub(super) use conflict_columns;

/// SQL for the person that stands for `$2`: its survivor when it was merged.
macro_rules! current_person_sql {
    () => {
        "COALESCE((SELECT merged_into FROM people WHERE workspace_id = $1 AND id = $2), $2)"
    };
}

/// The person, or the survivor it was merged into.
pub async fn find_person<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Option<Person>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT id, workspace_id, traits, trait_sources, merged_into, created_at, updated_at \
         FROM people WHERE workspace_id = $1 AND id = ",
        current_person_sql!()
    ))
    .bind(workspace.as_uuid())
    .bind(person)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// The person that owns an identifier, if any.
pub async fn find_person_by_identifier<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    identifier: &Identifier,
) -> Result<Option<Person>, StoreError> {
    sqlx::query_as(
        "SELECT p.id, p.workspace_id, p.traits, p.trait_sources, p.merged_into, p.created_at, \
                p.updated_at \
         FROM identities i JOIN people p ON p.workspace_id = i.workspace_id AND p.id = i.person_id \
         WHERE i.workspace_id = $1 AND i.kind = $2 AND i.value = $3",
    )
    .bind(workspace.as_uuid())
    .bind(identifier.kind().as_text())
    .bind(identifier.value())
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// The people that own `identifiers`, aligned with the input: `None` where no person does.
/// Never creates a person.
pub async fn find_people_by_identifiers<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    identifiers: &[Identifier],
) -> Result<Vec<Option<Uuid>>, StoreError> {
    let kinds: Vec<String> = identifiers
        .iter()
        .map(|identifier| identifier.kind().as_text())
        .collect();
    let values: Vec<&str> = identifiers.iter().map(Identifier::value).collect();
    let owners: Vec<(String, String, Uuid)> = sqlx::query_as(
        "SELECT i.kind, i.value, i.person_id FROM identities i          JOIN unnest($2::text[], $3::text[]) AS wanted(kind, value)            ON i.kind = wanted.kind AND i.value = wanted.value          WHERE i.workspace_id = $1",
    )
    .bind(workspace.as_uuid())
    .bind(&kinds)
    .bind(&values)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    let by_identity: std::collections::HashMap<(&str, &str), Uuid> = owners
        .iter()
        .map(|(kind, value, person)| ((kind.as_str(), value.as_str()), *person))
        .collect();
    Ok(kinds
        .iter()
        .zip(&values)
        .map(|(kind, value)| by_identity.get(&(kind.as_str(), *value)).copied())
        .collect())
}

/// The identities of a person (of its survivor when it was merged), ordered by kind then
/// value.
pub async fn list_identities<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Vec<StoredIdentity>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT id, person_id, kind, value FROM identities \
         WHERE workspace_id = $1 AND person_id = ",
        current_person_sql!(),
        " ORDER BY kind, value"
    ))
    .bind(workspace.as_uuid())
    .bind(person)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// The conflicts recorded against a person (or its survivor), oldest first.
pub async fn list_identity_conflicts<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Vec<IdentityConflict>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        conflict_columns!(),
        " FROM identity_conflicts WHERE workspace_id = $1 AND person_id = ",
        current_person_sql!(),
        " ORDER BY created_at, id"
    ))
    .bind(workspace.as_uuid())
    .bind(person)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_missing_person_reads_as_none(pool: PgPool) {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let identifier = Identifier::parse("email", "ada@example.com").unwrap();

        assert!(
            find_person(&pool, ws, Uuid::now_v7())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            find_person_by_identifier(&pool, ws, &identifier)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            list_identities(&pool, ws, Uuid::now_v7())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            list_identity_conflicts(&pool, ws, Uuid::now_v7())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_stored_identity_compares_by_value() {
        let id = Uuid::now_v7();
        let identity = StoredIdentity {
            id,
            person_id: id,
            kind: "email".into(),
            value: "a@b.c".into(),
        };

        assert_eq!(identity.clone(), identity);
        assert!(!format!("{identity:?}").contains("a@b.c"));
    }
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn identifiers_resolve_to_their_owners_in_input_order_and_never_cross_workspaces(
        pool: PgPool,
    ) {
        use serde_json::json;
        use time::macros::datetime;
        use zirv_flow_core::{Cause, TraceContext};

        use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_batch};

        let one = {
            let name = WorkspaceName::parse("one").unwrap();
            create_workspace(&pool, &name, &Slug::parse("one").unwrap())
                .await
                .unwrap()
                .id
        };
        let two = {
            let name = WorkspaceName::parse("two").unwrap();
            create_workspace(&pool, &name, &Slug::parse("two").unwrap())
                .await
                .unwrap()
                .id
        };
        let record = ProfileRecord {
            source: "crm".into(),
            stream: "s".into(),
            record_key: "1".into(),
            identifiers: vec![("email".into(), "ada@example.com".into())],
            traits: json!({}).as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: TraceContext::root(Cause::api("t")),
        };
        let outcomes = apply_profile_batch(&pool, one, &[record]).await.unwrap();
        let ProfileOutcome::Applied { person_id, .. } = outcomes[0] else {
            panic!("not applied");
        };
        let known = Identifier::parse("email", "ada@example.com").unwrap();
        let unknown = Identifier::parse("email", "nobody@example.com").unwrap();

        let found = find_people_by_identifiers(&pool, one, &[unknown.clone(), known.clone()])
            .await
            .unwrap();
        let elsewhere = find_people_by_identifiers(&pool, two, &[known])
            .await
            .unwrap();
        let none = find_people_by_identifiers(&pool, one, &[]).await.unwrap();

        assert_eq!(found, [None, Some(person_id)]);
        assert_eq!(elsewhere, [None]);
        assert!(none.is_empty());
    }
}
