//! The event schema registry: schemas, drafts, immutable versions and per-name settings.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::events::{EventMode, SchemaProperty, validate_event_name, validate_properties};
use zirv_flow_core::{ValidationError, WorkspaceId};
use zirv_flow_store::{
    EventRetention, EventSchema, EventSchemaUpdate, NewEventSchema, PgPool, PublishOutcome,
    SchemaVersion,
};

use crate::AppState;
use crate::admin::{self, AdminError, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

const MAX_DESCRIPTION_CHARS: usize = 500;

/// One property of a schema.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub(crate) struct PropertyBody {
    /// A top-level key of the event's properties: 1 to 128 characters.
    #[schema(example = "total")]
    name: String,
    /// One of `string`, `number`, `integer`, `boolean`, `timestamp`, `date`, `string_list`,
    /// `object`.
    #[serde(rename = "type")]
    #[schema(rename = "type", example = "number")]
    property_type: String,
    #[serde(default)]
    required: bool,
    /// The allowed values; for `string` properties only.
    #[serde(default, rename = "enum", skip_serializing_if = "Option::is_none")]
    #[schema(rename = "enum")]
    enum_values: Option<Vec<String>>,
    /// At most 500 characters.
    #[serde(default)]
    description: String,
    /// Values are masked for viewers without access to personal data.
    #[serde(default)]
    pii: bool,
}

impl From<&SchemaProperty> for PropertyBody {
    fn from(property: &SchemaProperty) -> Self {
        Self {
            name: property.name.clone(),
            property_type: property.property_type.as_str().to_owned(),
            required: property.required,
            enum_values: property.enum_values.clone(),
            description: property.description.clone(),
            pii: property.pii,
        }
    }
}

impl PropertyBody {
    fn into_property(self) -> Result<SchemaProperty, ValidationError> {
        Ok(SchemaProperty {
            name: self.name,
            property_type: self.property_type.parse()?,
            required: self.required,
            enum_values: self.enum_values,
            description: self.description,
            pii: self.pii,
        })
    }
}

fn properties_from(bodies: Vec<PropertyBody>) -> Result<Vec<SchemaProperty>, ValidationError> {
    let properties = bodies
        .into_iter()
        .map(PropertyBody::into_property)
        .collect::<Result<Vec<_>, _>>()?;
    validate_properties(&properties)?;
    Ok(properties)
}

