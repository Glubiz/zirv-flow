//! The field registry: the trait definitions of one workspace (architecture section 7.1).

use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::people::{Precedence, TraitType};

use crate::StoreError;

macro_rules! definition_columns {
    () => {
        "workspace_id, key, type, precedence, source_order, pii, channel_address, inferred, \
         archived, description, created_at, updated_at"
    };
}

/// A trait definition as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct TraitDefinition {
    pub workspace_id: WorkspaceId,
    pub key: String,
    pub trait_type: TraitType,
    pub precedence: Precedence,
    pub source_order: Vec<String>,
    pub pii: bool,
    pub channel_address: bool,
    pub inferred: bool,
    pub archived: bool,
    pub description: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// A definition to create. The key and the type are already validated.
#[derive(Clone, Debug)]
pub struct NewTraitDefinition {
    pub key: String,
    pub trait_type: TraitType,
    pub precedence: Precedence,
    pub source_order: Vec<String>,
    pub pii: bool,
    pub channel_address: bool,
    pub inferred: bool,
    pub description: String,
}

/// The mutable fields of a definition; `None` leaves a field as it is. The type is not here
/// because it never changes.
#[derive(Clone, Debug, Default)]
pub struct TraitDefinitionUpdate {
    pub precedence: Option<Precedence>,
    pub source_order: Option<Vec<String>>,
    pub pii: Option<bool>,
    pub channel_address: Option<bool>,
    pub description: Option<String>,
    pub archived: Option<bool>,
}

