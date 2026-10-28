//! The audit log's read API (architecture 16.2): a filtered, keyset-paged list per workspace
//! (workspace admins) and for the instance rows (the owner), the action catalogue for the filter
//! menu, and the streamed export in `export`. Rows hold opaque ids only; no response joins them
//! to emails or names.

mod acceptance;
pub(crate) mod export;

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{TraceId, WorkspaceId, audit};
use zirv_flow_store::{AuditEntry, AuditFilter};

use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::jobs_admin::{DEFAULT_LIMIT, parse_limit};
use crate::problem::{ApiError, Problem};

/// One audit row.
#[derive(Serialize, ToSchema)]
pub(crate) struct AuditEntryBody {
    id: i64,
    /// Null for an instance row such as a sign-in.
    workspace_id: Option<Uuid>,
    /// `user`, `api_key`, `system` or `anonymous`.
    #[schema(example = "user")]
    actor_kind: String,
    actor_id: Option<Uuid>,
    #[schema(example = "flow.published")]
    action: String,
    object_kind: Option<String>,
    object_id: Option<Uuid>,
    request_id: Option<String>,
    /// The trace of the request, which opens at the trace page: 32 lowercase hex characters.
    /// Null on rows from before traces were recorded and on rows written outside a request.
    trace_id: Option<String>,
    /// Ids and counts only; never an email, a name or a secret.
    #[schema(value_type = Object)]
    summary: Value,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    occurred_at: OffsetDateTime,
}