#[derive(Serialize, ToSchema)]
pub(crate) struct EventSchemaBody {
    name: String,
    /// One of `allow`, `warn`, `block`.
    #[schema(example = "warn")]
    mode: String,
    description: String,
    /// The latest published version; null until one is published.
    current_version: Option<i32>,
    /// True while a draft exists, which is not yet in force.
    has_draft: bool,
    /// The draft was inferred from observed events rather than written by a person.
    inferred: bool,
    /// Days events of this name are kept; null means the deployment default.
    retention_days: Option<i32>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl From<&EventSchema> for EventSchemaBody {
    fn from(schema: &EventSchema) -> Self {
        Self {
            name: schema.name.clone(),
            mode: schema.mode.as_str().to_owned(),
            description: schema.description.clone(),
            current_version: schema.current_version,
            has_draft: schema.draft.is_some(),
            inferred: schema.inferred,
            retention_days: schema.retention_days,
            created_at: schema.created_at,
            updated_at: schema.updated_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SchemaVersionBody {
    version: i32,
    properties: Vec<PropertyBody>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    published_at: OffsetDateTime,
    published_by: Option<Uuid>,
}

impl From<&SchemaVersion> for SchemaVersionBody {
    fn from(version: &SchemaVersion) -> Self {
        Self {
            version: version.version,
            properties: version.properties.iter().map(PropertyBody::from).collect(),
            published_at: version.published_at,
            published_by: version.published_by,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct EventSchemaDetailBody {
    schema: EventSchemaBody,
    /// The editable property list; null when there is none.
    draft: Option<Vec<PropertyBody>>,
    /// Published versions, newest first. A version never changes.
    versions: Vec<SchemaVersionBody>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateEventSchemaRequest {
    /// 1 to 128 characters without control characters.
    #[schema(example = "order_completed")]
    name: String,
    /// One of `allow`, `warn` (the default), `block`.
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    description: String,
    /// The first draft.
    #[serde(default)]
    properties: Vec<PropertyBody>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SaveDraftRequest {
    /// Replaces the draft.
    properties: Vec<PropertyBody>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateEventSchemaRequest {
    mode: Option<String>,
    description: Option<String>,
    /// Days events of this name are kept, within the operator's bounds; `null` goes back to
    /// the deployment default.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<i32>)]
    retention_days: Option<Option<i32>>,
}

/// Deserializes a field that was sent at all, `null` included, as `Some`.
fn nullable<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Option<i32>>, D::Error> {
    Option::<i32>::deserialize(deserializer).map(Some)
}

fn invalid(field: &'static str, message: &'static str) -> AdminError {
    AdminError::Invalid(ValidationError::new(field, message))
}

fn check_description(description: &str) -> Result<(), AdminError> {
    if description.chars().count() <= MAX_DESCRIPTION_CHARS {
        Ok(())
    } else {
        Err(invalid("description", "must be at most 500 characters"))
    }
}

fn property_names(properties: &[SchemaProperty]) -> Vec<&str> {
    properties
        .iter()
        .map(|property| property.name.as_str())
        .collect()
}

/// Creates a schema entry with its first draft and audits it.
pub(crate) async fn create_schema(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    request: CreateEventSchemaRequest,
) -> Result<EventSchema, AdminError> {
    validate_event_name(&request.name)?;
    let mode = request
        .mode
        .as_deref()
        .map_or(Ok(EventMode::Warn), str::parse)?;
    check_description(&request.description)?;
    let properties = properties_from(request.properties)?;

    let mut tx = zirv_flow_store::begin(pool).await?;
    let schema = zirv_flow_store::create_event_schema(
        &mut *tx,
        workspace,
        &NewEventSchema {
            name: request.name,
            mode,
            description: request.description,
            properties,
        },
    )
    .await
    .map_err(|error| {
        if error.is_conflict() {
            AdminError::Conflict("an event schema with that name already exists")
        } else {
            error.into()
        }
    })?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "event_schema.created",
        None,
        json!({ "name": schema.name, "mode": schema.mode.as_str() }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(schema)
}

/// Replaces the draft and audits the names of its properties.
pub(crate) async fn save_draft(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    name: &str,
    request: SaveDraftRequest,
) -> Result<EventSchema, AdminError> {
    let properties = properties_from(request.properties)?;
    let mut tx = zirv_flow_store::begin(pool).await?;
    let schema = zirv_flow_store::save_schema_draft(&mut *tx, workspace, name, &properties)
        .await?
        .ok_or(AdminError::NotFound("event schema"))?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "event_schema.draft_saved",
        None,
        json!({ "name": schema.name, "fields": property_names(&properties) }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(schema)
}

/// Publishes the draft as the next version and audits it. Refused when there is no draft.
pub(crate) async fn publish_draft(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    name: &str,
    published_by: Option<Uuid>,
) -> Result<EventSchema, AdminError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let schema = match zirv_flow_store::publish_schema_draft(&mut tx, workspace, name, published_by)
        .await?
    {
        PublishOutcome::Published(schema) => schema,
        PublishOutcome::NoDraft => {
            return Err(AdminError::Conflict("the schema has no draft to publish"));
        }
        PublishOutcome::NotFound => return Err(AdminError::NotFound("event schema")),
    };
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "event_schema.published",
        None,
        json!({ "name": schema.name, "version": schema.current_version }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(schema)
}

/// Changes the mode, description or retention override and audits the names of the changed
/// fields.
pub(crate) async fn update_schema(
    pool: &PgPool,
    context: &Context,
    retention: &EventRetention,
    workspace: WorkspaceId,
    name: &str,
    request: UpdateEventSchemaRequest,
) -> Result<EventSchema, AdminError> {
    let mut fields: Vec<&'static str> = Vec::new();
    let mut update = EventSchemaUpdate::default();
    if let Some(mode) = &request.mode {
        update.mode = Some(mode.parse()?);
        fields.push("mode");
    }
    if let Some(description) = request.description {
        check_description(&description)?;
        update.description = Some(description);
        fields.push("description");
    }
    if let Some(days) = request.retention_days {
        if let Some(days) = days {
            let within = u32::try_from(days)
                .is_ok_and(|days| (retention.min_days..=retention.max_days).contains(&days));
            if !within {
                return Err(invalid(
                    "retention_days",
                    "must lie within the retention bounds set by the operator",
                ));
            }
        }
        update.retention_days = Some(days);
        fields.push("retention_days");
    }
    if fields.is_empty() {
        return Err(invalid("body", "must change at least one field"));
    }

    let mut tx = zirv_flow_store::begin(pool).await?;
    let schema = zirv_flow_store::update_event_schema(&mut tx, workspace, name, &update)
        .await?
        .ok_or(AdminError::NotFound("event schema"))?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "event_schema.updated",
        None,
        json!({ "name": schema.name, "fields": fields }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(schema)
}

async fn detail(
    pool: &PgPool,
    workspace: WorkspaceId,
    schema: &EventSchema,
) -> Result<EventSchemaDetailBody, ApiError> {
    let versions = zirv_flow_store::list_schema_versions(pool, workspace, &schema.name).await?;
    Ok(EventSchemaDetailBody {
        schema: EventSchemaBody::from(schema),
        draft: schema
            .draft
            .as_ref()
            .map(|draft| draft.iter().map(PropertyBody::from).collect()),
        versions: versions.iter().map(SchemaVersionBody::from).collect(),
    })
}

/// The workspace's event schemas.
#[utoipa::path(
    get,
    operation_id = "list_event_schemas",
    path = "/api/v1/workspaces/{ws}/event-schemas",
    tag = "events",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The schemas, ordered by name", body = Vec<EventSchemaBody>),
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
) -> Result<Json<Vec<EventSchemaBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let schemas = zirv_flow_store::list_event_schemas(&state.pool, workspace.id).await?;
    Ok(Json(schemas.iter().map(EventSchemaBody::from).collect()))
}

/// Creates a schema with its first draft. Nothing is in force until it is published.
#[utoipa::path(
    post,
    operation_id = "create_event_schema",
    path = "/api/v1/workspaces/{ws}/event-schemas",
    tag = "events",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateEventSchemaRequest,
    responses(
        (status = 201, description = "The new schema", body = EventSchemaDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The name already exists", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name, mode or property", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateEventSchemaRequest>,
) -> Result<(StatusCode, Json<EventSchemaDetailBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let schema = create_schema(&state.pool, &principal.context(), workspace.id, body).await?;
    let body = detail(&state.pool, workspace.id, &schema).await?;
    Ok((StatusCode::CREATED, Json(body)))
}

/// A schema with its draft and its published versions.
#[utoipa::path(
    get,
    operation_id = "get_event_schema",
    path = "/api/v1/workspaces/{ws}/event-schemas/{name}",
    tag = "events",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("name" = String, Path, description = "Event name, percent-encoded")
    ),
    responses(
        (status = 200, description = "The schema", body = EventSchemaDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or schema, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, name)): Path<(String, String)>,
) -> Result<Json<EventSchemaDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let schema = zirv_flow_store::find_event_schema(&state.pool, workspace.id, &name)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(detail(&state.pool, workspace.id, &schema).await?))
}

/// Replaces the draft.
#[utoipa::path(
    put,
    operation_id = "save_event_schema_draft",
    path = "/api/v1/workspaces/{ws}/event-schemas/{name}/draft",
    tag = "events",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("name" = String, Path, description = "Event name, percent-encoded")
    ),
    request_body = SaveDraftRequest,
    responses(
        (status = 200, description = "The schema with its new draft", body = EventSchemaDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or schema, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid property", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn put_draft(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, name)): Path<(String, String)>,
    ApiJson(body): ApiJson<SaveDraftRequest>,
) -> Result<Json<EventSchemaDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let schema = save_draft(&state.pool, &principal.context(), workspace.id, &name, body).await?;
    Ok(Json(detail(&state.pool, workspace.id, &schema).await?))
}

/// Publishes the draft as the next version, which never changes afterwards, and clears the
/// draft.
#[utoipa::path(
    post,
    operation_id = "publish_event_schema",
    path = "/api/v1/workspaces/{ws}/event-schemas/{name}/versions",
    tag = "events",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("name" = String, Path, description = "Event name, percent-encoded")
    ),
    responses(
        (status = 201, description = "The schema with its new version", body = EventSchemaDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or schema, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The schema has no draft to publish", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn publish(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, name)): Path<(String, String)>,
) -> Result<(StatusCode, Json<EventSchemaDetailBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let published_by = match &principal {
        Principal::Session(session) => Some(session.user.id.as_uuid()),
        Principal::Key(_) => None,
    };
    let schema = publish_draft(
        &state.pool,
        &principal.context(),
        workspace.id,
        &name,
        published_by,
    )
    .await?;
    let body = detail(&state.pool, workspace.id, &schema).await?;
    Ok((StatusCode::CREATED, Json(body)))
}

/// Changes the mode, the description or the retention override.
#[utoipa::path(
    patch,
    operation_id = "update_event_schema",
    path = "/api/v1/workspaces/{ws}/event-schemas/{name}",
    tag = "events",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("name" = String, Path, description = "Event name, percent-encoded")
    ),
    request_body = UpdateEventSchemaRequest,
    responses(
        (status = 200, description = "The updated schema", body = EventSchemaDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or schema, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid mode, a retention outside the operator's bounds, or no field at all", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, name)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateEventSchemaRequest>,
) -> Result<Json<EventSchemaDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let schema = update_schema(
        &state.pool,
        &principal.context(),
        &state.event_retention,
        workspace.id,
        &name,
        body,
    )
    .await?;
    Ok(Json(detail(&state.pool, workspace.id, &schema).await?))
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::events::PropertyType;

    use super::*;

    fn body(json: serde_json::Value) -> PropertyBody {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn a_property_needs_only_a_name_and_a_type() {
        let property = body(json!({"name": "total", "type": "number"}))
            .into_property()
            .unwrap();

        assert_eq!(property.property_type, PropertyType::Number);
        assert!(!property.required && !property.pii && property.enum_values.is_none());
    }

    #[test]
    fn an_unknown_type_or_a_bad_list_is_a_validation_error() {
        let unknown = properties_from(vec![body(json!({"name": "a", "type": "money"}))]);
        let twice = properties_from(vec![
            body(json!({"name": "a", "type": "string"})),
            body(json!({"name": "a", "type": "number"})),
        ]);
        let bad_enum = properties_from(vec![body(
            json!({"name": "a", "type": "number", "enum": ["x"]}),
        )]);

        assert!(unknown.is_err() && twice.is_err() && bad_enum.is_err());
    }

    #[test]
    fn the_body_round_trips_the_type_and_enum_keys() {
        let property = SchemaProperty {
            name: "tier".into(),
            property_type: PropertyType::String,
            required: true,
            enum_values: Some(vec!["free".into()]),
            description: "d".into(),
            pii: true,
        };

        let shown = serde_json::to_value(PropertyBody::from(&property)).unwrap();

        assert_eq!(shown["type"], "string");
        assert_eq!(shown["enum"], json!(["free"]));
        assert_eq!(body(shown).into_property().unwrap(), property);
    }

    #[test]
    fn an_update_tells_a_null_retention_from_an_absent_one() {
        let absent: UpdateEventSchemaRequest =
            serde_json::from_value(json!({"mode": "block"})).unwrap();
        let null: UpdateEventSchemaRequest =
            serde_json::from_value(json!({"retention_days": null})).unwrap();
        let set: UpdateEventSchemaRequest =
            serde_json::from_value(json!({"retention_days": 90})).unwrap();

        assert_eq!(absent.retention_days, None);
        assert_eq!(null.retention_days, Some(None));
        assert_eq!(set.retention_days, Some(Some(90)));
    }

    #[test]
    fn descriptions_are_capped_at_500_characters() {
        assert!(check_description(&"a".repeat(500)).is_ok());
        assert!(check_description(&"a".repeat(501)).is_err());
    }
}
