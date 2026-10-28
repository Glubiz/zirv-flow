//! Person exports (architecture section 16.3): request one, list them, and download the
//! gzip-compressed JSON document. The download is the one place personal values leave the
//! system, so it needs the workspace admin role, is audited, and stops working once the
//! configured window (`retention.export_hours`, default 24 hours) is over (410).

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, TraceId, WorkspaceId};
use zirv_flow_store::{ExportDownload, PgPool, PrivacyExport};

use super::{PersonTarget, requested_by, resolve};
use crate::AppState;
use crate::admin::{self, AdminError, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::idempotency::{self, Keyed, ReplayedProblem, RequestHash};
use crate::jobs_admin::{encode_cursor, parse_page};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::trace;

#[derive(Serialize, ToSchema)]
pub(crate) struct ExportBody {
    id: Uuid,
    person_id: Uuid,
    /// `pending`, `ready`, `failed` once the job ran out of attempts or the document passed
    /// the size cap, or `expired`.
    #[schema(example = "ready")]
    status: String,
    /// The user who asked; null for an API key.
    requested_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    requested_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    ready_at: Option<OffsetDateTime>,
    /// The configured window (`retention.export_hours`, default 24 hours) after `ready_at`; the
    /// download answers 410 from then on.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    expires_at: Option<OffsetDateTime>,
    /// The compressed size; null until the export is ready.
    size_bytes: Option<i64>,
    /// The trace of the request and of its job: 32 lowercase hex characters.
    trace_id: String,
    /// Why the export failed, when known: `export_too_large` when the document passed
    /// `privacy.export_max_bytes`. Null otherwise.
    failure_code: Option<String>,
}

impl From<PrivacyExport> for ExportBody {
    fn from(export: PrivacyExport) -> Self {
        Self {
            id: export.id,
            person_id: export.person_id,
            status: export.status.as_str().to_owned(),
            requested_by: export.requested_by,
            requested_at: export.requested_at,
            ready_at: export.ready_at,
            expires_at: export.expires_at,
            size_bytes: export.size_bytes,
            trace_id: TraceId::from_uuid(export.trace_id).to_string(),
            failure_code: export.failure_code,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ExportsBody {
    /// Newest first.
    exports: Vec<ExportBody>,
    /// Pass as `before` for the next page; null when there is none.
    next_before: Option<String>,
}

/// Records an export request for the person the target names and enqueues the job, in one
/// transaction, audited as `privacy.export_requested` with the export id only.
pub(crate) async fn request(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    target: &PersonTarget,
    requested_by: Option<Uuid>,
    trace: &TraceContext,
) -> Result<PrivacyExport, AdminError> {
    let person = resolve(pool, workspace, target).await?;
    let mut tx = zirv_flow_store::begin(pool).await?;
    let export = zirv_flow_store::request_export(&mut tx, workspace, person, requested_by, trace)
        .await?
        .ok_or(AdminError::NotFound("person"))?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "privacy.export_requested",
        Some(("privacy_export", export.id)),
        json!({}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(export)
}

/// What a download is answered with.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Download {
    /// The gzip bytes; the download is audited.
    Content(Vec<u8>),
    Expired,
    NotReady,
}

/// Reads the export's content, and audits the download as `privacy.export_downloaded` with
/// the export id only, in the transaction that reads it. Nothing is returned if the audit
/// fails.
pub(crate) async fn download(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Download, AdminError> {
    match zirv_flow_store::download_export(pool, workspace, id).await? {
        ExportDownload::NotFound => Err(AdminError::NotFound("export")),
        ExportDownload::Expired => Ok(Download::Expired),
        ExportDownload::NotReady => Ok(Download::NotReady),
        ExportDownload::Content(bytes) => {
            let mut tx = zirv_flow_store::begin(pool).await?;
            admin::audit(
                &mut tx,
                context,
                Some(workspace),
                "privacy.export_downloaded",
                Some(("privacy_export", id)),
                json!({}),
            )
            .await?;
            zirv_flow_store::commit(tx).await?;
            Ok(Download::Content(bytes))
        }
    }
}

/// Asks for an export of everything held about a person: profile, identities, ledger
/// contributions, events, consent and its history, lists and segments and their history, merges
/// and identity conflicts, flow enrollments with their steps, waits and entries, the message log
/// and its links, forwarding rows and handled markers, ingest rejects, decision records and saved
/// template previews, with the data of merged people under the survivor. A document past the
/// size cap fails with the code `export_too_large`. A background job
/// builds it; it can be downloaded for the configured window
/// (`retention.export_hours`, default 24 hours) once it is ready. The request is given as a
/// `person_id` or as an `identifier`. Needs the workspace admin role and the CSRF token.
/// Audited as `privacy.export_requested`.
#[utoipa::path(
    post,
    operation_id = "request_export",
    path = "/api/v1/workspaces/{ws}/privacy/exports",
    tag = "privacy",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 255 characters; makes a retry safe for 24 hours")
    ),
    request_body = PersonTarget,
    responses(
        (status = 202, description = "The export, pending until the job has run", body = ExportBody),
        (status = 409, description = "The Idempotency-Key already requested an export (`idempotency_key_replayed`); resource_id is the export's id and the body is not kept, as it names the person", body = ReplayedProblem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or person, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The body names both or neither of person_id and identifier, the identifier is malformed, or the Idempotency-Key is invalid or reused (`idempotency_key_reused`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ApiJson(body): ApiJson<PersonTarget>,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let trace = trace::current().ok_or_else(ApiError::internal)?;
    let request_hash = RequestHash::of_json(&method, &uri, &body)?;
    let keyed = Keyed::of(&headers, "request_export")?;
    let guard = match idempotency::enter(&state.pool, workspace.id, keyed, request_hash).await? {
        Ok(guard) => guard,
        Err(replay) => return Ok(replay),
    };
    let export = request(
        &state.pool,
        &principal.context(),
        workspace.id,
        &body,
        requested_by(&principal),
        &trace,
    )
    .await?;
    idempotency::finish(guard, request_hash, StatusCode::ACCEPTED, export.id, None).await?;
    Ok((StatusCode::ACCEPTED, Json(ExportBody::from(export))).into_response())
}

/// The workspace's exports, newest first, with their status and expiry. The content is never
/// part of the listing.
#[utoipa::path(
    get,
    operation_id = "list_exports",
    path = "/api/v1/workspaces/{ws}/privacy/exports",
    tag = "privacy",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("limit" = Option<u32>, Query, description = "Exports per page, 1 to 200; default 50"),
        ("before" = Option<String>, Query, description = "The next_before of the previous page")
    ),
    responses(
        (status = 200, description = "A page of exports", body = ExportsBody),
        (status = 400, description = "limit is out of range or before is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a key without the admin scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<ExportsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let (limit, before) = parse_page(&query.unwrap_or_default())?;
    let (exports, more) =
        zirv_flow_store::list_privacy_exports(&state.pool, workspace.id, limit, before).await?;
    let next_before = match exports.last() {
        Some(last) if more => Some(encode_cursor((last.requested_at, last.id))),
        _ => None,
    };
    Ok(Json(ExportsBody {
        exports: exports.into_iter().map(ExportBody::from).collect(),
        next_before,
    }))
}

/// Downloads a ready export as gzip-compressed JSON (`zirv-flow.person-export/v1`). It holds
/// personal values, so it needs the workspace admin role, and every download is audited as
/// `privacy.export_downloaded`. An expired export answers 410.
#[utoipa::path(
    get,
    operation_id = "download_export",
    path = "/api/v1/workspaces/{ws}/privacy/exports/{id}/download",
    tag = "privacy",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Export id")
    ),
    responses(
        (status = 200, description = "The gzip-compressed JSON document", content_type = "application/gzip", body = Vec<u8>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a key without the admin scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or export, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The export is not ready", body = Problem, content_type = "application/problem+json"),
        (status = 410, description = "The export expired", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn download_handler(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let id: Uuid = path_id(&id)?;
    match download(&state.pool, &principal.context(), workspace.id, id).await? {
        Download::Content(bytes) => {
            let disposition = format!("attachment; filename=\"person-export-{id}.json.gz\"");
            let mut response = Response::new(Body::from(bytes));
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/gzip"),
            );
            headers.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&disposition).map_err(|_| ApiError::internal())?,
            );
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            Ok(response)
        }
        Download::Expired => Err(ApiError::new(StatusCode::GONE, "the export has expired")),
        Download::NotReady => Err(ApiError::new(
            StatusCode::CONFLICT,
            "the export is not ready",
        )),
    }
}

#[cfg(test)]
mod tests {
    use zirv_flow_store::ExportStatus;

    use super::*;

    #[test]
    fn an_export_is_shown_with_its_expiry_and_no_content() {
        let export = PrivacyExport {
            id: Uuid::nil(),
            person_id: Uuid::nil(),
            status: ExportStatus::Ready,
            requested_by: None,
            requested_at: OffsetDateTime::UNIX_EPOCH,
            ready_at: Some(OffsetDateTime::UNIX_EPOCH),
            expires_at: Some(OffsetDateTime::UNIX_EPOCH + time::Duration::hours(24)),
            size_bytes: Some(120),
            trace_id: Uuid::nil(),
            failure_code: None,
        };

        let shown = serde_json::to_value(ExportBody::from(export)).unwrap();

        assert_eq!(shown["status"], "ready");
        assert_eq!(shown["expires_at"], "1970-01-02T00:00:00Z");
        assert_eq!(shown["size_bytes"], 120);
        assert_eq!(shown["trace_id"].as_str().unwrap().len(), 32);
        assert!(shown["failure_code"].is_null());
        assert!(shown.get("content").is_none());
    }

    #[test]
    fn a_failed_export_shows_the_code_it_stopped_with() {
        let export = PrivacyExport {
            id: Uuid::nil(),
            person_id: Uuid::nil(),
            status: ExportStatus::Failed,
            requested_by: None,
            requested_at: OffsetDateTime::UNIX_EPOCH,
            ready_at: None,
            expires_at: None,
            size_bytes: None,
            trace_id: Uuid::nil(),
            failure_code: Some(zirv_flow_store::EXPORT_TOO_LARGE.to_owned()),
        };

        let shown = serde_json::to_value(ExportBody::from(export)).unwrap();

        assert_eq!(shown["status"], "failed");
        assert_eq!(shown["failure_code"], "export_too_large");
    }
}
