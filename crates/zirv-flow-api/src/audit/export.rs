//! The audit export: the log as newline-delimited JSON or CSV, streamed in keyset batches so
//! a log of any size is never held in memory. The export is itself audited (`audit.exported`,
//! with the format, the filters and the row count, never a value) before the first byte leaves.

use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, header};
use axum::response::Response;
use futures_util::{StreamExt, TryStreamExt};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{AuditEntry, AuditFilter};

use super::{AuditEntryBody, parse_query};
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::consent::csv;
use crate::problem::{ApiError, Problem};

const CSV_HEADER: &str = "id,occurred_at,workspace_id,actor_kind,actor_id,action,object_kind,\
                          object_id,request_id,trace_id,summary\n";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Format {
    Ndjson,
    Csv,
}

impl Format {
    fn parse(value: Option<&str>) -> Result<Self, ApiError> {
        match value {
            None | Some("ndjson") => Ok(Self::Ndjson),
            Some("csv") => Ok(Self::Csv),
            Some(_) => Err(ApiError::bad_request("format must be ndjson or csv")),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Ndjson => "ndjson",
            Self::Csv => "csv",
        }
    }

    const fn content_type(self) -> &'static str {
        match self {
            Self::Ndjson => "application/x-ndjson",
            Self::Csv => "text/csv; charset=utf-8",
        }
    }
}

/// A cell that a spreadsheet would run as a formula (the OWASP set: `=`, `+`, `-`, `@`, a tab or
/// a carriage return first) is prefixed with `'` so it reads as text; then it is quoted as CSV
/// needs.
fn cell(value: &str) -> String {
    if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        csv::field(&format!("'{value}"))
    } else {
        csv::field(value)
    }
}

fn csv_line(body: &AuditEntryBody) -> String {
    let id = |value: Option<Uuid>| value.map(|id| id.to_string()).unwrap_or_default();
    let text = |value: &Option<String>| cell(value.as_deref().unwrap_or_default());
    let cells = [
        body.id.to_string(),
        body.occurred_at.format(&Rfc3339).unwrap_or_default(),
        id(body.workspace_id),
        cell(&body.actor_kind),
        id(body.actor_id),
        cell(&body.action),
        text(&body.object_kind),
        id(body.object_id),
        text(&body.request_id),
        text(&body.trace_id),
        cell(&body.summary.to_string()),
    ];
    let mut line = cells.join(",");
    line.push('\n');
    line
}

/// One batch as the bytes of its lines.
fn encode_batch(format: Format, batch: Vec<AuditEntry>) -> Bytes {
    let mut text = String::new();
    for entry in batch {
        let body = AuditEntryBody::from(entry);
        match format {
            Format::Ndjson => {
                if let Ok(line) = serde_json::to_string(&body) {
                    text.push_str(&line);
                    text.push('\n');
                }
            }
            Format::Csv => text.push_str(&csv_line(&body)),
        }
    }
    Bytes::from(text)
}

/// The non-empty filters, as ids and plain words: the audit record of an export names what was
/// asked for and holds no row.
fn filters_summary(filter: &AuditFilter) -> Value {
    let time = |value: Option<time::OffsetDateTime>| value.and_then(|at| at.format(&Rfc3339).ok());
    let mut summary = json!({
        "action": filter.action,
        "actor_kind": filter.actor_kind,
        "actor_id": filter.actor_id,
        "object_kind": filter.object_kind,
        "object_id": filter.object_id,
        "from": time(filter.from),
        "to": time(filter.to),
        "request_id": filter.request_id,
    });
    if let Some(map) = summary.as_object_mut() {
        map.retain(|_, value| !value.is_null());
    }
    summary
}

