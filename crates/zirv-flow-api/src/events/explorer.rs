//! The event explorer (architecture section 12.3): counts over time, a breakdown of one
//! property, rows and a CSV export over the stored events of a time range.
//!
//! The endpoints are POSTs so filter values stay out of URLs and access logs. Every query
//! carries the workspace and the time bounds and runs under a statement timeout (the
//! `[explorer]` settings); one that runs past it answers 503 with the code `query_timeout`.
//! Rows are masked per viewer with the same rule as the person timeline. Nothing here logs
//! or audits a name, an identifier or a value.

use std::io;
use std::time::Duration;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::stream;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::events::mask_event;
use zirv_flow_core::{TraceId, WorkspaceId};
use zirv_flow_store::{
    Bucket, ExploreError, ExploreFilter, ExploreLimits, ExploreRow, ExploreScope, FilterOp, PgPool,
    RowCursor,
};

use crate::admin;
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::consent::csv;
use crate::jobs_admin::{DEFAULT_LIMIT, MAX_LIMIT};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

/// The explorer's limits, from the `[explorer]` settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExplorerLimits {
    /// How long an interactive query (count, breakdown, list) may run.
    pub interactive_timeout: Duration,
    /// How long one page query of an export may run.
    pub export_timeout: Duration,
    /// The most rows or buckets an interactive query returns.
    pub interactive_row_limit: u32,
    /// The most rows an export writes.
    pub export_row_limit: u32,
}

impl Default for ExplorerLimits {
    fn default() -> Self {
        Self {
            interactive_timeout: Duration::from_millis(5_000),
            export_timeout: Duration::from_millis(60_000),
            interactive_row_limit: 10_000,
            export_row_limit: 1_000_000,
        }
    }
}

impl ExplorerLimits {
    fn interactive(&self) -> ExploreLimits {
        ExploreLimits {
            timeout: self.interactive_timeout,
            row_limit: self.interactive_row_limit,
        }
    }

    /// How long one page query of an export, or a replay dry run, may run.
    pub(crate) fn export_timeout(&self) -> Duration {
        self.export_timeout
    }

    fn export(&self) -> ExploreLimits {
        ExploreLimits {
            timeout: self.export_timeout,
            row_limit: self.export_row_limit,
        }
    }
}

/// One property filter. The operators that apply follow the property's type in the published
/// schema of `name`; without a name, or for a property the schema does not declare, values
/// compare as text.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ExploreFilterRequest {
    /// A top-level property name.
    #[schema(example = "plan")]
    key: String,
    /// `eq`, `ne`, `in`, `prefix`, `gt`, `gte`, `lt`, `lte` or `exists`. For a string list,
    /// `eq` means "contains". `ne` also matches events without the property.
    #[schema(example = "eq")]
    op: String,
    /// Required for every operator but `exists` (where `false` means "absent"); `in` takes
    /// a list of up to 100 values.
    #[serde(default)]
    value: Option<Value>,
}

/// What a query looks at.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ExploreScopeRequest {
    /// Only events of this name.
    name: Option<String>,
    #[serde(default)]
    filters: Vec<ExploreFilterRequest>,
    /// Start of the range, inclusive.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    from: OffsetDateTime,
    /// End of the range, exclusive; at most 400 days after `from`.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    to: OffsetDateTime,
    /// Only this person's events, and those of the people merged into them.
    person_id: Option<Uuid>,
}

impl ExploreScopeRequest {
    fn filters(&self) -> Result<Vec<ExploreFilter>, ApiError> {
        self.filters
            .iter()
            .map(|filter| {
                let op: FilterOp = filter
                    .op
                    .parse()
                    .map_err(|()| ApiError::unprocessable("filters.op: is not a known operator"))?;
                Ok(ExploreFilter {
                    key: filter.key.clone(),
                    op,
                    value: filter.value.clone(),
                })
            })
            .collect()
    }

