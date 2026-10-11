//! The field registry: the trait definitions of a workspace.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use zirv_flow_core::people::{Precedence, TraitType, validate_key};
use zirv_flow_core::{ValidationError, WorkspaceId};
use zirv_flow_store::{NewJob, NewTraitDefinition, PgPool, TraitDefinition, TraitDefinitionUpdate};

use crate::AppState;
use crate::admin::{self, AdminError, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

const MAX_DESCRIPTION_CHARS: usize = 500;
const MAX_SOURCES: usize = 50;
const MAX_SOURCE_CHARS: usize = 64;

#[derive(Serialize, ToSchema)]
pub(crate) struct TraitBody {
    key: String,
    /// One of `string`, `number`, `boolean`, `timestamp`, `date`, `string_list`.
    #[serde(rename = "type")]
    #[schema(rename = "type", example = "string")]
    trait_type: String,
    /// One of `latest`, `source_priority`, `first`.
    #[schema(example = "latest")]
    precedence: String,
    /// Source names in priority order; used by `source_priority`.
    source_order: Vec<String>,
    pii: bool,
    channel_address: bool,
    /// Registered automatically from an incoming record rather than created by a person.
    inferred: bool,
    archived: bool,
    description: String,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl From<&TraitDefinition> for TraitBody {
    fn from(definition: &TraitDefinition) -> Self {
        Self {
            key: definition.key.clone(),
            trait_type: definition.trait_type.as_str().to_owned(),
            precedence: definition.precedence.as_str().to_owned(),
            source_order: definition.source_order.clone(),
            pii: definition.pii,
            channel_address: definition.channel_address,
            inferred: definition.inferred,
            archived: definition.archived,
            description: definition.description.clone(),
            created_at: definition.created_at,
            updated_at: definition.updated_at,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateTraitRequest {
    /// Lowercase letters, digits and underscores, starting with a letter; at most 64.
    #[schema(example = "plan_tier")]
    key: String,
    /// One of `string`, `number`, `boolean`, `timestamp`, `date`, `string_list`. It cannot
    /// be changed later: archive the key and create a new one.
    #[serde(rename = "type")]
    #[schema(rename = "type", example = "string")]
    trait_type: String,
    /// One of `latest` (the default), `source_priority`, `first`.
    #[serde(default)]
    precedence: Option<String>,
    /// Source names in priority order, for `source_priority`.
    #[serde(default)]
    source_order: Vec<String>,
    #[serde(default)]
    pii: bool,
    #[serde(default)]
    channel_address: bool,
    /// At most 500 characters.
    #[serde(default)]
    description: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateTraitRequest {
    precedence: Option<String>,
    source_order: Option<Vec<String>>,
    pii: Option<bool>,
    channel_address: Option<bool>,
    description: Option<String>,
    archived: Option<bool>,
    /// Refused with 422 when present: the type of a trait never changes.
    #[serde(rename = "type", default, deserialize_with = "present")]
    #[schema(ignore)]
    trait_type: Option<serde_json::Value>,
}

/// Deserializes a field that was sent at all, `null` included, as `Some`.
fn present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}

fn invalid(field: &'static str, message: &'static str) -> AdminError {
    AdminError::Invalid(ValidationError::new(field, message))
}

fn parse_precedence(text: &str) -> Result<Precedence, AdminError> {
    Precedence::parse(text)
        .ok_or_else(|| invalid("precedence", "must be latest, source_priority or first"))
}

fn check_source_order(order: &[String]) -> Result<(), AdminError> {
    let valid = order.len() <= MAX_SOURCES
        && order
            .iter()
            .all(|source| !source.is_empty() && source.chars().count() <= MAX_SOURCE_CHARS);
    if valid {
        Ok(())
    } else {
        Err(invalid(
            "source_order",
            "must list at most 50 non-empty source names of at most 64 characters",
        ))
    }
}

fn check_description(description: &str) -> Result<(), AdminError> {
    if description.chars().count() <= MAX_DESCRIPTION_CHARS {
        Ok(())
    } else {
        Err(invalid("description", "must be at most 500 characters"))
    }
}

/// Creates a definition and audits it. Refused when the key exists, archived or not.
pub(crate) async fn create_trait(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    request: &CreateTraitRequest,
) -> Result<TraitDefinition, AdminError> {
    validate_key(&request.key)?;
    let trait_type: TraitType = request.trait_type.parse()?;
    let precedence = request
        .precedence
        .as_deref()
        .map_or(Ok(Precedence::Latest), parse_precedence)?;
    check_source_order(&request.source_order)?;
    check_description(&request.description)?;

    let mut tx = zirv_flow_store::begin(pool).await?;
    let definition = zirv_flow_store::create_trait_definition(
        &mut *tx,
        workspace,
        &NewTraitDefinition {
            key: request.key.clone(),
            trait_type,
            precedence,
            source_order: request.source_order.clone(),
            pii: request.pii,
            channel_address: request.channel_address,
            inferred: false,
            description: request.description.clone(),
        },
    )
    .await
    .map_err(|error| {
        if error.is_conflict() {
            AdminError::Conflict("a trait with that key already exists")
        } else {
            error.into()
        }
    })?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "trait.created",
        None,
        json!({ "key": definition.key }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(definition)
}

/// Updates the mutable fields of a definition and audits the names of the changed fields.
pub(crate) async fn update_trait(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    key: &str,
    request: &UpdateTraitRequest,
) -> Result<TraitDefinition, AdminError> {
    if request.trait_type.is_some() {
        return Err(invalid("type", "cannot be changed"));
    }
    let mut fields: Vec<&'static str> = Vec::new();
    let mut update = TraitDefinitionUpdate::default();
    if let Some(precedence) = &request.precedence {
        update.precedence = Some(parse_precedence(precedence)?);
        fields.push("precedence");
    }
    if let Some(order) = &request.source_order {
        check_source_order(order)?;
        update.source_order = Some(order.clone());
        fields.push("source_order");
    }
    if let Some(pii) = request.pii {
        update.pii = Some(pii);
        fields.push("pii");
    }
    if let Some(channel_address) = request.channel_address {
        update.channel_address = Some(channel_address);
        fields.push("channel_address");
    }
    if let Some(description) = &request.description {
        check_description(description)?;
        update.description = Some(description.clone());
        fields.push("description");
    }
    if let Some(archived) = request.archived {
        update.archived = Some(archived);
        fields.push("archived");
    }
    if fields.is_empty() {
        return Err(invalid("body", "must change at least one field"));
    }

    let mut tx = zirv_flow_store::begin(pool).await?;
    // Locked until commit: an overlapping update waits, then compares against this one.
    let before = zirv_flow_store::find_trait_definition_for_update(&mut *tx, workspace, key)
        .await?
        .ok_or(AdminError::NotFound("trait"))?;
    let definition = zirv_flow_store::update_trait_definition(&mut *tx, workspace, key, &update)
        .await?
        .ok_or(AdminError::NotFound("trait"))?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "trait.updated",
        None,
        json!({ "key": definition.key, "fields": fields }),
    )
    .await?;
    // Materialized values follow the registry: a rule change reaches existing people through
    // a job that commits with this change.
    // Only a change of what is stored needs the materialized values recomputed.
    if before.precedence != definition.precedence
        || before.source_order != definition.source_order
        || before.archived != definition.archived
    {
        let mut job = NewJob::new("traits.recompute", super::request_trace());
        job.workspace_id = Some(workspace);
        job.payload = json!({ "workspace_id": workspace, "key": definition.key });
        zirv_flow_store::enqueue(&mut *tx, &job).await?;
    }
    zirv_flow_store::commit(tx).await?;
    Ok(definition)
}

/// The workspace's field registry, archived definitions included.
#[utoipa::path(
    get,
    operation_id = "list_traits",
    path = "/api/v1/workspaces/{ws}/traits",
    tag = "people",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The definitions, ordered by key", body = Vec<TraitBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<Vec<TraitBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let definitions = zirv_flow_store::list_trait_definitions(&state.pool, workspace.id).await?;
    Ok(Json(definitions.iter().map(TraitBody::from).collect()))
}

/// Registers a trait.
#[utoipa::path(
    post,
    operation_id = "create_trait",
    path = "/api/v1/workspaces/{ws}/traits",
    tag = "people",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateTraitRequest,
    responses(
        (status = 201, description = "The new definition", body = TraitBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The key already exists", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid key, type, precedence or other field", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateTraitRequest>,
) -> Result<(StatusCode, Json<TraitBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let definition = create_trait(&state.pool, &principal.context(), workspace.id, &body).await?;
    Ok((StatusCode::CREATED, Json(TraitBody::from(&definition))))
}

/// Changes the mutable fields of a trait. The type is not one of them.
#[utoipa::path(
    patch,
    operation_id = "update_trait",
    path = "/api/v1/workspaces/{ws}/traits/{key}",
    tag = "people",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("key" = String, Path, description = "Trait key")
    ),
    request_body = UpdateTraitRequest,
    responses(
        (status = 200, description = "The updated definition", body = TraitBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or trait, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid field, no field at all, or an attempt to change the type", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, key)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateTraitRequest>,
) -> Result<Json<TraitBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let definition =
        update_trait(&state.pool, &principal.context(), workspace.id, &key, &body).await?;
    Ok(Json(TraitBody::from(&definition)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_request(json: serde_json::Value) -> CreateTraitRequest {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn a_create_request_needs_only_a_key_and_a_type() {
        let request = create_request(json!({"key": "plan", "type": "string"}));

        assert_eq!(request.trait_type, "string");
        assert!(request.precedence.is_none() && request.source_order.is_empty());
        assert!(!request.pii && !request.channel_address && request.description.is_empty());
    }

    #[test]
    fn precedence_source_order_and_description_are_checked() {
        assert!(parse_precedence("latest").is_ok());
        assert!(matches!(
            parse_precedence("newest"),
            Err(AdminError::Invalid(error)) if error.field() == "precedence"
        ));
        assert!(check_source_order(&["crm".into()]).is_ok());
        assert!(check_source_order(&["".into()]).is_err());
        assert!(check_source_order(&vec!["x".to_owned(); 51]).is_err());
        assert!(check_description(&"a".repeat(500)).is_ok());
        assert!(check_description(&"a".repeat(501)).is_err());
    }

    #[test]
    fn the_body_shows_the_type_under_its_json_name_and_rfc3339_times() {
        let definition = TraitDefinition {
            workspace_id: WorkspaceId::new(),
            key: "plan".into(),
            trait_type: TraitType::StringList,
            precedence: Precedence::SourcePriority,
            source_order: vec!["crm".into()],
            pii: true,
            channel_address: false,
            inferred: true,
            archived: false,
            description: "d".into(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };

        let json = serde_json::to_value(TraitBody::from(&definition)).unwrap();

        assert_eq!(json["type"], "string_list");
        assert_eq!(json["precedence"], "source_priority");
        assert_eq!(json["created_at"], "1970-01-01T00:00:00Z");
        assert_eq!(json["inferred"], true);
    }

    #[test]
    fn an_update_request_notices_a_type() {
        let with: UpdateTraitRequest = serde_json::from_value(json!({"type": "number"})).unwrap();
        let without: UpdateTraitRequest = serde_json::from_value(json!({"pii": true})).unwrap();

        let null: UpdateTraitRequest = serde_json::from_value(json!({"type": null})).unwrap();

        assert!(with.trait_type.is_some());
        assert!(null.trait_type.is_some());
        assert!(without.trait_type.is_none() && without.pii == Some(true));
    }
}