#[derive(FromRow)]
pub(crate) struct DefinitionRow {
    workspace_id: Uuid,
    key: String,
    r#type: String,
    precedence: String,
    source_order: Vec<String>,
    pii: bool,
    channel_address: bool,
    inferred: bool,
    archived: bool,
    description: String,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

fn corrupt(what: &'static str) -> StoreError {
    StoreError::query(sqlx::Error::Decode(what.into()))
}

impl DefinitionRow {
    pub(crate) fn into_definition(self) -> Result<TraitDefinition, StoreError> {
        Ok(TraitDefinition {
            workspace_id: WorkspaceId::from_uuid(self.workspace_id),
            key: self.key,
            trait_type: self
                .r#type
                .parse()
                .map_err(|_| corrupt("unknown trait type"))?,
            precedence: Precedence::parse(&self.precedence)
                .ok_or_else(|| corrupt("unknown precedence"))?,
            source_order: self.source_order,
            pii: self.pii,
            channel_address: self.channel_address,
            inferred: self.inferred,
            archived: self.archived,
            description: self.description,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// Every definition of the workspace, archived ones included, ordered by key.
pub async fn list_trait_definitions<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Vec<TraitDefinition>, StoreError> {
    let rows: Vec<DefinitionRow> = sqlx::query_as(concat!(
        "SELECT ",
        definition_columns!(),
        " FROM trait_definitions WHERE workspace_id = $1 ORDER BY key"
    ))
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    rows.into_iter()
        .map(DefinitionRow::into_definition)
        .collect()
}

pub async fn find_trait_definition<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    key: &str,
) -> Result<Option<TraitDefinition>, StoreError> {
    let row: Option<DefinitionRow> = sqlx::query_as(concat!(
        "SELECT ",
        definition_columns!(),
        " FROM trait_definitions WHERE workspace_id = $1 AND key = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(key)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(DefinitionRow::into_definition).transpose()
}

/// Reads a definition and locks its row until the caller's transaction ends, so a second
/// update of the same key waits and then compares against what this one wrote.
pub async fn find_trait_definition_for_update<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    key: &str,
) -> Result<Option<TraitDefinition>, StoreError> {
    let row: Option<DefinitionRow> = sqlx::query_as(concat!(
        "SELECT ",
        definition_columns!(),
        " FROM trait_definitions WHERE workspace_id = $1 AND key = $2 FOR UPDATE"
    ))
    .bind(workspace.as_uuid())
    .bind(key)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(DefinitionRow::into_definition).transpose()
}

/// Creates a definition. A key that exists, archived or not, is a conflict.
pub async fn create_trait_definition<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    new: &NewTraitDefinition,
) -> Result<TraitDefinition, StoreError> {
    let row: DefinitionRow = sqlx::query_as(concat!(
        "INSERT INTO trait_definitions \
            (workspace_id, key, type, precedence, source_order, pii, channel_address, inferred, \
             description) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING ",
        definition_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(&new.key)
    .bind(new.trait_type.as_str())
    .bind(new.precedence.as_str())
    .bind(&new.source_order)
    .bind(new.pii)
    .bind(new.channel_address)
    .bind(new.inferred)
    .bind(&new.description)
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)?;
    row.into_definition()
}

/// Applies the given fields. `None` when the key does not exist in the workspace.
pub async fn update_trait_definition<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    key: &str,
    update: &TraitDefinitionUpdate,
) -> Result<Option<TraitDefinition>, StoreError> {
    let row: Option<DefinitionRow> = sqlx::query_as(concat!(
        "UPDATE trait_definitions SET \
            precedence = COALESCE($3, precedence), \
            source_order = COALESCE($4, source_order), \
            pii = COALESCE($5, pii), \
            channel_address = COALESCE($6, channel_address), \
            description = COALESCE($7, description), \
            archived = COALESCE($8, archived), \
            updated_at = now() \
         WHERE workspace_id = $1 AND key = $2 RETURNING ",
        definition_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(key)
    .bind(update.precedence.map(|p| p.as_str()))
    .bind(update.source_order.as_deref())
    .bind(update.pii)
    .bind(update.channel_address)
    .bind(update.description.as_deref())
    .bind(update.archived)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(DefinitionRow::into_definition).transpose()
}

/// Registers an unregistered key with an inferred type. A concurrent registration wins:
/// `false` means the key already existed, so the caller re-reads the definition.
pub(crate) async fn insert_inferred_definition(
    connection: &mut sqlx::PgConnection,
    workspace: WorkspaceId,
    key: &str,
    trait_type: TraitType,
) -> Result<bool, StoreError> {
    let inserted = sqlx::query(
        "INSERT INTO trait_definitions (workspace_id, key, type, precedence, inferred) \n         VALUES ($1, $2, $3, 'latest', true) ON CONFLICT DO NOTHING",
    )
    .bind(workspace.as_uuid())
    .bind(key)
    .bind(trait_type.as_str())
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(inserted.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    pub(crate) fn new_definition(key: &str) -> NewTraitDefinition {
        NewTraitDefinition {
            key: key.to_owned(),
            trait_type: TraitType::String,
            precedence: Precedence::Latest,
            source_order: Vec::new(),
            pii: false,
            channel_address: false,
            inferred: false,
            description: String::new(),
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_definition_is_created_listed_and_found(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mut new = new_definition("plan");
        new.precedence = Precedence::SourcePriority;
        new.source_order = vec!["crm".into(), "shop".into()];
        new.pii = true;

        let created = create_trait_definition(&pool, ws, &new).await.unwrap();

        assert_eq!(created.key, "plan");
        assert_eq!(created.source_order, ["crm", "shop"]);
        assert!(created.pii && !created.archived && !created.inferred);
        assert_eq!(
            list_trait_definitions(&pool, ws).await.unwrap(),
            std::slice::from_ref(&created)
        );
        assert_eq!(
            find_trait_definition(&pool, ws, "plan").await.unwrap(),
            Some(created)
        );
        assert_eq!(
            find_trait_definition(&pool, ws, "nope").await.unwrap(),
            None
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_duplicate_key_is_a_conflict_even_when_archived(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        create_trait_definition(&pool, ws, &new_definition("plan"))
            .await
            .unwrap();
        update_trait_definition(
            &pool,
            ws,
            "plan",
            &TraitDefinitionUpdate {
                archived: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let error = create_trait_definition(&pool, ws, &new_definition("plan"))
            .await
            .unwrap_err();

        assert!(error.is_conflict());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_update_changes_only_the_given_fields(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        create_trait_definition(&pool, ws, &new_definition("plan"))
            .await
            .unwrap();

        let updated = update_trait_definition(
            &pool,
            ws,
            "plan",
            &TraitDefinitionUpdate {
                description: Some("The plan".into()),
                pii: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(updated.description, "The plan");
        assert!(updated.pii);
        assert_eq!(updated.precedence, Precedence::Latest);
        assert_eq!(updated.trait_type, TraitType::String);
        let missing = update_trait_definition(&pool, ws, "nope", &TraitDefinitionUpdate::default())
            .await
            .unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn an_update_defaults_to_changing_nothing() {
        let update = TraitDefinitionUpdate::default();

        assert!(update.precedence.is_none() && update.archived.is_none());
        assert!(update.source_order.is_none() && update.description.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_locking_read_makes_a_second_one_wait_and_then_see_the_first_ones_write(
        pool: PgPool,
    ) {
        use std::time::Duration;

        let ws = workspace(&pool, "one").await;
        create_trait_definition(&pool, ws, &new_definition("plan"))
            .await
            .unwrap();
        let mut first = crate::begin(&pool).await.unwrap();
        let before = find_trait_definition_for_update(&mut *first, ws, "plan")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.precedence, Precedence::Latest);

        let second = {
            let pool = pool.clone();
            tokio::spawn(async move {
                let mut tx = crate::begin(&pool).await.unwrap();
                let seen = find_trait_definition_for_update(&mut *tx, ws, "plan")
                    .await
                    .unwrap()
                    .unwrap();
                crate::commit(tx).await.unwrap();
                seen
            })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!second.is_finished(), "the second read did not wait");
        update_trait_definition(
            &mut *first,
            ws,
            "plan",
            &TraitDefinitionUpdate {
                precedence: Some(Precedence::First),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        crate::commit(first).await.unwrap();

        let seen = second.await.unwrap();
        assert_eq!(seen.precedence, Precedence::First);
        assert!(
            find_trait_definition_for_update(&pool, ws, "nope")
                .await
                .unwrap()
                .is_none()
        );
    }
}