impl From<AuditEntry> for AuditEntryBody {
    fn from(entry: AuditEntry) -> Self {
        Self {
            id: entry.id,
            workspace_id: entry.workspace_id,
            actor_kind: entry.actor_kind,
            actor_id: entry.actor_id,
            action: entry.action,
            object_kind: entry.object_kind,
            object_id: entry.object_id,
            request_id: entry.request_id,
            trace_id: entry.trace_id.map(|id| TraceId::from_uuid(id).to_string()),
            summary: entry.summary,
            occurred_at: entry.occurred_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AuditPageBody {
    /// Newest first.
    entries: Vec<AuditEntryBody>,
    /// Pass as `cursor` for the next page; null when there is none.
    next_cursor: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AuditAreaBody {
    /// The text before the dot; pass `<area>.` as `action` to filter by area.
    #[schema(example = "flow")]
    area: &'static str,
    actions: Vec<&'static str>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AuditActionsBody {
    areas: Vec<AuditAreaBody>,
}

/// The query of a list or an export.
pub(crate) struct AuditQuery {
    pub(crate) filter: AuditFilter,
    pub(crate) limit: u32,
    pub(crate) cursor: Option<i64>,
    pub(crate) format: Option<String>,
}

fn parse_uuid(name: &str, value: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(value).map_err(|_| ApiError::bad_request(format!("{name} must be a UUID")))
}

fn parse_time(name: &str, value: &str) -> Result<OffsetDateTime, ApiError> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| ApiError::bad_request(format!("{name} must be an RFC 3339 timestamp")))
}

/// Reads the filters, `limit`, `cursor` and `format` of the query string, for `workspace` or for
/// the instance rows when it is none. Unknown keys are ignored.
pub(crate) fn parse_query(
    workspace: Option<WorkspaceId>,
    query: &str,
) -> Result<AuditQuery, ApiError> {
    let mut parsed = AuditQuery {
        filter: AuditFilter {
            workspace,
            ..AuditFilter::default()
        },
        limit: DEFAULT_LIMIT,
        cursor: None,
        format: None,
    };
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let filter = &mut parsed.filter;
        match key.as_ref() {
            "limit" => parsed.limit = parse_limit(&value)?,
            "cursor" => {
                parsed.cursor = Some(
                    decode_cursor(&value)
                        .ok_or_else(|| ApiError::bad_request("cursor is malformed"))?,
                );
            }
            "format" => parsed.format = Some(value.into_owned()),
            "action" => {
                let known = audit::is_known(&value)
                    || value
                        .strip_suffix('.')
                        .is_some_and(|area| audit::ACTIONS.iter().any(|a| audit::area(a) == area));
                if !known {
                    return Err(ApiError::bad_request(
                        "action must be a listed action or a listed area followed by a dot",
                    ));
                }
                filter.action = Some(value.into_owned());
            }
            "actor_kind" => {
                if !["user", "api_key", "system", "anonymous"].contains(&value.as_ref()) {
                    return Err(ApiError::bad_request(
                        "actor_kind must be user, api_key, system or anonymous",
                    ));
                }
                filter.actor_kind = Some(value.into_owned());
            }
            "actor_id" => filter.actor_id = Some(parse_uuid("actor_id", &value)?),
            "object_kind" => filter.object_kind = Some(value.into_owned()),
            "object_id" => filter.object_id = Some(parse_uuid("object_id", &value)?),
            "from" => filter.from = Some(parse_time("from", &value)?),
            "to" => filter.to = Some(parse_time("to", &value)?),
            "request_id" => filter.request_id = Some(value.into_owned()),
            _ => {}
        }
    }
    Ok(parsed)
}

/// Opaque to callers: base64url of the id the next page starts below.
pub(crate) fn encode_cursor(id: i64) -> String {
    URL_SAFE_NO_PAD.encode(format!("audit:{id}"))
}

pub(crate) fn decode_cursor(cursor: &str) -> Option<i64> {
    let bytes = URL_SAFE_NO_PAD.decode(cursor).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    text.strip_prefix("audit:")?.parse().ok()
}

async fn page(
    state: &AppState,
    workspace: Option<WorkspaceId>,
    query: Option<String>,
) -> Result<Json<AuditPageBody>, ApiError> {
    let query = parse_query(workspace, &query.unwrap_or_default())?;
    let page = zirv_flow_store::list_audit_page(
        &state.pool,
        &query.filter,
        query.cursor,
        i64::from(query.limit),
    )
    .await?;
    Ok(Json(AuditPageBody {
        entries: page.entries.into_iter().map(AuditEntryBody::from).collect(),
        next_cursor: page.next.map(encode_cursor),
    }))
}

/// The workspace's audit log, newest first, filtered and keyset-paged. Needs the workspace admin
/// role. Filters: `action` (a listed action, or an area such as `flow.`), `actor_kind`,
/// `actor_id`, `object_kind`, `object_id`, `from` (inclusive) and `to` (exclusive) on the time
/// of the row, and `request_id`.
#[utoipa::path(
    get,
    operation_id = "list_audit",
    path = "/api/v1/workspaces/{ws}/audit",
    tag = "audit",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("limit" = Option<u32>, Query, description = "Rows per page, 1 to 200; default 50"),
        ("cursor" = Option<String>, Query, description = "The next_cursor of the previous page"),
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
        (status = 200, description = "A page of audit rows", body = AuditPageBody),
        (status = 400, description = "A parameter is malformed", body = Problem, content_type = "application/problem+json"),
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
) -> Result<Json<AuditPageBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    page(&state, Some(workspace.id), query).await
}

/// The instance audit rows (sign-ins, user creation, key rotation), newest first, with the
/// same filters and paging as the workspace log. Instance owner only.
#[utoipa::path(
    get,
    operation_id = "list_instance_audit",
    path = "/api/v1/audit",
    tag = "audit",
    params(
        ("limit" = Option<u32>, Query, description = "Rows per page, 1 to 200; default 50"),
        ("cursor" = Option<String>, Query, description = "The next_cursor of the previous page"),
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
        (status = 200, description = "A page of instance audit rows", body = AuditPageBody),
        (status = 400, description = "A parameter is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not the instance owner, or an API key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []))
)]
pub(crate) async fn list_instance(
    State(state): State<AppState>,
    principal: Principal,
    RawQuery(query): RawQuery,
) -> Result<Json<AuditPageBody>, ApiError> {
    principal.require_owner()?;
    page(&state, None, query).await
}

