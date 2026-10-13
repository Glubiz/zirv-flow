//! Previewing a mapping: reads the first records of a stream from the source and shows what the
//! mapping makes of them, with the same `core::mapping` function a sync run uses. Nothing is
//! written. The answer holds the values of the sample, so only workspace admins may ask, as for
//! discovery and a connection test; nothing of it reaches a log, a metric or the audit trail.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_connectors::sources::{Record, StreamConfig, StreamDescriptor, SyncMode};
use zirv_flow_core::mapping::{self, Mapping, TraitState};

use super::actions::{CALL_TIMEOUT, prepare, timed_out, upstream};
use super::invalid;
use super::streams::{checked_mapping, descriptor_of, trait_catalog};
use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::imports::MappingBody;
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

/// The records a preview shows when it is not asked for a number.
const DEFAULT_LIMIT: u32 = 10;
/// The most records a preview shows.
const MAX_LIMIT: u32 = 20;

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreviewRequest {
    /// A draft mapping to try. It is checked exactly as a save is. Absent, the stream's saved
    /// mapping is used.
    #[serde(default)]
    mapping: Option<MappingBody>,
    /// How many records to show, 1 to 20; 10 when absent.
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct PreviewIdentifier {
    kind: String,
    value: String,
}

/// Why the mapping refused a record: the field and the rule, never the value.
#[derive(Serialize, ToSchema)]
pub(crate) struct PreviewError {
    field: String,
    reason: String,
}

/// The event a record would yield.
#[derive(Serialize, ToSchema)]
pub(crate) struct PreviewEvent {
    name: String,
    /// When the event happened, when the mapping says; otherwise the run's start decides.
    occurred_at: Option<String>,
    #[schema(value_type = Object)]
    properties: Map<String, Value>,
}

/// One record of the sample and what the mapping makes of it.
#[derive(Serialize, ToSchema)]
pub(crate) struct PreviewRecord {
    /// The record's key within the stream.
    key: String,
    /// The source reports the record as deleted.
    deleted: bool,
    /// `mapped`, `rejected` or `deleted`.
    result: String,
    identifiers: Vec<PreviewIdentifier>,
    #[schema(value_type = Object)]
    traits: Map<String, Value>,
    /// The lists the person would join.
    lists: Vec<Uuid>,
    event: Option<PreviewEvent>,
    errors: Vec<PreviewError>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct PreviewBody {
    records: Vec<PreviewRecord>,
    /// How many records were mapped.
    read: u32,
    /// The stream has more records than the preview shows.
    truncated: bool,
}

/// Maps one record the way a sync run does.
fn preview_record(
    mapping: &Mapping,
    descriptor: &StreamDescriptor,
    catalog: &HashMap<String, TraitState>,
    record: &Record,
) -> PreviewRecord {
    let empty = |result: &str| PreviewRecord {
        key: record.key.clone(),
        deleted: record.deleted,
        result: result.to_owned(),
        identifiers: Vec::new(),
        traits: Map::new(),
        lists: Vec::new(),
        event: None,
        errors: Vec::new(),
    };
    if record.deleted {
        return empty("deleted");
    }
    let declared = descriptor.fields.iter().map(|field| field.name.as_str());
    match mapping::map_json(mapping, &record.data, declared, catalog) {
        Ok(mapped) => PreviewRecord {
            identifiers: mapped
                .identifiers
                .into_iter()
                .map(|(kind, value)| PreviewIdentifier { kind, value })
                .collect(),
            traits: mapped.traits,
            lists: mapped.lists,
            event: mapped.event.map(|event| PreviewEvent {
                name: event.name,
                occurred_at: event.occurred_at.and_then(|at| at.format(&Rfc3339).ok()),
                properties: event.properties,
            }),
            ..empty("mapped")
        },
        Err(errors) => PreviewRecord {
            errors: errors
                .into_iter()
                .map(|error| PreviewError {
                    field: error.column,
                    reason: error.reason.as_str().to_owned(),
                })
                .collect(),
            ..empty("rejected")
        },
    }
}

/// Shows what a mapping makes of the first records of a stream. Reads from the start of the
/// stream, never from a checkpoint, and writes nothing. The saved mapping is used unless the
/// body brings a draft, which is checked as a save is. Workspace admins only; not audited, since
/// it changes nothing.
#[utoipa::path(
    post,
    operation_id = "preview_stream_mapping",
    path = "/api/v1/workspaces/{ws}/sources/{id}/streams/{name}/preview",
    tag = "sources",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Source id"),
        ("name" = String, Path, description = "Stream name")
    ),
    request_body = PreviewRequest,
    responses(
        (status = 200, description = "The sample records and what the mapping makes of them", body = PreviewBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, source or stream, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The source type is not in this build", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid draft mapping (the same codes as a save), a limit outside 1 to 20, no mapping to preview (`stream_not_mapped`), or a source that reported a blank or repeated field name (`code` is `blank_header` or `duplicate_header`)", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The source refused or failed the request", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to open the credential", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "The source did not answer in time", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn preview(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id, name)): Path<(String, String, String)>,
    ApiJson(request): ApiJson<PreviewRequest>,
) -> Result<Json<PreviewBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let source_id = path_id(&id)?;
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(invalid("limit", "must be 1 to 20"));
    }
    let stream = zirv_flow_store::find_stream(&state.pool, workspace.id, source_id, &name)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let descriptor = descriptor_of(&stream)?;
    let draft = match request.mapping {
        Some(body) => body,
        None => stream
            .mapping
            .as_ref()
            .and_then(|value| serde_json::from_value::<Mapping>(value.clone()).ok())
            .and_then(|saved| MappingBody::from_core(&saved))
            .ok_or_else(|| ApiError::unprocessable("stream_not_mapped"))?,
    };
    let mapping = checked_mapping(
        &state,
        workspace.id,
        &descriptor,
        &stream.primary_key,
        &draft,
    )
    .await?;
    let catalog = trait_catalog(&state, workspace.id).await?;

    // The credential is opened right before the read and dropped with it.
    let call = prepare(&state, workspace.id, source_id).await?;
    let config = StreamConfig {
        name: stream.name.clone(),
        sync_mode: SyncMode::FullRefresh,
        cursor_field: None,
        primary_key: stream.primary_key.clone(),
    };
    let wanted = limit as usize;
    let read = async {
        let mut batches = call.driver.read(
            &call.context,
            &call.source.config,
            &call.credential,
            &config,
            None,
        );
        let mut records: Vec<Record> = Vec::new();
        // One more than shown, to know whether the stream has more.
        while records.len() <= wanted {
            match batches.next().await {
                Some(Ok(batch)) => records.extend(batch.records),
                Some(Err(error)) => return Err(error),
                None => break,
            }
        }
        Ok(records)
    };
    let mut records = tokio::time::timeout(CALL_TIMEOUT, read)
        .await
        .map_err(|_| timed_out())?
        .map_err(upstream)?;
    let truncated = records.len() > wanted;
    records.truncate(wanted);
    let shown: Vec<PreviewRecord> = records
        .iter()
        .map(|record| preview_record(&mapping, &descriptor, &catalog, record))
        .collect();
    Ok(Json(PreviewBody {
        read: u32::try_from(shown.len()).unwrap_or(u32::MAX),
        records: shown,
        truncated,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_connectors::sources::{FieldDescriptor, FieldType};
    use zirv_flow_core::people::TraitType;

    use super::*;

    fn descriptor() -> StreamDescriptor {
        StreamDescriptor {
            name: "records".into(),
            fields: ["profile", "order"]
                .into_iter()
                .map(|name| FieldDescriptor {
                    name: name.into(),
                    field_type: FieldType::Json,
                })
                .collect(),
            sync_modes: vec![SyncMode::FullRefresh],
            default_cursor_field: None,
            default_primary_key: Vec::new(),
        }
    }

    fn mapping() -> Mapping {
        serde_json::from_value(json!({
            "columns": [
                {"column": "profile.email", "target": {"type": "identifier", "kind": "email"}},
                {"column": "profile.name", "target": {"type": "trait", "key": "name"}}
            ],
            "event": {"name": "seen", "properties": [{"property": "sku", "column": "order.sku"}]}
        }))
        .unwrap()
    }

    fn catalog() -> HashMap<String, TraitState> {
        HashMap::from([("name".to_owned(), TraitState::Active(TraitType::String))])
    }

    fn record(data: Value, deleted: bool) -> Record {
        Record {
            key: "k".into(),
            data,
            deleted,
        }
    }

    #[test]
    fn a_record_shows_its_identifiers_traits_lists_and_event() {
        let shown = preview_record(
            &mapping(),
            &descriptor(),
            &catalog(),
            &record(
                json!({"profile": {"email": "A@b.co", "name": "Ada"}, "order": {"sku": "s1"}}),
                false,
            ),
        );

        assert_eq!(shown.result, "mapped");
        assert_eq!(shown.identifiers[0].kind, "email");
        assert_eq!(shown.traits["name"], "Ada");
        let event = shown.event.unwrap();
        assert_eq!(event.name, "seen");
        assert_eq!(event.properties["sku"], "s1");
        assert_eq!(event.occurred_at, None);
    }

    #[test]
    fn a_rejected_record_lists_fields_and_codes_and_no_value() {
        let shown = preview_record(
            &mapping(),
            &descriptor(),
            &catalog(),
            &record(json!({"profile": {"email": "secret-not-an-email"}}), false),
        );

        assert_eq!(shown.result, "rejected");
        assert_eq!(shown.errors[0].field, "profile.email");
        assert_eq!(shown.errors[0].reason, "invalid_identifier");
        let rendered = serde_json::to_string(&shown.errors).unwrap();
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn a_deleted_record_is_shown_as_deleted_and_not_mapped() {
        let shown = preview_record(
            &mapping(),
            &descriptor(),
            &catalog(),
            &record(json!({}), true),
        );

        assert_eq!(shown.result, "deleted");
        assert!(shown.deleted && shown.identifiers.is_empty() && shown.errors.is_empty());
    }

    #[test]
    fn the_request_refuses_unknown_fields() {
        assert!(serde_json::from_value::<PreviewRequest>(json!({"limit": 3})).is_ok());
        assert!(serde_json::from_value::<PreviewRequest>(json!({"records": []})).is_err());
    }
}
