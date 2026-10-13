//! The streams of a source: the list, and the settings the user chooses for each. Settings are
//! checked against the descriptor discovery last reported, so a stream cannot be set up in a way
//! its source cannot serve.

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
use zirv_flow_store::{Stream, StreamSettings};

use super::problems;
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
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
    /// How long without new data before the stream counts as stale, in seconds.
    staleness_threshold_seconds: Option<i32>,
    discovered: DescriptorBody,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl TryFrom<&Stream> for StreamBody {
    type Error = ApiError;

    fn try_from(stream: &Stream) -> Result<Self, ApiError> {
        let descriptor = descriptor_of(stream)?;
        Ok(Self {
            id: stream.id,
            name: stream.name.clone(),
            enabled: stream.enabled,
            sync_mode: stream.sync_mode.clone(),
            cursor_field: stream.cursor_field.clone(),
            primary_key: stream.primary_key.clone(),
            staleness_threshold_seconds: stream.staleness_threshold_seconds,
            discovered: DescriptorBody::from(&descriptor),
            updated_at: stream.updated_at,
        })
    }
}

pub(super) fn bodies(streams: &[Stream]) -> Result<Vec<StreamBody>, ApiError> {
    streams.iter().map(StreamBody::try_from).collect()
}

fn descriptor_of(stream: &Stream) -> Result<StreamDescriptor, ApiError> {
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
    let mode = check_settings(&descriptor_of(&existing)?, request)?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let stream = zirv_flow_store::configure_stream(
        &mut *tx,
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
    Ok(Json(bodies(&streams)?))
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
        (status = 422, description = "Settings the stream cannot serve, such as incremental without a cursor field", body = Problem, content_type = "application/problem+json")
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
    let stream = configure(
        &state,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        &name,
        &body,
    )
    .await?;
    Ok(Json(StreamBody::try_from(&stream)?))
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
