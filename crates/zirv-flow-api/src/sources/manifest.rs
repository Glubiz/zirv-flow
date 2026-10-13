//! Authoring an `http` source's manifest: checking a draft (`validate`) and reading a few raw
//! records from the API with it (`sample`). Neither writes anything and neither is audited; the
//! sample holds record values, so only workspace admins may ask, as for a mapping preview, and
//! nothing of it reaches a log, a metric or the audit trail.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_connectors::manifest;
use zirv_flow_connectors::sources::{StreamConfig, SyncMode, validate_config};

use super::actions::{prepare, read_sample};
use super::{field_problems, invalid};
use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

/// The records a sample shows when it is not asked for a number.
const DEFAULT_LIMIT: u32 = 10;
/// The most records a sample shows.
const MAX_LIMIT: u32 = 20;
/// What the request body may hold beyond the manifest itself.
pub(crate) const ENVELOPE_BYTES: usize = 1024;

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidateRequest {
    /// The draft manifest: any JSON value. Whether it is a valid manifest v1 is the answer.
    #[serde(default)]
    #[schema(value_type = Object)]
    manifest: Value,
}

/// One problem of a manifest: a code and the JSON pointer of the key it concerns. Never a value.
#[derive(Serialize, ToSchema)]
pub(crate) struct IssueBody {
    /// A stable code such as `unknown_field`, `required` or `path_invalid`.
    code: String,
    /// A JSON pointer into the manifest; empty for the manifest itself.
    path: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ManifestFieldBody {
    name: String,
    /// `string`, `number`, `boolean`, `timestamp`, `json` or `unknown`.
    #[serde(rename = "type")]
    field_type: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ManifestStreamBody {
    name: String,
    fields: Vec<ManifestFieldBody>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ValidateBody {
    valid: bool,
    /// At most 50, in the order found; empty when the manifest is valid.
    issues: Vec<IssueBody>,
    /// The streams the manifest declares, for a stream picker; empty when it is invalid.
    streams: Vec<ManifestStreamBody>,
}

fn field_type_name(field_type: zirv_flow_connectors::sources::FieldType) -> String {
    serde_json::to_value(field_type)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The answer for a draft: valid with its streams, or the issues.
pub(crate) fn validated(draft: &Value) -> ValidateBody {
    match manifest::validate(draft) {
        Ok(manifest) => ValidateBody {
            valid: true,
            issues: Vec::new(),
            streams: manifest
                .streams
                .iter()
                .map(|stream| ManifestStreamBody {
                    name: stream.name.clone(),
                    fields: stream
                        .fields
                        .iter()
                        .map(|field| ManifestFieldBody {
                            name: field.name.clone(),
                            field_type: field_type_name(field.field_type),
                        })
                        .collect(),
                })
                .collect(),
        },
        Err(issues) => ValidateBody {
            valid: false,
            issues: issues
                .into_iter()
                .map(|issue| IssueBody {
                    code: issue.code.to_owned(),
                    path: issue.path,
                })
                .collect(),
            streams: Vec::new(),
        },
    }
}

/// Checks a draft manifest. A bad manifest is a 200 with its issues, not a 422, because the editor
/// asks on every pause in typing; saving a source with the same manifest answers 422 with the
/// same codes. Writes nothing and is not audited. Workspace admins only.
#[utoipa::path(
    post,
    operation_id = "validate_source_manifest",
    path = "/api/v1/workspaces/{ws}/source-manifests/validate",
    tag = "sources",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = ValidateRequest,
    responses(
        (status = 200, description = "Whether the manifest is valid, its issues, and its streams", body = ValidateBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is over the manifest size cap (64 KiB) plus 1 KiB", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn validate(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<ValidateRequest>,
) -> Result<Json<ValidateBody>, ApiError> {
    authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    Ok(Json(validated(&body.manifest)))
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SampleRequest {
    /// A draft manifest to try, checked exactly as a save is. Absent, the source's saved
    /// manifest is used.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    manifest: Option<Value>,
    /// The stream to read, by name.
    stream: String,
    /// How many records to read, 1 to 20; 10 when absent.
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SampleBody {
    /// The records exactly as the API returned them (raw values, before any mapping).
    #[schema(value_type = Vec<Object>)]
    records: Vec<Map<String, Value>>,
    /// How many records are returned.
    read: u32,
    /// The stream has more records than the sample shows.
    truncated: bool,
}

/// Whether a draft keeps the saved manifest's base URL and `auth` section as they are.
fn keeps_connection(saved: &Value, draft: &Value) -> bool {
    let saved = saved.get("manifest").unwrap_or(&Value::Null);
    ["base_url", "auth"]
        .iter()
        .all(|key| saved.get(key) == draft.get(key))
}

/// Reads the first records of a stream with a draft or the saved manifest, using the source's
/// stored credential and account tokens, and returns them raw. Reads from the start, never from
/// a checkpoint, and writes nothing. Workspace admins only; not audited, since it changes
/// nothing. A rate-limited or refused call answers as discovery does.
#[utoipa::path(
    post,
    operation_id = "sample_source_manifest",
    path = "/api/v1/workspaces/{ws}/sources/{id}/manifest/sample",
    tag = "sources",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Source id")
    ),
    request_body = SampleRequest,
    responses(
        (status = 200, description = "The raw sample records", body = SampleBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or source, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The source does not use a manifest, its type is not in this build, or a draft changes the saved base URL or authentication (`manifest_draft_auth_changed`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid draft manifest (the same codes as a save), a stream the manifest does not have, or a limit outside 1 to 20", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The API refused or failed the request", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to open the credential", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "The API did not answer in time", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn sample(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(request): ApiJson<SampleRequest>,
) -> Result<Json<SampleBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(invalid("limit", "must be 1 to 20"));
    }
    let id = path_id::<Uuid>(&id)?;
    // Checked before the credential or the account tokens are opened: a draft that moves the base
    // URL or the authentication would send them to an endpoint they were not set up for.
    if let Some(draft) = &request.manifest {
        let saved = zirv_flow_store::find_source(&state.pool, workspace.id, id)
            .await?
            .ok_or_else(ApiError::not_found)?;
        if saved.kind == "http" && !keeps_connection(&saved.config, draft) {
            return Err(ApiError::manifest_draft_auth_changed());
        }
    }
    let call = prepare(&state, workspace.id, id).await?;
    if call.driver.spec().kind != "http" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this source does not use a manifest",
        ));
    }
    // A draft is checked exactly as a save is; the saved settings were checked when saved.
    let config = match request.manifest {
        Some(draft) => {
            let config = json!({ "manifest": draft });
            validate_config(call.driver.spec(), &config)
                .map_err(|errors| field_problems("config", errors))?;
            config
        }
        None => call.source.config.clone(),
    };
    let known = config
        .get("manifest")
        .and_then(|value| manifest::validate(value).ok())
        .and_then(|manifest| {
            manifest
                .streams
                .into_iter()
                .find(|stream| stream.name == request.stream)
        })
        .ok_or_else(|| invalid("stream", "is not in the manifest"))?;
    let stream = StreamConfig {
        name: known.name,
        sync_mode: SyncMode::FullRefresh,
        cursor_field: None,
        primary_key: known.primary_key,
    };
    let (records, truncated) = read_sample(&call, &config, &stream, limit as usize).await?;
    let records: Vec<Map<String, Value>> = records
        .into_iter()
        .filter_map(|record| match record.data {
            Value::Object(object) => Some(object),
            _ => None,
        })
        .collect();
    Ok(Json(SampleBody {
        read: u32::try_from(records.len()).unwrap_or(u32::MAX),
        records,
        truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> Value {
        json!({
            "version": "v1",
            "base_url": "https://api.example.test",
            "auth": {"type": "none"},
            "streams": [{
                "name": "contacts", "path": "/contacts",
                "fields": [{"name": "id", "type": "string"}, {"name": "n", "type": "number"}],
                "primary_key": ["id"]
            }]
        })
    }

    #[test]
    fn a_draft_may_change_streams_but_not_the_base_url_or_the_auth() {
        let saved = json!({"manifest": valid()});
        let keep = |edit: &dyn Fn(&mut Value)| {
            let mut draft = valid();
            edit(&mut draft);
            keeps_connection(&saved, &draft)
        };

        assert!(keep(&|draft| draft["streams"][0]["path"] = json!("/other")));
        assert!(!keep(
            &|draft| draft["base_url"] = json!("https://evil.example.test")
        ));
        assert!(!keep(&|draft| draft["auth"] = json!({"type": "bearer"})));
    }

    #[test]
    fn a_valid_draft_lists_its_streams_and_typed_fields() {
        let body = validated(&valid());

        assert!(body.valid && body.issues.is_empty());
        assert_eq!(body.streams.len(), 1);
        assert_eq!(body.streams[0].name, "contacts");
        let fields: Vec<_> = body.streams[0]
            .fields
            .iter()
            .map(|field| (field.name.as_str(), field.field_type.as_str()))
            .collect();
        assert_eq!(fields, [("id", "string"), ("n", "number")]);
    }

    #[test]
    fn an_invalid_draft_lists_its_issues_by_code_and_path_and_no_streams() {
        let mut draft = valid();
        draft["streams"][0]["path"] = json!("SECRET-no-slash");
        draft["extra"] = json!(1);

        let body = validated(&draft);

        assert!(!body.valid && body.streams.is_empty());
        let issues: Vec<_> = body
            .issues
            .iter()
            .map(|issue| (issue.code.as_str(), issue.path.as_str()))
            .collect();
        assert!(issues.contains(&("path_invalid", "/streams/0/path")));
        assert!(issues.contains(&("unknown_field", "/extra")));
        assert!(
            !serde_json::to_string(&body.issues)
                .unwrap()
                .contains("SECRET")
        );
    }

    #[test]
    fn anything_that_is_not_a_manifest_is_an_invalid_draft_not_an_error() {
        for draft in [json!(null), json!([]), json!("text"), json!(5), json!({})] {
            let body = validated(&draft);
            assert!(!body.valid, "{draft}");
            assert!(!body.issues.is_empty(), "{draft}");
        }
    }
}
