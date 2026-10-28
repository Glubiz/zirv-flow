//! Previewing a mapping: reads the first records of a stream from the source and shows what the
//! mapping makes of them, with the same `core::mapping` function a sync run uses. Nothing is
//! written. The answer holds the values of the sample, so only workspace admins may ask, as for
//! discovery and a connection test; nothing of it reaches a log, a metric or the audit trail.

use hashbrown::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_connectors::sources::Record;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::mapping::{self, JsonRecord, Mapping, TraitState};
use zirv_flow_store::Stream;

use super::actions::read_stream;
use super::streams::{checked_mapping, descriptor_of, saved_mapping_body, trait_catalog};
use super::{invalid, is_push};
use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::imports::MappingBody;
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

/// The records a preview shows when it is not asked for a number.
const DEFAULT_LIMIT: u32 = 10;
/// The most records a preview shows.
const MAX_LIMIT: u32 = 20;
/// The most bytes a pasted sample may hold, in all.
const MAX_SAMPLE_BYTES: usize = 64 * 1024;

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
    /// For a webhook stream, which has no source to read: 1 to 20 JSON objects (64 KiB in all)
    /// to map as the drain would. Required for a webhook stream and refused for any other.
    #[serde(default)]
    #[schema(value_type = Option<Vec<std::collections::BTreeMap<String, serde_json::Value>>>)]
    sample: Option<Vec<Value>>,
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
    #[schema(format = DateTime)]
    occurred_at: Option<String>,
    #[schema(value_type = Object)]
    properties: Map<String, Value>,
}

/// One record of the sample and what the mapping makes of it.
#[derive(Serialize, ToSchema)]
pub(crate) struct PreviewRecord {
    /// The record's key within the stream.
    key: String,
    /// A run would remove the record: the source reports it deleted or the mapping's delete signal
    /// holds.
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
    declared: &[&str],
    catalog: &HashMap<String, TraitState>,
    record: &Record,
) -> PreviewRecord {
    let empty = |result: &str| PreviewRecord {
        key: record.key.clone(),
        deleted: false,
        result: result.to_owned(),
        identifiers: Vec::new(),
        traits: Map::new(),
        lists: Vec::new(),
        event: None,
        errors: Vec::new(),
    };
    let cells = JsonRecord::from_value_for_mapping(&record.data, mapping, declared.iter().copied());
    if mapping::deletion(mapping, &cells, record.deleted).is_some() {
        return PreviewRecord {
            deleted: true,
            ..empty("deleted")
        };
    }
    match mapping::apply(mapping, &cells, catalog) {
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
    let source = zirv_flow_store::find_source(&state.pool, workspace.id, source_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let push = is_push(&source.kind);
    match (push, &request.sample) {
        (true, None) => {
            return Err(ApiError::unprocessable(
                "sample: a webhook stream has no source to read; send a sample payload",
            )
            .with_code("sample_required"));
        }
        (false, Some(_)) => {
            return Err(ApiError::unprocessable(
                "sample: only a webhook stream previews a sample payload",
            )
            .with_code("sample_not_supported"));
        }
        _ => {}
    }
    let draft = match request.mapping {
        Some(body) => body,
        None => saved_mapping_body(&stream)
            .ok_or_else(|| ApiError::unprocessable("stream_not_mapped"))?,
    };
    let mapping = checked_mapping(
        &state,
        workspace.id,
        &descriptor,
        &stream.primary_key,
        &draft,
        push,
    )
    .await?;
    let body = preview_mapping(
        &state,
        workspace.id,
        source_id,
        &stream,
        &mapping,
        limit,
        request.sample.as_deref(),
    )
    .await?;
    Ok(Json(body))
}

/// Maps the first `limit` records of `stream` with an already checked `mapping`: the records a
/// pulled source gives from the start of the stream, or the pasted `sample` of a webhook stream.
/// Reads nothing but those records and writes nothing. The answer holds record values, so only
/// a caller that is allowed to see them may be given it.
pub(crate) async fn preview_mapping(
    state: &AppState,
    workspace: WorkspaceId,
    source_id: Uuid,
    stream: &Stream,
    mapping: &Mapping,
    limit: u32,
    sample: Option<&[Value]>,
) -> Result<PreviewBody, ApiError> {
    let catalog = trait_catalog(state, workspace).await?;

    if let Some(sample) = sample {
        let objects = check_sample(sample)?;
        let declared = mapping.root_fields();
        let shown: Vec<PreviewRecord> = objects
            .iter()
            .enumerate()
            .map(|(position, data)| {
                let record = Record {
                    key: (position + 1).to_string(),
                    data: data.clone(),
                    deleted: false,
                    occurrence: None,
                };
                preview_record(mapping, &declared, &catalog, &record)
            })
            .collect();
        return Ok(PreviewBody {
            read: u32::try_from(shown.len()).unwrap_or(u32::MAX),
            records: shown,
            truncated: false,
        });
    }

    let descriptor = descriptor_of(stream)?;
    let (records, truncated) =
        read_stream(state, workspace, source_id, stream, limit as usize).await?;
    let declared: Vec<&str> = descriptor
        .fields
        .iter()
        .map(|field| field.name.as_str())
        .collect();
    let shown: Vec<PreviewRecord> = records
        .iter()
        .map(|record| preview_record(mapping, &declared, &catalog, record))
        .collect();
    Ok(PreviewBody {
        read: u32::try_from(shown.len()).unwrap_or(u32::MAX),
        records: shown,
        truncated,
    })
}

/// The sample payloads of a preview: 1 to 20 JSON objects, 64 KiB in all.
pub(crate) fn check_sample(sample: &[Value]) -> Result<&[Value], ApiError> {
    if !(1..=MAX_LIMIT as usize).contains(&sample.len()) {
        return Err(invalid("sample", "must hold 1 to 20 objects"));
    }
    if sample.iter().any(|item| !item.is_object()) {
        return Err(invalid("sample", "must hold only JSON objects"));
    }
    let size: usize = sample
        .iter()
        .map(|item| serde_json::to_vec(item).map_or(usize::MAX, |bytes| bytes.len()))
        .fold(0, usize::saturating_add);
    if size > MAX_SAMPLE_BYTES {
        return Err(invalid("sample", "must be at most 64 KiB"));
    }
    Ok(sample)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use zirv_flow_core::people::TraitType;

    use super::*;

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
            occurrence: None,
        }
    }

    #[test]
    fn a_record_shows_its_identifiers_traits_lists_and_event() {
        let shown = preview_record(
            &mapping(),
            &["profile", "order"],
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
            &["profile", "order"],
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
            &["profile", "order"],
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
