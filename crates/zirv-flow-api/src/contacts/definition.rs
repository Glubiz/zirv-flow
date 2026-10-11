//! The contact definition of a workspace: which registry fields its contacts have, which are
//! required and which are identifiers.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::contacts::ContactField;
use zirv_flow_store::{ContactDefinition, DefinitionError};

use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, ItemProblem, Problem};

/// One field of the definition.
#[derive(Deserialize, Serialize, ToSchema)]
pub(crate) struct ContactFieldBody {
    /// A key of the field registry; the registry supplies the type.
    #[schema(example = "email")]
    key: String,
    /// A contact without this field is refused with 422.
    #[serde(default)]
    required: bool,
    /// `email`, `phone`, `anonymous_id` or `external:<name>`: the value also becomes an
    /// identity of the contact's person. The field must be a `string` field. Null for an
    /// ordinary field.
    #[serde(default)]
    #[schema(example = "email")]
    identifier: Option<String>,
}

impl From<ContactFieldBody> for ContactField {
    fn from(body: ContactFieldBody) -> Self {
        Self {
            key: body.key,
            required: body.required,
            identifier: body.identifier,
        }
    }
}

impl From<ContactField> for ContactFieldBody {
    fn from(field: ContactField) -> Self {
        Self {
            key: field.key,
            required: field.required,
            identifier: field.identifier,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ContactDefinitionBody {
    /// False while the workspace has saved no definition: contacts then need only a key, and
    /// any registry key is accepted as a field. A saved definition, even an empty one,
    /// allows only its fields.
    defined: bool,
    /// In display order. The contact key is implicit and not listed.
    fields: Vec<ContactFieldBody>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    updated_at: Option<OffsetDateTime>,
    /// The user who saved it; null when an API key did, or when none is saved.
    updated_by: Option<Uuid>,
}

impl From<Option<ContactDefinition>> for ContactDefinitionBody {
    fn from(definition: Option<ContactDefinition>) -> Self {
        match definition {
            Some(definition) => Self {
                defined: true,
                fields: definition.fields.into_iter().map(Into::into).collect(),
                updated_at: Some(definition.updated_at),
                updated_by: definition.updated_by,
            },
            None => Self {
                defined: false,
                fields: Vec::new(),
                updated_at: None,
                updated_by: None,
            },
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct PutContactDefinitionRequest {
    /// At most 100 fields, each registry key once, each identifier kind once.
    fields: Vec<ContactFieldBody>,
}

/// The workspace's contact definition.
#[utoipa::path(
    get,
    operation_id = "get_contact_definition",
    path = "/api/v1/workspaces/{ws}/contact-definition",
    tag = "contacts",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The definition", body = ContactDefinitionBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<ContactDefinitionBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let definition = zirv_flow_store::find_contact_definition(&state.pool, workspace.id).await?;
    Ok(Json(definition.into()))
}

/// Saves the contact definition, replacing the previous one. Every key must be a live registry
/// key, and an identifier field must be a `string` field. Needs the workspace admin role.
/// Audited as `contact_definition.updated`.
#[utoipa::path(
    put,
    operation_id = "put_contact_definition",
    path = "/api/v1/workspaces/{ws}/contact-definition",
    tag = "contacts",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = PutContactDefinitionRequest,
    responses(
        (status = 200, description = "The saved definition", body = ContactDefinitionBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A malformed list, or fields the registry cannot back; errors lists them by position, key and reason (`unknown_key`, `archived_key`, `identifier_needs_string`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn put(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<PutContactDefinitionRequest>,
) -> Result<Json<ContactDefinitionBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let fields: Vec<ContactField> = body.fields.into_iter().map(Into::into).collect();
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let saved = zirv_flow_store::put_contact_definition(
        &mut tx,
        workspace.id,
        &fields,
        principal.user_id().map(|user| user.as_uuid()),
    )
    .await
    .map_err(|error| match error {
        DefinitionError::Invalid(error) => ApiError::from(error),
        DefinitionError::Fields(problems) => {
            ApiError::unprocessable("one or more fields cannot be used").with_errors(
                problems
                    .into_iter()
                    .map(|problem| ItemProblem {
                        index: u32::try_from(problem.index).unwrap_or(u32::MAX),
                        field: problem.key,
                        reason: problem.reason.to_owned(),
                    })
                    .collect(),
            )
        }
        DefinitionError::Store(error) => error.into(),
    })?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "contact_definition.updated",
        None,
        json!({ "fields": fields.len() }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(Json(Some(saved).into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_without_a_saved_definition_reads_as_undefined_and_empty() {
        let body = ContactDefinitionBody::from(None);

        assert!(!body.defined);
        assert!(body.fields.is_empty());
        assert!(body.updated_at.is_none());
    }

    #[test]
    fn a_field_defaults_to_optional_and_not_an_identifier() {
        let field: ContactFieldBody = serde_json::from_value(json!({"key": "plan"})).unwrap();

        let core = ContactField::from(field);

        assert!(!core.required);
        assert_eq!(core.identifier, None);
    }
}
