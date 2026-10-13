//! The streams of a source: the list, and the settings the user chooses for each. Settings are
//! checked against the descriptor discovery last reported, so a stream cannot be set up in a way
//! its source cannot serve.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_connectors::sources::{
    StreamDescriptor, StreamSettingsInput, SyncMode, validate_stream_settings,
};
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::mapping::{self, Header, Mapping, MappingError, TraitState};
use zirv_flow_store::{Stream, StreamFreshness, StreamSettings};

use super::problems;
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::imports::MappingBody;
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct FieldBody {
    name: String,
    /// One of `string`, `number`, `boolean`, `timestamp`, `json`, `unknown`.
    field_type: String,
}

/// What discovery last reported about a stream.
#[derive(Serialize, ToSchema)]
pub(crate) struct DescriptorBody {
    fields: Vec<FieldBody>,
    /// The sync modes the stream supports: `full_refresh`, `incremental`.
    sync_modes: Vec<String>,
    default_cursor_field: Option<String>,
    default_primary_key: Vec<String>,
}

impl From<&StreamDescriptor> for DescriptorBody {
    fn from(descriptor: &StreamDescriptor) -> Self {
        Self {
            fields: descriptor
                .fields
                .iter()
                .map(|field| FieldBody {
                    name: field.name.clone(),
                    field_type: serde_json::to_value(field.field_type)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .unwrap_or_else(|| "unknown".to_owned()),
                })
                .collect(),
            sync_modes: descriptor
                .sync_modes
                .iter()
                .map(|mode| mode.as_str().to_owned())
                .collect(),
            default_cursor_field: descriptor.default_cursor_field.clone(),
            default_primary_key: descriptor.default_primary_key.clone(),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct StreamBody {
    id: Uuid,
    name: String,
    /// Whether runs read this stream. New streams start disabled.
    enabled: bool,
    /// `full_refresh` or `incremental`.
    sync_mode: String,
    cursor_field: Option<String>,
    primary_key: Vec<String>,
    /// The stream's own threshold: seconds without a successful run before the stream counts as
    /// stale. Absent means twice the source schedule's interval, or never stale when the source
    /// has no schedule. `freshness.threshold_seconds` is the one in force.
    staleness_threshold_seconds: Option<i32>,
    /// Whether the data is current; null while the stream, or its source, is disabled.
    freshness: Option<FreshnessBody>,
    /// How a record of the stream becomes a person; null until one is set.
    mapping: Option<MappingBody>,
    discovered: DescriptorBody,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl StreamBody {
    fn new(stream: &Stream, freshness: Option<FreshnessBody>) -> Result<Self, ApiError> {
        let descriptor = descriptor_of(stream)?;
        Ok(Self {
            id: stream.id,
            name: stream.name.clone(),
            enabled: stream.enabled,
            sync_mode: stream.sync_mode.clone(),
            cursor_field: stream.cursor_field.clone(),
            primary_key: stream.primary_key.clone(),
            staleness_threshold_seconds: stream.staleness_threshold_seconds,
            freshness,
            mapping: stream.mapping.as_ref().and_then(|value| {
                serde_json::from_value::<Mapping>(value.clone())
                    .ok()
                    .and_then(|mapping| MappingBody::from_core(&mapping))
            }),
            discovered: DescriptorBody::from(&descriptor),
            updated_at: stream.updated_at,
        })
    }
}

/// Whether a stream's data is current (architecture 7.5). The state is derived on every read.
#[derive(Serialize, ToSchema)]
pub(crate) struct FreshnessBody {
    /// `fresh`, `stale` (past the threshold since the last success) or `failing` (the last run
    /// failed).
    #[schema(example = "fresh")]
    state: String,
    /// When the stream last succeeded; null if it never has.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    last_success_at: Option<OffsetDateTime>,
    /// The stream's checkpoint, opaque connector state that may hold a value of a record.
    /// Workspace admins only; null for everyone else, for a full refresh and before the first
    /// batch.
    #[schema(value_type = Option<serde_json::Value>)]
    high_water_mark: Option<serde_json::Value>,
    /// Seconds without a success before the stream counts as stale; null when it never turns
    /// stale (no explicit threshold and no schedule).
    threshold_seconds: Option<i64>,
    /// Whether `threshold_seconds` is the default of twice the schedule interval and not the
    /// stream's own.
    threshold_is_default: bool,
}

impl FreshnessBody {
    fn new(freshness: &StreamFreshness, show_high_water_mark: bool) -> Self {
        Self {
            state: freshness.state.as_str().to_owned(),
            last_success_at: freshness.last_success_at,
            high_water_mark: freshness
                .high_water_mark
                .clone()
                .filter(|_| show_high_water_mark),
            threshold_seconds: freshness.threshold_seconds,
            threshold_is_default: freshness.threshold_is_default,
        }
    }
}

/// The bodies of `streams`, each with its freshness. `show_high_water_mark` is for workspace
/// admins only.
pub(super) async fn bodies(
    state: &AppState,
    workspace: WorkspaceId,
    source: Uuid,
    streams: &[Stream],
    show_high_water_mark: bool,
) -> Result<Vec<StreamBody>, ApiError> {
    let now = zirv_flow_store::database_now(&state.pool).await?;
    let freshness =
        zirv_flow_store::stream_freshness(&state.pool, Some(workspace), Some(source), now).await?;
    let by_id: HashMap<Uuid, &StreamFreshness> = freshness
        .iter()
        .map(|found| (found.stream_id, found))
        .collect();
    streams
        .iter()
        .map(|stream| {
            let found = by_id
                .get(&stream.id)
                .map(|found| FreshnessBody::new(found, show_high_water_mark));
            StreamBody::new(stream, found)
        })
        .collect()
}

pub(super) fn descriptor_of(stream: &Stream) -> Result<StreamDescriptor, ApiError> {
    serde_json::from_value(stream.discovered.clone()).map_err(|_| ApiError::internal())
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct StreamSettingsRequest {
    enabled: bool,
    /// `full_refresh` or `incremental`; one the stream supports.
    #[schema(example = "incremental")]
    sync_mode: String,
    /// A field of the stream. Required for `incremental`.
    cursor_field: Option<String>,
    /// Fields of the stream that identify a record.
    #[serde(default)]
    primary_key: Vec<String>,
    /// A positive number of seconds.
    staleness_threshold_seconds: Option<i32>,
    /// How a record of the stream becomes a person. Absent leaves the stream's mapping as it is.
    /// Columns are the stream's field names (top level); trait keys must be in the registry.
    #[serde(default)]
    mapping: Option<MappingBody>,
}

/// Checks the settings against the descriptor. Returns the sync mode on success.
fn check_settings(
    descriptor: &StreamDescriptor,
    request: &StreamSettingsRequest,
) -> Result<SyncMode, ApiError> {
    validate_stream_settings(
        descriptor,
        &StreamSettingsInput {
            sync_mode: &request.sync_mode,
            cursor_field: request.cursor_field.as_deref(),
            primary_key: &request.primary_key,
            staleness_threshold_seconds: request.staleness_threshold_seconds,
        },
    )
    .map_err(|errors| {
        problems(
            errors
                .into_iter()
                .map(|error| (error.field, error.reason))
                .collect(),
        )
    })
}

/// The mapping problem as one line: its code, and the column when it is on one.
fn mapping_problem(error: &MappingError) -> ApiError {
    let detail = match error.column() {
        Some(column) => format!("mapping: {} (column \"{column}\")", error.code()),
        None => format!("mapping: {}", error.code()),
    };
    ApiError::unprocessable(detail)
}

/// The trait registry as the mapping sees it.
pub(super) async fn trait_catalog(
    state: &AppState,
    workspace: WorkspaceId,
) -> Result<std::collections::HashMap<String, TraitState>, ApiError> {
    Ok(
        zirv_flow_store::list_trait_definitions(&state.pool, workspace)
            .await?
            .into_iter()
            .map(|definition| {
                let trait_state = if definition.archived {
                    TraitState::Archived
                } else {
                    TraitState::Active(definition.trait_type)
                };
                (definition.key, trait_state)
            })
            .collect(),
    )
}

/// The stream's field names as a mapping header. A source can report names a header refuses (a
/// blank or repeated one); that is the source's answer being unusable, not a server fault.
fn descriptor_header(descriptor: &StreamDescriptor) -> Result<Header, ApiError> {
    Header::new(
        descriptor
            .fields
            .iter()
            .map(|field| field.name.clone())
            .collect(),
    )
    .map_err(|error| ApiError::source_fields_invalid(error.code()))
}

/// Checks a mapping against the stream's field names (a column may be a dotted path into a
/// field) and the trait registry, that every list it names exists in the workspace, and that a
/// mapping with an event has a primary key to hang the event's idempotency on.
pub(super) async fn checked_mapping(
    state: &AppState,
    workspace: WorkspaceId,
    descriptor: &StreamDescriptor,
    primary_key: &[String],
    body: &MappingBody,
) -> Result<Mapping, ApiError> {
    let mapping = body.to_core()?;
    let header = descriptor_header(descriptor)?;
    let catalog = trait_catalog(state, workspace).await?;
    mapping::validate_paths(&mapping, &header, &catalog)
        .map_err(|error| mapping_problem(&error))?;
    for rule in &mapping.columns {
        if let mapping::Target::List { list_id } = &rule.target
            && zirv_flow_store::find_list(&state.pool, workspace, *list_id)
                .await?
                .is_none()
        {
            return Err(ApiError::unprocessable(format!(
                "mapping: unknown_list (column \"{}\")",
                rule.column
            )));
        }
    }
    if mapping.has_event() && primary_key.is_empty() {
        return Err(ApiError::unprocessable(
            "mapping: event_requires_primary_key",
        ));
    }
    Ok(mapping)
}

/// Stores the settings of a stream and audits `stream.configured`.
pub(crate) async fn configure(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    source_id: Uuid,
    name: &str,
    request: &StreamSettingsRequest,
) -> Result<Stream, ApiError> {
    let existing = zirv_flow_store::find_stream(&state.pool, workspace, source_id, name)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let descriptor = descriptor_of(&existing)?;
    let mode = check_settings(&descriptor, request)?;
    let mapping = match &request.mapping {
        Some(body) => {
            Some(checked_mapping(state, workspace, &descriptor, &request.primary_key, body).await?)
        }
        None => None,
    };
    // A mapping left as it is must still fit the key this request sets.
    if mapping.is_none()
        && request.primary_key.is_empty()
        && existing
            .mapping
            .as_ref()
            .and_then(|value| serde_json::from_value::<Mapping>(value.clone()).ok())
            .is_some_and(|saved| saved.has_event())
    {
        return Err(ApiError::unprocessable(
            "mapping: event_requires_primary_key",
        ));
    }
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let stream = zirv_flow_store::configure_stream(
        &mut tx,
        workspace,
        source_id,
        name,
        &StreamSettings {
            enabled: request.enabled,
            sync_mode: mode.as_str(),
            cursor_field: request.cursor_field.as_deref(),
            primary_key: &request.primary_key,
            staleness_threshold_seconds: request.staleness_threshold_seconds,
        },
    )
    .await?
    .ok_or_else(ApiError::not_found)?;
    let stream = match &mapping {
        Some(mapping) => {
            let value = serde_json::to_value(mapping).map_err(|_| ApiError::internal())?;
            zirv_flow_store::set_stream_mapping(&mut *tx, workspace, source_id, name, &value)
                .await?
                .ok_or_else(ApiError::not_found)?
        }
        None => stream,
    };
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "stream.configured",
        Some(("stream", stream.id)),
        json!({
            "source_id": source_id,
            "name": stream.name,
            "enabled": stream.enabled,
            "sync_mode": stream.sync_mode,
            "mapping_set": mapping.is_some(),
            "staleness_threshold_seconds": stream.staleness_threshold_seconds,
        }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(stream)
}

/// The streams of a source, ordered by name.
#[utoipa::path(
    get,
    operation_id = "list_source_streams",
    path = "/api/v1/workspaces/{ws}/sources/{id}/streams",
    tag = "sources",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Source id")
    ),
    responses(
        (status = 200, description = "The streams", body = Vec<StreamBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or source, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<Vec<StreamBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let source = path_id(&id)?;
    zirv_flow_store::find_source(&state.pool, workspace.id, source)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let streams = zirv_flow_store::list_streams(&state.pool, workspace.id, source).await?;
    let admin = may_see_values(&state, &principal, workspace.id).await;
    Ok(Json(
        bodies(&state, workspace.id, source, &streams, admin).await?,
    ))
}

/// Sets whether a stream is read and how: sync mode, cursor field, primary key and staleness
/// threshold. Workspace admins only. Audited as `stream.configured`.
#[utoipa::path(
    put,
    operation_id = "configure_source_stream",
    path = "/api/v1/workspaces/{ws}/sources/{id}/streams/{name}",
    tag = "sources",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Source id"),
        ("name" = String, Path, description = "Stream name")
    ),
    request_body = StreamSettingsRequest,
    responses(
        (status = 200, description = "The stream with its new settings", body = StreamBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, source or stream, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A change of the sync mode, cursor field or primary key while the stream has a queued or running run (`code` is `sync_run_active`); wait for the run or cancel it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Settings the stream cannot serve, such as incremental without a cursor field, or a mapping that names a column the stream does not have", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn put(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id, name)): Path<(String, String, String)>,
    ApiJson(body): ApiJson<StreamSettingsRequest>,
) -> Result<Json<StreamBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let source = path_id(&id)?;
    let stream = configure(
        &state,
        &principal.context(),
        workspace.id,
        source,
        &name,
        &body,
    )
    .await?;
    // Only an admin gets here, so the high-water mark is shown.
    let mut bodies = bodies(&state, workspace.id, source, &[stream], true).await?;
    Ok(Json(bodies.remove(0)))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use zirv_flow_connectors::sources::{FieldDescriptor, FieldType};

    use super::*;

    fn descriptor() -> StreamDescriptor {
        StreamDescriptor {
            name: "contacts".into(),
            fields: ["id", "updated_at"]
                .into_iter()
                .map(|name| FieldDescriptor {
                    name: name.into(),
                    field_type: FieldType::String,
                })
                .collect(),
            sync_modes: vec![SyncMode::FullRefresh, SyncMode::Incremental],
            default_cursor_field: Some("updated_at".into()),
            default_primary_key: vec!["id".into()],
        }
    }

    fn request(mode: &str, cursor: Option<&str>) -> StreamSettingsRequest {
        StreamSettingsRequest {
            enabled: true,
            sync_mode: mode.into(),
            cursor_field: cursor.map(str::to_owned),
            primary_key: vec!["id".into()],
            staleness_threshold_seconds: Some(60),
            mapping: None,
        }
    }

    #[test]
    fn valid_settings_return_the_sync_mode() {
        let mode = check_settings(&descriptor(), &request("incremental", Some("updated_at")));

        assert_eq!(mode.unwrap(), SyncMode::Incremental);
        assert!(check_settings(&descriptor(), &request("full_refresh", None)).is_ok());
    }

    #[test]
    fn incremental_without_a_cursor_is_refused_with_a_reason() {
        let error = check_settings(&descriptor(), &request("incremental", None)).unwrap_err();

        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            error.detail(),
            "cursor_field is required for an incremental stream"
        );
    }

    #[test]
    fn every_broken_rule_is_reported_together() {
        let mut bad = request("weekly", Some("nope"));
        bad.primary_key = vec!["missing".into()];
        bad.staleness_threshold_seconds = Some(0);

        let error = check_settings(&descriptor(), &bad).unwrap_err();

        assert_eq!(
            error.detail(),
            "sync_mode must be full_refresh or incremental; \
             cursor_field is not a field of this stream; \
             primary_key names a field this stream does not have; \
             staleness_threshold_seconds must be positive"
        );
    }

    #[test]
    fn a_sync_mode_the_stream_does_not_support_is_refused() {
        let mut only_full = descriptor();
        only_full.sync_modes = vec![SyncMode::FullRefresh];

        let error = check_settings(&only_full, &request("incremental", Some("updated_at")));

        assert_eq!(
            error.unwrap_err().detail(),
            "sync_mode is not supported by this stream"
        );
    }

    #[test]
    fn a_descriptor_body_names_types_and_modes_as_text() {
        let body = DescriptorBody::from(&descriptor());

        let json = serde_json::to_value(body).unwrap();

        assert_eq!(json["fields"][0]["field_type"], "string");
        assert_eq!(json["sync_modes"], json!(["full_refresh", "incremental"]));
        assert_eq!(json["default_primary_key"], json!(["id"]));
    }
}
