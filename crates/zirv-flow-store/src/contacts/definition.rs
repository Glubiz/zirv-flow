//! The contact definition (architecture section 7.6): which registry fields a workspace's
//! contacts have, which are required and which are identifiers.

use serde_json::Value;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::contacts::{ContactField, RegistryKey, TypedField, validate_definition};
use zirv_flow_core::people::TraitType;
use zirv_flow_core::{ValidationError, WorkspaceId};

use crate::StoreError;
use crate::people::list_trait_definitions;

/// A workspace's contact definition.
#[derive(Clone, Debug, PartialEq)]
pub struct ContactDefinition {
    pub fields: Vec<ContactField>,
    pub updated_at: OffsetDateTime,
    /// The user who saved it; none when an API key did.
    pub updated_by: Option<Uuid>,
}

/// A definition field the registry cannot back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DefinitionProblem {
    /// Position in the submitted list.
    pub index: usize,
    pub key: String,
    /// `unknown_key`, `archived_key` or `identifier_needs_string`.
    pub reason: &'static str,
}

/// Why a definition was not saved.
#[derive(Debug)]
pub enum DefinitionError {
    /// The list itself is malformed.
    Invalid(ValidationError),
    /// Fields the registry cannot back.
    Fields(Vec<DefinitionProblem>),
    Store(StoreError),
}

impl From<StoreError> for DefinitionError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

fn corrupt(what: &'static str) -> StoreError {
    StoreError::query(sqlx::Error::Decode(what.into()))
}

/// The workspace's definition, or none while it has not saved one.
pub async fn find_contact_definition<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Option<ContactDefinition>, StoreError> {
    let row: Option<(Value, OffsetDateTime, Option<Uuid>)> = sqlx::query_as(
        "SELECT fields, updated_at, updated_by FROM contact_definitions WHERE workspace_id = $1",
    )
    .bind(workspace.as_uuid())
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(|(fields, updated_at, updated_by)| {
        Ok(ContactDefinition {
            fields: serde_json::from_value(fields).map_err(|_| corrupt("malformed definition"))?,
            updated_at,
            updated_by,
        })
    })
    .transpose()
}

/// Saves the definition, replacing the previous one. Every key must be a live registry key,
/// and an identifier field must be a `string` field.
pub async fn put_contact_definition(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    fields: &[ContactField],
    updated_by: Option<Uuid>,
) -> Result<ContactDefinition, DefinitionError> {
    validate_definition(fields).map_err(DefinitionError::Invalid)?;
    let registry = list_trait_definitions(&mut *connection, workspace).await?;
    let problems: Vec<DefinitionProblem> = fields
        .iter()
        .enumerate()
        .filter_map(|(index, field)| {
            let reason = match registry.iter().find(|entry| entry.key == field.key) {
                None => "unknown_key",
                Some(entry) if entry.archived => "archived_key",
                Some(entry)
                    if field.identifier.is_some() && entry.trait_type != TraitType::String =>
                {
                    "identifier_needs_string"
                }
                Some(_) => return None,
            };
            Some(DefinitionProblem {
                index,
                key: field.key.clone(),
                reason,
            })
        })
        .collect();
    if !problems.is_empty() {
        return Err(DefinitionError::Fields(problems));
    }
    let value = serde_json::to_value(fields).map_err(|_| corrupt("definition cannot be stored"))?;
    let (updated_at,): (OffsetDateTime,) = sqlx::query_as(
        "INSERT INTO contact_definitions (workspace_id, fields, updated_by) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (workspace_id) DO UPDATE \
            SET fields = EXCLUDED.fields, updated_by = EXCLUDED.updated_by, updated_at = now() \
         RETURNING updated_at",
    )
    .bind(workspace.as_uuid())
    .bind(value)
    .bind(updated_by)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(ContactDefinition {
        fields: fields.to_vec(),
        updated_at,
        updated_by,
    })
}

/// Every key of the workspace's field registry, archived ones included.
pub async fn contact_registry(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Vec<RegistryKey>, StoreError> {
    Ok(list_trait_definitions(connection, workspace)
        .await?
        .into_iter()
        .map(|entry| RegistryKey {
            key: entry.key,
            trait_type: entry.trait_type,
            archived: entry.archived,
        })
        .collect())
}

/// The definition with the registry type of each field, for validating a write. A field whose
/// registry key was archived is left out, so a write cannot set it; none while the workspace
/// has no definition.
pub async fn typed_definition(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Option<Vec<TypedField>>, StoreError> {
    let Some(definition) = find_contact_definition(&mut *connection, workspace).await? else {
        return Ok(None);
    };
    let registry = list_trait_definitions(&mut *connection, workspace).await?;
    Ok(Some(
        definition
            .fields
            .into_iter()
            .filter_map(|field| {
                let entry = registry
                    .iter()
                    .find(|entry| entry.key == field.key && !entry.archived)?;
                Some(TypedField {
                    field,
                    trait_type: entry.trait_type,
                })
            })
            .collect(),
    ))
}