/// Audits the export, then streams it. The row count and the last id are read first, so the
/// count in the audit row is what the stream carries; rows appended meanwhile are not in it.
async fn export(
    state: &AppState,
    principal: &Principal,
    workspace: Option<WorkspaceId>,
    query: Option<String>,
) -> Result<Response, ApiError> {
    let query = parse_query(workspace, &query.unwrap_or_default())?;
    let format = Format::parse(query.format.as_deref())?;
    let filter = query.filter;
    let (rows, through) = zirv_flow_store::count_audit(&state.pool, &filter).await?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    admin::audit(
        &mut tx,
        &principal.context(),
        workspace,
        "audit.exported",
        None,
        json!({"format": format.name(), "rows": rows, "filters": filters_summary(&filter)}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;

    let batches = zirv_flow_store::stream_audit(state.pool.clone(), filter, through.unwrap_or(0))
        .map_ok(move |batch| encode_batch(format, batch));
    let head = match format {
        Format::Csv => Some(Bytes::from_static(CSV_HEADER.as_bytes())),
        Format::Ndjson => None,
    };
    let stream = futures_util::stream::iter(head.map(Ok)).chain(batches);

    let mut response = Response::new(Body::from_stream(stream));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(format.content_type()),
    );
    let extension = match format {
        Format::Ndjson => "ndjson",
        Format::Csv => "csv",
    };
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"audit.{extension}\""))
            .map_err(|_| ApiError::internal())?,
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// Exports the workspace's audit log, newest first, as `format=ndjson` (default) or `csv`, with
/// the filters of the list. The body is streamed in batches, so the size is unbounded. A CSV cell
/// that starts with `=`, `+`, `-`, `@`, a tab or a carriage return is prefixed with `'`. Needs
/// the workspace admin role. Audited as `audit.exported`.
#[utoipa::path(
    get,
    operation_id = "export_audit",
    path = "/api/v1/workspaces/{ws}/audit/export",
    tag = "audit",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("format" = Option<String>, Query, description = "ndjson (default) or csv"),
        ("action" = Option<String>, Query, description = "A listed action, or an area followed by a dot"),
        ("actor_kind" = Option<String>, Query, description = "user, api_key, system or anonymous"),
        ("actor_id" = Option<Uuid>, Query, description = "The acting user or key"),
        ("object_kind" = Option<String>, Query, description = "The kind of object acted on"),
        ("object_id" = Option<Uuid>, Query, description = "The object acted on"),
        ("from" = Option<String>, Query, description = "RFC 3339; rows at or after it"),
        ("to" = Option<String>, Query, description = "RFC 3339; rows before it"),
        ("request_id" = Option<String>, Query, description = "The request that caused the row")
    ),
    responses(
        (status = 200, description = "The rows, newest first, one per line", content_type = "application/x-ndjson", body = String),
        (status = 400, description = "A parameter is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a key without the admin scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn workspace(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    export(&state, &principal, Some(workspace.id), query).await
}

/// Exports the instance audit rows like the workspace export. Instance owner only. Audited as
/// `audit.exported`.
#[utoipa::path(
    get,
    operation_id = "export_instance_audit",
    path = "/api/v1/audit/export",
    tag = "audit",
    params(
        ("format" = Option<String>, Query, description = "ndjson (default) or csv"),
        ("action" = Option<String>, Query, description = "A listed action, or an area followed by a dot"),
        ("actor_kind" = Option<String>, Query, description = "user, api_key, system or anonymous"),
        ("actor_id" = Option<Uuid>, Query, description = "The acting user or key"),
        ("object_kind" = Option<String>, Query, description = "The kind of object acted on"),
        ("object_id" = Option<Uuid>, Query, description = "The object acted on"),
        ("from" = Option<String>, Query, description = "RFC 3339; rows at or after it"),
        ("to" = Option<String>, Query, description = "RFC 3339; rows before it"),
        ("request_id" = Option<String>, Query, description = "The request that caused the row")
    ),
    responses(
        (status = 200, description = "The rows, newest first, one per line", content_type = "application/x-ndjson", body = String),
        (status = 400, description = "A parameter is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not the instance owner, or an API key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []))
)]
pub(crate) async fn instance(
    State(state): State<AppState>,
    principal: Principal,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    principal.require_owner()?;
    export(&state, &principal, None, query).await
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;

    fn body(request_id: Option<&str>) -> AuditEntryBody {
        AuditEntryBody::from(AuditEntry {
            id: 9,
            workspace_id: None,
            actor_kind: "system".into(),
            actor_id: None,
            action: "audit.exported".into(),
            object_kind: None,
            object_id: None,
            request_id: request_id.map(str::to_owned),
            trace_id: None,
            summary: json!({"rows": 2}),
            occurred_at: OffsetDateTime::UNIX_EPOCH,
        })
    }

    #[test]
    fn a_cell_that_starts_like_a_formula_is_prefixed() {
        for lead in ["=", "+", "-", "@", "\t", "\r"] {
            let value = format!("{lead}SUM(A1)");

            let shown = cell(&value);

            assert!(
                shown.trim_matches('"').starts_with('\''),
                "{lead:?} gave {shown:?}"
            );
        }
    }

    #[test]
    fn other_cells_are_left_alone_and_commas_are_quoted() {
        assert_eq!(cell("flow.created"), "flow.created");
        assert_eq!(cell("a-b"), "a-b");
        assert_eq!(cell("a,b"), "\"a,b\"");
        assert_eq!(cell(""), "");
    }

    #[test]
    fn a_csv_line_carries_the_columns_and_guards_a_request_id() {
        let line = csv_line(&body(Some("=cmd")));

        assert!(line.starts_with("9,1970-01-01T00:00:00Z,,system,,audit.exported,"));
        assert!(line.contains(",'=cmd,"));
        assert!(line.ends_with('\n'));
        assert_eq!(
            CSV_HEADER.matches(',').count(),
            line.matches(',').count(),
            "one cell per header column"
        );
    }

    #[test]
    fn an_ndjson_batch_has_one_object_per_line() {
        let entry = AuditEntry {
            id: 1,
            workspace_id: None,
            actor_kind: "system".into(),
            actor_id: None,
            action: "audit.exported".into(),
            object_kind: None,
            object_id: None,
            request_id: None,
            trace_id: None,
            summary: json!({}),
            occurred_at: OffsetDateTime::UNIX_EPOCH,
        };

        let bytes = encode_batch(Format::Ndjson, vec![entry.clone(), entry]);

        let lines: Vec<&str> = std::str::from_utf8(&bytes).unwrap().lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines
                .iter()
                .all(|l| serde_json::from_str::<Value>(l).is_ok())
        );
    }

    #[test]
    fn the_format_defaults_to_ndjson_and_refuses_others() {
        assert_eq!(Format::parse(None).unwrap(), Format::Ndjson);
        assert_eq!(Format::parse(Some("csv")).unwrap(), Format::Csv);
        assert!(Format::parse(Some("xml")).is_err());
    }

    #[test]
    fn the_filters_summary_names_only_the_filters_given() {
        let filter = AuditFilter {
            action: Some("flow.".into()),
            ..AuditFilter::default()
        };

        assert_eq!(filters_summary(&filter), json!({"action": "flow."}));
    }
}
