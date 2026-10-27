//! The segments of a workspace: create, save versions, read, list and archive. Each write is
//! audited with ids, the name and the version, never the definition's values.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use utoipa::openapi::schema::{ObjectBuilder, SchemaType, Type};
use utoipa::openapi::{Ref, RefOr, Schema};
use uuid::Uuid;
use zirv_flow_core::{UserId, ValidationError, WorkspaceId};
use zirv_flow_segments::{compile, normalize, parse_and_validate};
use zirv_flow_store::{
    CADENCE_MINUTES, DEFAULT_CADENCE_MINUTES, NewSegment, NewSegmentVersion, PgPool, Segment,
    SegmentPatch, SegmentVersion,
};

use super::{catalog_of, issues_error, name_taken, segment_error};
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::consent::bool_param;
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::trace::request_trace;

const MAX_NAME_CHARS: usize = 100;
const MAX_DESCRIPTION_CHARS: usize = 500;

fn definition_schema() -> RefOr<Schema> {
    RefOr::Ref(Ref::from_schema_name(super::schema::DEFINITION))
}

fn cadence_schema() -> Schema {
    ObjectBuilder::new()
        .schema_type(SchemaType::Type(Type::Integer))
        .enum_values(Some(CADENCE_MINUTES))
        .description(Some(
            "How often a time-sensitive segment is recomputed in full, in minutes; default 60.",
        ))
        .into()
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SegmentBody {
    id: Uuid,
    #[schema(example = "Pro customers")]
    name: String,
    description: String,
    /// The version that is current: the newest one saved.
    current_version: i32,
    /// Minutes between full recomputes.
    #[schema(schema_with = cadence_schema)]
    cadence_minutes: i32,
    /// The version the members were last computed from; null before the first refresh.
    computed_version: Option<i32>,
    /// When the members were last computed; null before the first refresh.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    computed_at: Option<OffsetDateTime>,
    /// How many people are members now.
    member_count: i32,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    archived_at: Option<OffsetDateTime>,
    /// The user who created it; null when an API key did.
    created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl From<&Segment> for SegmentBody {
    fn from(segment: &Segment) -> Self {
        Self {
            id: segment.id,
            name: segment.name.clone(),
            description: segment.description.clone(),
            current_version: segment.current_version,
            cadence_minutes: segment.cadence_minutes,
            computed_version: segment.computed_version,
            computed_at: segment.computed_at,
            member_count: segment.member_count,
            archived_at: segment.archived_at,
            created_by: segment.created_by,
            created_at: segment.created_at,
            updated_at: segment.updated_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SegmentVersionBody {
    version: i32,
    /// The definition as it was saved.
    #[schema(schema_with = definition_schema)]
    definition: Value,
    /// What the definition reads: traits, events, lists, segments, consent, identifiers and the
    /// change classes.
    #[schema(value_type = Object)]
    deps: Value,
    /// Whether membership can change as time passes with no new data.
    time_sensitive: bool,
    created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
}

impl From<&SegmentVersion> for SegmentVersionBody {
    fn from(version: &SegmentVersion) -> Self {
        Self {
            version: version.version,
            definition: version.definition.clone(),
            deps: version.deps.clone(),
            time_sensitive: version.time_sensitive,
            created_by: version.created_by,
            created_at: version.created_at,
        }
    }
}

/// A segment with its current version.
#[derive(Serialize, ToSchema)]
pub(crate) struct SegmentDetailBody {
    #[serde(flatten)]
    segment: SegmentBody,
    current: SegmentVersionBody,
}

impl SegmentDetailBody {
    fn new(segment: &Segment, version: &SegmentVersion) -> Self {
        Self {
            segment: SegmentBody::from(segment),
            current: SegmentVersionBody::from(version),
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateSegmentRequest {
    /// 1 to 100 characters after trimming; unique among the workspace's live segments, ignoring
    /// case.
    #[schema(example = "Pro customers")]
    name: String,
    /// At most 500 characters.
    #[serde(default)]
    description: String,
    #[schema(schema_with = cadence_schema)]
    cadence_minutes: Option<i32>,
    /// The first version.
    #[schema(schema_with = definition_schema)]
    definition: Value,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateSegmentRequest {
    name: Option<String>,
    description: Option<String>,
    #[schema(schema_with = cadence_schema)]
    cadence_minutes: Option<i32>,
    /// A definition is saved as the next version and becomes the current one.
    #[schema(schema_with = definition_schema)]
    definition: Option<Value>,
}

fn invalid(field: &'static str, message: &'static str) -> ApiError {
    ValidationError::new(field, message).into()
}

fn check_name(name: &str) -> Result<&str, ApiError> {
    let name = name.trim();
    let length = name.chars().count();
    if length == 0 || length > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        Err(invalid(
            "name",
            "must be 1 to 100 characters without control characters",
        ))
    } else {
        Ok(name)
    }
}

fn check_description(description: &str) -> Result<(), ApiError> {
    if description.chars().count() <= MAX_DESCRIPTION_CHARS {
        Ok(())
    } else {
        Err(invalid("description", "must be at most 500 characters"))
    }
}

fn check_cadence(cadence: i32) -> Result<i32, ApiError> {
    if CADENCE_MINUTES.contains(&cadence) {
        Ok(cadence)
    } else {
        Err(invalid(
            "cadence_minutes",
            "must be one of 5, 10, 15, 30, 60, 120, 180, 240, 360, 480, 720, 1440, 10080",
        ))
    }
}

/// Validates a definition against the catalog read in `connection`'s transaction and compiles
/// it, which gives the normalized form, the dependencies and whether it is time-sensitive.
async fn prepare(
    connection: &mut zirv_flow_store::PgConnection,
    workspace: WorkspaceId,
    definition: Value,
) -> Result<NewSegmentVersion, ApiError> {
    let (catalog, storage) =
        catalog_of(zirv_flow_store::segment_catalog(connection, workspace).await?);
    let now = zirv_flow_store::database_now(&mut *connection).await?;
    let parsed = parse_and_validate(&definition, &catalog).map_err(issues_error)?;
    let compiled = compile(&parsed, &catalog, &storage, now).map_err(issues_error)?;
    Ok(NewSegmentVersion {
        definition,
        normalized: normalize(parsed).to_value(),
        deps: compiled.deps.to_value(),
        time_sensitive: compiled.time_sensitive,
    })
}

/// Creates a segment with its first version and audits it as `segment.created`.
pub(crate) async fn create_segment(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    created_by: Option<UserId>,
    request: CreateSegmentRequest,
) -> Result<(Segment, SegmentVersion), ApiError> {
    let name = check_name(&request.name)?.to_owned();
    check_description(&request.description)?;
    let cadence = request
        .cadence_minutes
        .map_or(Ok(DEFAULT_CADENCE_MINUTES), check_cadence)?;
    let mut tx = zirv_flow_store::begin(pool).await?;
    zirv_flow_store::lock_segments(&mut tx, workspace).await?;
    let content = prepare(&mut tx, workspace, request.definition).await?;
    let new = NewSegment {
        id: Uuid::now_v7(),
        name,
        description: request.description,
        cadence_minutes: cadence,
    };
    let (segment, version) = zirv_flow_store::create_segment(
        &mut tx,
        workspace,
        &new,
        &content,
        created_by,
        &request_trace(),
    )
    .await
    .map_err(name_taken)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "segment.created",
        Some(("segment", segment.id)),
        json!({ "version": version.version }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((segment, version))
}

/// Changes a segment and audits the changed fields as `segment.updated`, with the new version
/// when a definition was saved.
pub(crate) async fn update_segment(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    updated_by: Option<UserId>,
    request: UpdateSegmentRequest,
) -> Result<(Segment, SegmentVersion), ApiError> {
    let mut fields: Vec<&'static str> = Vec::new();
    let name = request
        .name
        .as_deref()
        .map(check_name)
        .transpose()?
        .map(str::to_owned);
    if name.is_some() {
        fields.push("name");
    }
    if let Some(description) = &request.description {
        check_description(description)?;
        fields.push("description");
    }
    let cadence = request.cadence_minutes.map(check_cadence).transpose()?;
    if cadence.is_some() {
        fields.push("cadence_minutes");
    }
    if request.definition.is_some() {
        fields.push("definition");
    }
    if fields.is_empty() {
        return Err(invalid("body", "must change at least one field"));
    }
    let mut tx = zirv_flow_store::begin(pool).await?;
    zirv_flow_store::lock_segments(&mut tx, workspace).await?;
    let version = match request.definition {
        Some(definition) => Some(prepare(&mut tx, workspace, definition).await?),
        None => None,
    };
    let patch = SegmentPatch {
        name,
        description: request.description,
        cadence_minutes: cadence,
        version,
    };
    let (segment, saved) = zirv_flow_store::update_segment(
        &mut tx,
        workspace,
        id,
        &patch,
        updated_by,
        &request_trace(),
    )
    .await
    .map_err(segment_error)?;
    let current = match saved {
        Some(saved) => saved,
        None => {
            zirv_flow_store::find_segment_version(&mut *tx, workspace, id, segment.current_version)
                .await?
                .ok_or_else(ApiError::internal)?
        }
    };
    let mut summary = json!({ "fields": fields });
    if patch.version.is_some() {
        summary["version"] = json!(segment.current_version);
    }
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "segment.updated",
        Some(("segment", segment.id)),
        summary,
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((segment, current))
}

/// Archives a segment and audits it as `segment.archived`.
pub(crate) async fn archive_segment(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<(Segment, SegmentVersion), ApiError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    zirv_flow_store::lock_segments(&mut tx, workspace).await?;
    let segment = zirv_flow_store::archive_segment(&mut tx, workspace, id)
        .await
        .map_err(segment_error)?;
    let current =
        zirv_flow_store::find_segment_version(&mut *tx, workspace, id, segment.current_version)
            .await?
            .ok_or_else(ApiError::internal)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "segment.archived",
        Some(("segment", segment.id)),
        json!({ "version": segment.current_version }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((segment, current))
}

/// The workspace's live segments by name; archived ones with `archived=true`.
#[utoipa::path(
    get,
    operation_id = "list_segments",
    path = "/api/v1/workspaces/{ws}/segments",
    tag = "segments",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("archived" = Option<bool>, Query, description = "Include archived segments; default false")
    ),
    responses(
        (status = 200, description = "The segments", body = Vec<SegmentBody>),
        (status = 400, description = "archived is not true or false", body = Problem, content_type = "application/problem+json"),
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
    RawQuery(query): RawQuery,
) -> Result<Json<Vec<SegmentBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let archived = bool_param(query.as_deref().unwrap_or_default(), "archived")?;
    let segments = zirv_flow_store::list_segments(&state.pool, workspace.id, archived).await?;
    Ok(Json(segments.iter().map(SegmentBody::from).collect()))
}

/// Creates a segment with its first version. The definition is checked against the workspace's
/// traits, events, lists, segments and consent topics. Audited as `segment.created`.
#[utoipa::path(
    post,
    operation_id = "create_segment",
    path = "/api/v1/workspaces/{ws}/segments",
    tag = "segments",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateSegmentRequest,
    responses(
        (status = 201, description = "The new segment with its first version", body = SegmentDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A live segment has that name", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name, description or cadence, or a definition that does not validate (`code` is `segment_invalid`, with `issues`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateSegmentRequest>,
) -> Result<(StatusCode, Json<SegmentDetailBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Edit).await?;
    let (segment, version) = create_segment(
        &state.pool,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        body,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(SegmentDetailBody::new(&segment, &version)),
    ))
}

/// One segment with its current version, its dependencies and whether it is time-sensitive.
#[utoipa::path(
    get,
    operation_id = "get_segment",
    path = "/api/v1/workspaces/{ws}/segments/{id}",
    tag = "segments",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Segment id")
    ),
    responses(
        (status = 200, description = "The segment", body = SegmentDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or segment, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<SegmentDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id = path_id(&id)?;
    let segment = zirv_flow_store::find_segment(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let version = zirv_flow_store::find_segment_version(
        &state.pool,
        workspace.id,
        id,
        segment.current_version,
    )
    .await?
    .ok_or_else(ApiError::internal)?;
    Ok(Json(SegmentDetailBody::new(&segment, &version)))
}

/// Changes a segment. A definition is saved as the next version and becomes the current one,
/// after the references are checked for a cycle. Audited as `segment.updated`.
#[utoipa::path(
    patch,
    operation_id = "update_segment",
    path = "/api/v1/workspaces/{ws}/segments/{id}",
    tag = "segments",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Segment id")
    ),
    request_body = UpdateSegmentRequest,
    responses(
        (status = 200, description = "The updated segment with its current version", body = SegmentDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or segment, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Another live segment has that name, or the segment is archived (`code` is `segment_archived`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid field, no field at all, a definition that does not validate (`segment_invalid`, with `issues`), or a definition that closes a reference cycle (`segment_cycle`, with `cycle`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateSegmentRequest>,
) -> Result<Json<SegmentDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Edit).await?;
    let (segment, version) = update_segment(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        principal.user_id(),
        body,
    )
    .await?;
    Ok(Json(SegmentDetailBody::new(&segment, &version)))
}

/// Archives a segment; refused while a current version of another live segment references it.
/// Versions stay readable. Audited as `segment.archived`.
#[utoipa::path(
    post,
    operation_id = "archive_segment",
    path = "/api/v1/workspaces/{ws}/segments/{id}/archive",
    tag = "segments",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Segment id")
    ),
    responses(
        (status = 200, description = "The archived segment", body = SegmentDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or segment, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Another segment references it (`code` is `segment_referenced`), or it is already archived (`segment_archived`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn archive(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<SegmentDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Edit).await?;
    let (segment, version) = archive_segment(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
    )
    .await?;
    Ok(Json(SegmentDetailBody::new(&segment, &version)))
}

/// The versions of a segment, newest first.
#[utoipa::path(
    get,
    operation_id = "list_segment_versions",
    path = "/api/v1/workspaces/{ws}/segments/{id}/versions",
    tag = "segments",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Segment id")
    ),
    responses(
        (status = 200, description = "The versions", body = Vec<SegmentVersionBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or segment, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn versions(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<Vec<SegmentVersionBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id = path_id(&id)?;
    zirv_flow_store::find_segment(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let versions = zirv_flow_store::list_segment_versions(&state.pool, workspace.id, id).await?;
    Ok(Json(
        versions.iter().map(SegmentVersionBody::from).collect(),
    ))
}

/// One version of a segment.
#[utoipa::path(
    get,
    operation_id = "get_segment_version",
    path = "/api/v1/workspaces/{ws}/segments/{id}/versions/{n}",
    tag = "segments",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Segment id"),
        ("n" = i32, Path, description = "Version number")
    ),
    responses(
        (status = 200, description = "The version", body = SegmentVersionBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, segment or version, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn version(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id, n)): Path<(String, String, String)>,
) -> Result<Json<SegmentVersionBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let version = zirv_flow_store::find_segment_version(
        &state.pool,
        workspace.id,
        path_id(&id)?,
        path_id(&n)?,
    )
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(SegmentVersionBody::from(&version)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_trimmed_and_bounded_to_100_characters() {
        assert_eq!(check_name("  Pro ").unwrap(), "Pro");
        assert!(check_name("   ").is_err());
        assert!(check_name(&"x".repeat(100)).is_ok());
        assert!(check_name(&"x".repeat(101)).is_err());
        assert!(check_name("two\nlines").is_err());
    }

    #[test]
    fn a_description_may_be_empty_but_not_long() {
        assert!(check_description("").is_ok());
        assert!(check_description(&"x".repeat(500)).is_ok());
        assert!(check_description(&"x".repeat(501)).is_err());
    }

    #[test]
    fn a_cadence_outside_the_ruled_set_is_refused_and_each_member_of_it_is_accepted() {
        assert!(check_cadence(7).is_err());
        assert!(check_cadence(0).is_err());
        assert!(check_cadence(10081).is_err());
        for minutes in CADENCE_MINUTES {
            assert_eq!(check_cadence(minutes).unwrap(), minutes);
        }
        assert!(CADENCE_MINUTES.contains(&DEFAULT_CADENCE_MINUTES));
    }

    #[test]
    fn the_cadence_schema_lists_exactly_the_allowed_values() {
        let schema = serde_json::to_value(cadence_schema()).unwrap();

        assert_eq!(schema["type"], "integer");
        assert_eq!(schema["enum"], json!(CADENCE_MINUTES));
    }

    #[test]
    fn a_create_request_needs_a_name_and_a_definition_and_defaults_the_rest() {
        let request: CreateSegmentRequest =
            serde_json::from_str(r#"{"name":"A","definition":{"v":1}}"#).unwrap();

        assert_eq!(
            (request.description.as_str(), request.cadence_minutes),
            ("", None)
        );
        assert!(serde_json::from_str::<CreateSegmentRequest>(r#"{"name":"A"}"#).is_err());
        assert!(serde_json::from_str::<CreateSegmentRequest>(r#"{"definition":{}}"#).is_err());
    }
}