/// The audit actions, grouped by area, for a filter menu. Needs the workspace admin role.
#[utoipa::path(
    get,
    operation_id = "list_audit_actions",
    path = "/api/v1/workspaces/{ws}/audit/actions",
    tag = "audit",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "Every action an audit row may carry", body = AuditActionsBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a key without the admin scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn actions(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<AuditActionsBody>, ApiError> {
    authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    Ok(Json(catalogue()))
}

fn catalogue() -> AuditActionsBody {
    let mut areas: Vec<AuditAreaBody> = Vec::new();
    for action in audit::ACTIONS {
        let area = audit::area(action);
        match areas.last_mut() {
            Some(last) if last.area == area => last.actions.push(action),
            _ => areas.push(AuditAreaBody {
                area,
                actions: vec![action],
            }),
        }
    }
    AuditActionsBody { areas }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_round_trips_and_is_url_safe() {
        let cursor = encode_cursor(123_456_789);

        assert_eq!(decode_cursor(&cursor), Some(123_456_789));
        assert!(!cursor.contains(['=', '+', '/']));
    }

    #[test]
    fn a_malformed_cursor_does_not_decode() {
        for bad in ["", "!!", "YXVkaXQ6eA", "eDoxMg"] {
            assert_eq!(decode_cursor(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_query_fills_every_filter() {
        let id = Uuid::new_v4();
        let query = format!(
            "limit=7&action=flow.&actor_kind=user&actor_id={id}&object_kind=flow&object_id={id}\
             &from=2026-01-01T00:00:00Z&to=2026-02-01T00:00:00Z&request_id=r-1&format=csv"
        );

        let parsed = parse_query(Some(WorkspaceId::new()), &query).unwrap();

        assert_eq!(parsed.limit, 7);
        assert_eq!(parsed.filter.action.as_deref(), Some("flow."));
        assert_eq!(parsed.filter.actor_id, Some(id));
        assert_eq!(parsed.filter.object_id, Some(id));
        assert!(parsed.filter.from < parsed.filter.to);
        assert_eq!(parsed.filter.request_id.as_deref(), Some("r-1"));
        assert_eq!(parsed.format.as_deref(), Some("csv"));
    }

    #[test]
    fn a_bad_parameter_is_refused() {
        for bad in [
            "limit=0",
            "limit=201",
            "cursor=zz",
            "action=made.up",
            "action=nothing.",
            "actor_kind=robot",
            "actor_id=x",
            "from=yesterday",
        ] {
            assert!(parse_query(None, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_catalogue_groups_every_action_by_area() {
        let body = catalogue();

        let total: usize = body.areas.iter().map(|a| a.actions.len()).sum();
        assert_eq!(total, audit::ACTIONS.len());
        assert!(
            body.areas
                .iter()
                .all(|a| a.actions.iter().all(|x| audit::area(x) == a.area))
        );
        let mut names: Vec<_> = body.areas.iter().map(|a| a.area).collect();
        names.dedup();
        assert_eq!(names.len(), body.areas.len(), "an area appears once");
    }

    #[test]
    fn an_entry_shows_its_trace_as_hex() {
        let entry = AuditEntry {
            id: 1,
            workspace_id: None,
            actor_kind: "system".into(),
            actor_id: None,
            action: "audit.exported".into(),
            object_kind: None,
            object_id: None,
            request_id: None,
            trace_id: Some(Uuid::nil()),
            summary: serde_json::json!({}),
            occurred_at: OffsetDateTime::UNIX_EPOCH,
        };

        let shown = serde_json::to_value(AuditEntryBody::from(entry)).unwrap();

        assert_eq!(shown["trace_id"].as_str().unwrap().len(), 32);
        assert_eq!(shown["occurred_at"], "1970-01-01T00:00:00Z");
    }
}