    fn scope<'a>(&'a self, filters: &'a [ExploreFilter]) -> ExploreScope<'a> {
        ExploreScope {
            name: self.name.as_deref(),
            filters,
            from: self.from,
            to: self.to,
            person: self.person_id,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ExploreCountRequest {
    #[serde(flatten)]
    scope: ExploreScopeRequest,
    /// `hour`, `day` or `week`, in UTC.
    #[schema(example = "day")]
    bucket: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct BucketBody {
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    bucket_start: OffsetDateTime,
    count: i64,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ExploreCountBody {
    /// Oldest first; buckets without events are left out.
    buckets: Vec<BucketBody>,
    /// The sum of the buckets.
    total: i64,
    /// More buckets matched than the row limit allows; the newest were left out.
    truncated: bool,
}

/// Events per bucket over the range, for the filters given. The sum of the buckets is the
/// number of matching events, and equals what the breakdown and the list cover.
#[utoipa::path(
    post,
    operation_id = "explore_event_count",
    path = "/api/v1/workspaces/{ws}/events/explore/count",
    tag = "events",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = ExploreCountRequest,
    responses(
        (status = 200, description = "Counts per bucket", body = ExploreCountBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope, a missing CSRF token, or a probe of a PII property by a caller who may not see values (`code` is `pii_property_not_explorable`)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A malformed range, filter or bucket", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The query ran past its time limit (`code` is `query_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn count(
    State(state): State<crate::AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(request): ApiJson<ExploreCountRequest>,
) -> Result<Json<ExploreCountBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let bucket: Bucket = request
        .bucket
        .parse()
        .map_err(|()| ApiError::unprocessable("bucket: must be hour, day or week"))?;
    let filters = request.scope.filters()?;
    let reveal = may_see_values(&state, &principal, workspace.id).await;
    let result = zirv_flow_store::explore_count(
        &state.pool,
        workspace.id,
        &request.scope.scope(&filters),
        bucket,
        state.explorer.interactive(),
        reveal,
    )
    .await?;
    let total = result.buckets.iter().map(|(_, count)| count).sum();
    Ok(Json(ExploreCountBody {
        buckets: result
            .buckets
            .into_iter()
            .map(|(bucket_start, count)| BucketBody {
                bucket_start,
                count,
            })
            .collect(),
        total,
        truncated: result.truncated,
    }))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ExploreBreakdownRequest {
    #[serde(flatten)]
    scope: ExploreScopeRequest,
    /// The property to count the values of. A caller who may not see values can break down
    /// only a property the published schema of `name` declares non-PII.
    #[schema(example = "plan")]
    property: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ValueCountBody {
    /// The property's text; a list or an object is its JSON.
    value: String,
    count: i64,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ExploreBreakdownBody {
    property: String,
    /// The 50 most common values, most common first.
    values: Vec<ValueCountBody>,
    /// Matching events whose value is not among `values`.
    other: i64,
    /// Matching events without the property.
    missing: i64,
}

/// The most common values of one property over the same events a count covers.
#[utoipa::path(
    post,
    operation_id = "explore_event_breakdown",
    path = "/api/v1/workspaces/{ws}/events/explore/breakdown",
    tag = "events",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = ExploreBreakdownRequest,
    responses(
        (status = 200, description = "The top values", body = ExploreBreakdownBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope, a missing CSRF token, or a PII property for a caller who may not see values (`code` is `pii_property_not_explorable`)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A malformed range, filter or property", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The query ran past its time limit (`code` is `query_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn breakdown(
    State(state): State<crate::AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(request): ApiJson<ExploreBreakdownRequest>,
) -> Result<Json<ExploreBreakdownBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let filters = request.scope.filters()?;
    let reveal = may_see_values(&state, &principal, workspace.id).await;
    let result = zirv_flow_store::explore_breakdown(
        &state.pool,
        workspace.id,
        &request.scope.scope(&filters),
        &request.property,
        state.explorer.interactive(),
        reveal,
    )
    .await?;
    Ok(Json(ExploreBreakdownBody {
        property: request.property,
        values: result
            .values
            .into_iter()
            .map(|(value, count)| ValueCountBody { value, count })
            .collect(),
        other: result.other,
        missing: result.missing,
    }))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ExploreListRequest {
    #[serde(flatten)]
    scope: ExploreScopeRequest,
    /// Rows per page, 1 to 200; default 50.
    limit: Option<u32>,
    /// The `next_after` of the previous page.
    after: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ExploredEventBody {
    id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    occurred_at: OffsetDateTime,
    name: String,
    source: String,
    person_id: Uuid,
    /// `valid`, `flagged` or `unplanned`.
    validation_state: String,
    /// Timing flags such as `late`, and `replayed` and `no_triggers` for a replayed event.
    flags: Vec<String>,
    /// The stored properties. The values of properties the published schema flags as PII,
    /// of undeclared properties and of every property of an event without a published
    /// schema are null for callers who may not see values.
    #[schema(value_type = Object)]
    properties: Map<String, Value>,
    /// The names whose values were replaced by null.
    masked: Vec<String>,
    /// 32 lowercase hex characters.
    trace_id: String,
}

fn event_body(row: ExploreRow, reveal: bool) -> ExploredEventBody {
    let masked = mask_event(
        &row.properties,
        row.schema.as_ref().map(|schema| schema.as_slice()),
        reveal,
    );
    ExploredEventBody {
        id: row.id,
        occurred_at: row.occurred_at,
        name: row.name,
        source: row.source,
        person_id: row.person_id,
        validation_state: row.validation_state,
        flags: row.flags,
        properties: masked.properties,
        masked: masked.masked,
        trace_id: TraceId::from_uuid(row.trace_id).to_string(),
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ExploreListBody {
    /// Newest first.
    events: Vec<ExploredEventBody>,
    /// Pass as `after` for the next page; null when there is none.
    next_after: Option<String>,
}

/// Opaque to callers: base64url of the microseconds since the epoch and the id.
fn encode_cursor(cursor: RowCursor) -> String {
    let micros = cursor.at.unix_timestamp_nanos() / 1000;
    URL_SAFE_NO_PAD.encode(format!("{micros}:{}", cursor.id))
}

fn decode_cursor(text: &str) -> Option<RowCursor> {
    let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (micros, id) = text.split_once(':')?;
    let nanos = micros.parse::<i128>().ok()?.checked_mul(1000)?;
    Some(RowCursor {
        at: OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()?,
        id: id.parse().ok()?,
    })
}

/// The events of the range that match, newest first in keyset pages, masked for the caller.
#[utoipa::path(
    post,
    operation_id = "explore_event_list",
    path = "/api/v1/workspaces/{ws}/events/explore/list",
    tag = "events",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = ExploreListRequest,
    responses(
        (status = 200, description = "A page of events", body = ExploreListBody),
        (status = 400, description = "limit is out of range or after is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope, a missing CSRF token, or a probe of a PII property by a caller who may not see values (`code` is `pii_property_not_explorable`)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A malformed range or filter", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The query ran past its time limit (`code` is `query_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<crate::AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(request): ApiJson<ExploreListRequest>,
) -> Result<Json<ExploreListBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let limit = match request.limit {
        None => DEFAULT_LIMIT,
        Some(limit) if (1..=MAX_LIMIT).contains(&limit) => limit,
        Some(_) => {
            return Err(ApiError::bad_request(format!(
                "limit must be from 1 to {MAX_LIMIT}"
            )));
        }
    };
    let after = match &request.after {
        None => None,
        Some(text) => Some(
            decode_cursor(text)
                .ok_or_else(|| ApiError::bad_request("after is not a valid cursor"))?,
        ),
    };
    let filters = request.scope.filters()?;
    let reveal = may_see_values(&state, &principal, workspace.id).await;
    let page = zirv_flow_store::explore_rows(
        &state.pool,
        workspace.id,
        &request.scope.scope(&filters),
        after,
        limit,
        state.explorer.interactive(),
        reveal,
    )
    .await?;
    Ok(Json(ExploreListBody {
        events: page
            .rows
            .into_iter()
            .map(|row| event_body(row, reveal))
            .collect(),
        next_after: page.next.map(encode_cursor),
    }))
}

const EXPORT_HEADER: &str =
    "id,occurred_at,name,source,person_id,validation_state,flags,trace_id,properties,masked\n";
const EXPORT_PAGE: u32 = 1_000;

/// A cell that a spreadsheet would run as a formula is made text.
fn cell(value: &str) -> String {
    if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        csv::field(&format!("'{value}"))
    } else {
        csv::field(value)
    }
}

/// The CSV lines of a page: the same columns and masking as the list.
fn export_lines(rows: Vec<ExploreRow>, reveal: bool) -> String {
    let mut text = String::new();
    for row in rows {
        let body = event_body(row, reveal);
        let line = [
            body.id.to_string(),
            body.occurred_at.format(&Rfc3339).unwrap_or_default(),
            cell(&body.name),
            cell(&body.source),
            body.person_id.to_string(),
            body.validation_state,
            cell(&body.flags.join("|")),
            body.trace_id,
            csv::field(&Value::Object(body.properties).to_string()),
            cell(&body.masked.join("|")),
        ];
        text.push_str(&line.join(","));
        text.push('\n');
    }
    text
}

/// The shape of a query for the audit trail: operators and counts, never a name, a key or a
/// value.
fn audit_shape(request: &ExploreScopeRequest, filters: &[ExploreFilter]) -> Value {
    json!({
        "filter_ops": filters.iter().map(|filter| filter.op.as_str()).collect::<Vec<_>>(),
        "named": request.name.is_some(),
        "person": request.person_id.is_some(),
        "range_days": (request.to - request.from).whole_days(),
    })
}

enum Export {
    Header,
    Page {
        after: Option<RowCursor>,
        written: u64,
    },
    Done,
}

/// The matching events as CSV with the list's columns and masking, streamed a page at a
/// time (`id,occurred_at,name,source,person_id,validation_state,flags,trace_id,properties,masked`;
/// `flags` and `masked` are joined by `|`). At most the export row limit is written; when
/// more matched, the last line is a `#` comment saying so. The export is audited as
/// `events.exported` with the shape of the filters and the row count, no values.
#[utoipa::path(
    post,
    operation_id = "export_events",
    path = "/api/v1/workspaces/{ws}/events/explore/export",
    tag = "events",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = ExploreScopeRequest,
    responses(
        (status = 200, description = "The events as CSV, newest first", body = String, content_type = "text/csv"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope, a missing CSRF token, or a probe of a PII property by a caller who may not see values (`code` is `pii_property_not_explorable`)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A malformed range or filter", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The query ran past its time limit (`code` is `query_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn export(
    State(state): State<crate::AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(request): ApiJson<ExploreScopeRequest>,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let filters = request.filters()?;
    let reveal = may_see_values(&state, &principal, workspace.id).await;
    let limits = state.explorer.export();

    // Count first: a bad filter or a timeout is an error response, not a broken stream, and
    // the audit entry exists whether or not the client reads to the end.
    let matched = zirv_flow_store::explore_total(
        &state.pool,
        workspace.id,
        &request.scope(&filters),
        limits,
        reveal,
    )
    .await
    .map_err(ApiError::from)?;
    let rows = u64::try_from(matched)
        .unwrap_or(0)
        .min(u64::from(limits.row_limit));
    let mut shape = audit_shape(&request, &filters);
    shape["rows"] = json!(rows);
    shape["truncated"] = json!(u64::try_from(matched).unwrap_or(0) > rows);
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "events.exported",
        None,
        shape,
    )
    .await?;
    zirv_flow_store::commit(tx).await?;

    let source = ExportSource {
        pool: state.pool.clone(),
        workspace: workspace.id,
        request,
        filters,
        limits,
        reveal,
        truncated: u64::try_from(matched).unwrap_or(0) > rows,
    };
    let body = stream::unfold((source, Export::Header), |(source, phase)| async move {
        match phase {
            Export::Header => Some((
                Ok(Bytes::from_static(EXPORT_HEADER.as_bytes())),
                (
                    source,
                    Export::Page {
                        after: None,
                        written: 0,
                    },
                ),
            )),
            Export::Page { after, written } => {
                let remaining = u64::from(source.limits.row_limit).saturating_sub(written);
                if remaining == 0 {
                    return Some((Ok(source.tail()), (source, Export::Done)));
                }
                let size = u32::try_from(remaining.min(u64::from(EXPORT_PAGE))).unwrap_or(1);
                let page = zirv_flow_store::explore_rows(
                    &source.pool,
                    source.workspace,
                    &source.request.scope(&source.filters),
                    after,
                    size,
                    source.limits,
                    source.reveal,
                )
                .await;
                match page {
                    Ok(page) => {
                        let count = u64::try_from(page.rows.len()).unwrap_or(0);
                        let text = export_lines(page.rows, source.reveal);
                        let next = match page.next {
                            Some(cursor) => Export::Page {
                                after: Some(cursor),
                                written: written + count,
                            },
                            None => Export::Done,
                        };
                        let text = if matches!(next, Export::Done) {
                            text + &source.tail_text()
                        } else {
                            text
                        };
                        Some((Ok(Bytes::from(text)), (source, next)))
                    }
                    Err(error) => {
                        tracing::error!(
                            timeout = matches!(&error, ExploreError::Store(e) if e.is_timeout()),
                            "the event export failed"
                        );
                        Some((
                            Err(io::Error::other("the export failed")),
                            (source, Export::Done),
                        ))
                    }
                }
            }
            Export::Done => None,
        }
    });
    let mut response = Response::new(Body::from_stream(body));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        "text/csv; charset=utf-8".parse().unwrap(),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        "attachment; filename=\"events.csv\"".parse().unwrap(),
    );
    Ok(response)
}

struct ExportSource {
    pool: PgPool,
    workspace: WorkspaceId,
    request: ExploreScopeRequest,
    filters: Vec<ExploreFilter>,
    limits: ExploreLimits,
    reveal: bool,
    /// More events matched than the row limit.
    truncated: bool,
}

impl ExportSource {
    fn tail_text(&self) -> String {
        if self.truncated {
            format!(
                "# truncated: the export stops at the row limit of {} rows\n",
                self.limits.row_limit
            )
        } else {
            String::new()
        }
    }

    fn tail(&self) -> Bytes {
        Bytes::from(self.tail_text())
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use time::macros::datetime;
    use zirv_flow_core::events::{PropertyType, SchemaProperty};

    use super::*;

    fn scope_request(filters: Vec<ExploreFilterRequest>) -> ExploreScopeRequest {
        ExploreScopeRequest {
            name: Some("order".into()),
            filters,
            from: datetime!(2026-06-01 0:00 UTC),
            to: datetime!(2026-06-08 0:00 UTC),
            person_id: None,
        }
    }

    fn filter(op: &str) -> ExploreFilterRequest {
        ExploreFilterRequest {
            key: "plan".into(),
            op: op.into(),
            value: Some(json!("pro")),
        }
    }

    fn row(properties: Value, schema: Option<Vec<SchemaProperty>>) -> ExploreRow {
        ExploreRow {
            id: Uuid::nil(),
            occurred_at: datetime!(2026-06-01 12:00 UTC),
            name: "=cmd".into(),
            source: "web".into(),
            person_id: Uuid::nil(),
            validation_state: "valid".into(),
            flags: vec!["late".into(), "replayed".into()],
            trace_id: Uuid::nil(),
            properties: properties.as_object().cloned().unwrap(),
            schema: schema.map(std::sync::Arc::new),
        }
    }

    fn declared(name: &str, pii: bool) -> SchemaProperty {
        SchemaProperty {
            name: name.into(),
            property_type: PropertyType::String,
            required: false,
            enum_values: None,
            description: String::new(),
            pii,
        }
    }

    #[test]
    fn the_limits_default_to_the_documented_values() {
        let limits = ExplorerLimits::default();

        assert_eq!(limits.interactive_timeout, Duration::from_millis(5_000));
        assert_eq!(limits.export_timeout, Duration::from_millis(60_000));
        assert_eq!(limits.interactive_row_limit, 10_000);
        assert_eq!(limits.export_row_limit, 1_000_000);
        assert_eq!(limits.interactive().row_limit, 10_000);
        assert_eq!(limits.export().timeout, Duration::from_secs(60));
    }

    #[test]
    fn filters_parse_their_operator_and_an_unknown_one_is_a_422() {
        let ok = scope_request(vec![filter("eq"), filter("gte")])
            .filters()
            .unwrap();
        let bad = scope_request(vec![filter("like")]).filters().unwrap_err();

        assert_eq!(ok.len(), 2);
        assert_eq!(ok[1].op, FilterOp::Gte);
        assert_eq!(bad.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn a_cursor_round_trips_to_the_microsecond_and_garbage_does_not_decode() {
        let cursor = RowCursor {
            at: OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_123_456_000).unwrap(),
            id: Uuid::now_v7(),
        };

        let text = encode_cursor(cursor);

        assert_eq!(decode_cursor(&text), Some(cursor));
        assert!(!text.contains(['=', '+', '/']));
        for bad in [
            "",
            "!!",
            &URL_SAFE_NO_PAD.encode("1:x"),
            &URL_SAFE_NO_PAD.encode("x:y"),
        ] {
            assert_eq!(decode_cursor(bad), None);
        }
    }

    #[test]
    fn rows_are_masked_for_those_without_access_in_the_list_and_the_csv() {
        let schema = vec![declared("email", true), declared("plan", false)];
        let properties = json!({"email": "a@example.com", "plan": "pro"});

        let hidden = event_body(row(properties.clone(), Some(schema.clone())), false);
        let shown = event_body(row(properties.clone(), Some(schema.clone())), true);
        let csv_hidden = export_lines(vec![row(properties.clone(), Some(schema.clone()))], false);
        let csv_shown = export_lines(vec![row(properties, Some(schema))], true);

        assert_eq!(hidden.properties["email"], Value::Null);
        assert_eq!(hidden.masked, ["email"]);
        assert_eq!(shown.properties["email"], "a@example.com");
        assert!(!csv_hidden.contains("a@example.com"));
        assert!(csv_shown.contains("a@example.com"));
        assert_eq!(hidden.trace_id.len(), 32);
    }

    #[test]
    fn csv_cells_that_would_run_as_formulas_are_made_text_and_commas_are_quoted() {
        let line = export_lines(vec![row(json!({"a": "x,y"}), None)], true);

        assert!(
            line.starts_with(
                "00000000-0000-0000-0000-000000000000,2026-06-01T12:00:00Z,'=cmd,web,"
            )
        );
        assert!(line.contains("late|replayed"));
        assert!(line.contains("\"{\"\"a\"\":null}\"") || line.contains("x,y"));
        assert_eq!(cell("plain"), "plain");
        assert_eq!(cell("@sum"), "'@sum");
        assert_eq!(cell("a,b"), "\"a,b\"");
    }

    #[test]
    fn the_audit_shape_holds_operators_and_counts_only() {
        let request = scope_request(vec![filter("eq"), filter("prefix")]);
        let filters = request.filters().unwrap();

        let shape = audit_shape(&request, &filters);

        assert_eq!(shape["filter_ops"], json!(["eq", "prefix"]));
        assert_eq!(shape["named"], true);
        assert_eq!(shape["range_days"], 7);
        let text = shape.to_string();
        assert!(!text.contains("order") && !text.contains("pro") && !text.contains("plan"));
    }
}
