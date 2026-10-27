//! A segment's members and their history, and "refresh now" (architecture 8.3 and 8.4).
//! Members and history are read in one snapshot with the segment's data age.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{Cause, TraceContext, WorkspaceId};
use zirv_flow_store::{HistoryCursor, RefreshRequest, SegmentError, Tx};

use super::preview::{DataAgeBody, read_error};
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, may_view_pii, path_id};
use crate::jobs_admin::{DEFAULT_LIMIT, parse_limit};
use crate::people::search::DisplayBody;
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct SegmentMemberBody {
    /// The person.
    id: Uuid,
    /// The strongest identifier; null for a person with none. The value is masked for callers
    /// who may not see personal values.
    display: Option<DisplayBody>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    entered_at: OffsetDateTime,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SegmentMembersBody {
    /// The current members by person id.
    items: Vec<SegmentMemberBody>,
    /// Pass as `cursor` for the next page; null when there is none.
    next_cursor: Option<String>,
    /// When the members were last computed; null before the first refresh.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    computed_at: Option<OffsetDateTime>,
    /// How current the data behind the members is.
    data_age: DataAgeBody,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SegmentHistoryItemBody {
    person_id: Uuid,
    display: Option<DisplayBody>,
    /// `entered` or `exited`.
    #[schema(example = "entered")]
    kind: &'static str,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    at: OffsetDateTime,
    /// `initial`, `definition_change`, `data_change`, `time` or `manual`.
    #[schema(example = "time")]
    cause: String,
    /// The version that entered the person, or whose refresh closed the row.
    version: i32,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SegmentHistoryBody {
    /// Entries and exits, newest first.
    items: Vec<SegmentHistoryItemBody>,
    /// Pass as `cursor` for the next page; null when there is none.
    next_cursor: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RefreshQueuedBody {
    /// True when a refresh was queued; false when a manual refresh was already waiting.
    queued: bool,
}

/// `?cursor&limit`.
fn parse_page(query: &str) -> Result<(u32, Option<String>), ApiError> {
    let mut limit = DEFAULT_LIMIT;
    let mut cursor = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "limit" => limit = parse_limit(&value)?,
            "cursor" => cursor = Some(value.into_owned()),
            _ => {}
        }
    }
    Ok((limit, cursor))
}

fn bad_cursor() -> ApiError {
    ApiError::bad_request("cursor is not a valid cursor")
}

fn encode_history_cursor(cursor: HistoryCursor) -> String {
    let micros = cursor.at.unix_timestamp_nanos() / 1000;
    URL_SAFE_NO_PAD.encode(format!("{micros}:{}:{}", cursor.row_id, cursor.kind))
}

fn decode_history_cursor(text: &str) -> Option<HistoryCursor> {
    let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let mut parts = text.split(':');
    let micros = parts.next()?.parse::<i128>().ok()?;
    let row_id = parts.next()?.parse::<i64>().ok()?;
    let kind = parts
        .next()?
        .parse::<i16>()
        .ok()
        .filter(|k| (0..=1).contains(k))?;
    if parts.next().is_some() {
        return None;
    }
    Some(HistoryCursor {
        at: OffsetDateTime::from_unix_timestamp_nanos(micros.checked_mul(1000)?).ok()?,
        row_id,
        kind,
    })
}

fn display(value: Option<(String, String)>, reveal: bool) -> Option<DisplayBody> {
    value.map(|(kind, value)| DisplayBody::new(kind, value, reveal))
}

/// Opens the read snapshot and checks the segment is in the workspace.
async fn snapshot(
    state: &AppState,
    workspace: WorkspaceId,
    segment: Uuid,
) -> Result<(Tx, zirv_flow_store::Segment), ApiError> {
    let mut tx =
        zirv_flow_store::begin_segment_snapshot(&state.pool, state.segment_preview_timeout)
            .await
            .map_err(read_error)?;
    let found = zirv_flow_store::find_segment(&mut *tx, workspace, segment)
        .await
        .map_err(read_error)?
        .ok_or_else(ApiError::not_found)?;
    Ok((tx, found))
}

/// A page of the segment's current members by person id, with when they were computed and how
/// old the data behind them is. Identifier values are shown to the owner, workspace admins and
/// `admin` keys and masked for everyone else.
#[utoipa::path(
    get,
    operation_id = "list_segment_members",
    path = "/api/v1/workspaces/{ws}/segments/{id}/members",
    tag = "segments",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Segment id"),
        ("limit" = Option<u32>, Query, description = "Members per page, 1 to 200; default 50"),
        ("cursor" = Option<String>, Query, description = "The next_cursor of the previous page")
    ),
    responses(
        (status = 200, description = "A page of members", body = SegmentMembersBody),
        (status = 400, description = "limit is out of range, or cursor is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or segment, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The read ran past its time limit (`code` is `segment_preview_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn members(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<SegmentMembersBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let segment: Uuid = path_id(&id)?;
    let (limit, cursor) = parse_page(query.as_deref().unwrap_or_default())?;
    let after = cursor
        .map(|text| Uuid::parse_str(&text).map_err(|_| bad_cursor()))
        .transpose()?;
    let reveal = may_view_pii(&state, &principal, workspace.id).await;

    let (mut tx, found) = snapshot(&state, workspace.id, segment).await?;
    let (rows, more) =
        zirv_flow_store::segment_members_page(&mut tx, workspace.id, segment, after, limit)
            .await
            .map_err(read_error)?;
    let version = zirv_flow_store::find_segment_version(
        &mut *tx,
        workspace.id,
        segment,
        found.current_version,
    )
    .await
    .map_err(read_error)?
    .ok_or_else(ApiError::internal)?;
    let data_age = zirv_flow_store::segment_data_age(&mut tx, workspace.id, &version.deps)
        .await
        .map_err(read_error)?;
    drop(tx);

    let next_cursor = rows
        .last()
        .filter(|_| more)
        .map(|row| row.person_id.to_string());
    Ok(Json(SegmentMembersBody {
        items: rows
            .into_iter()
            .map(|row| SegmentMemberBody {
                id: row.person_id,
                display: display(row.display, reveal),
                entered_at: row.entered_at,
            })
            .collect(),
        next_cursor,
        computed_at: found.computed_at,
        data_age: data_age.into(),
    }))
}

/// Entries into and exits from the segment, newest first, each with its cause and version.
#[utoipa::path(
    get,
    operation_id = "list_segment_history",
    path = "/api/v1/workspaces/{ws}/segments/{id}/history",
    tag = "segments",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Segment id"),
        ("limit" = Option<u32>, Query, description = "Items per page, 1 to 200; default 50"),
        ("cursor" = Option<String>, Query, description = "The next_cursor of the previous page")
    ),
    responses(
        (status = 200, description = "A page of the history", body = SegmentHistoryBody),
        (status = 400, description = "limit is out of range, or cursor is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or segment, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The read ran past its time limit (`code` is `segment_preview_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn history(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<SegmentHistoryBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let segment: Uuid = path_id(&id)?;
    let (limit, cursor) = parse_page(query.as_deref().unwrap_or_default())?;
    let after = cursor
        .map(|text| decode_history_cursor(&text).ok_or_else(bad_cursor))
        .transpose()?;
    let reveal = may_view_pii(&state, &principal, workspace.id).await;

    let (mut tx, _) = snapshot(&state, workspace.id, segment).await?;
    let (rows, more) =
        zirv_flow_store::segment_history_page(&mut tx, workspace.id, segment, after, limit)
            .await
            .map_err(read_error)?;
    drop(tx);

    let next_cursor = rows
        .last()
        .filter(|_| more)
        .map(|row| encode_history_cursor(HistoryCursor::of(row)));
    Ok(Json(SegmentHistoryBody {
        items: rows
            .into_iter()
            .map(|row| SegmentHistoryItemBody {
                person_id: row.person_id,
                display: display(row.display, reveal),
                kind: row.kind,
                at: row.at,
                cause: row.cause,
                version: row.version,
            })
            .collect(),
        next_cursor,
    }))
}

/// Queues a full refresh of the segment now. The trace's cause is `manual`: the signed-in user,
/// or the request for an API key. Audited as `segment.refresh_requested`.
#[utoipa::path(
    post,
    operation_id = "refresh_segment",
    path = "/api/v1/workspaces/{ws}/segments/{id}/refresh",
    tag = "segments",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Segment id")
    ),
    responses(
        (status = 202, description = "The refresh is queued", body = RefreshQueuedBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or segment, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The segment is archived (`code` is `segment_archived`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn refresh(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<(StatusCode, Json<RefreshQueuedBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Edit).await?;
    let segment: Uuid = path_id(&id)?;
    let cause = match principal.user_id() {
        Some(user) => Cause::manual(user),
        None => Cause::api(crate::request_id::current().unwrap_or_default()),
    };
    let trace = TraceContext::root(cause);

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let found = zirv_flow_store::find_segment(&mut *tx, workspace.id, segment)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if found.archived_at.is_some() {
        return Err(super::segment_error(SegmentError::Archived));
    }
    let queued = zirv_flow_store::enqueue_refresh(
        &mut tx,
        workspace.id,
        segment,
        RefreshRequest::Manual,
        &trace,
    )
    .await?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "segment.refresh_requested",
        Some(("segment", segment)),
        json!({ "name": crate::admin::audit_text(&found.name), "queued": queued }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((StatusCode::ACCEPTED, Json(RefreshQueuedBody { queued })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_takes_a_limit_up_to_200_and_a_cursor() {
        assert_eq!(parse_page("").unwrap(), (50, None));
        assert_eq!(
            parse_page("limit=200&cursor=abc").unwrap(),
            (200, Some("abc".into()))
        );
        for bad in ["limit=0", "limit=201", "limit=x"] {
            assert_eq!(
                parse_page(bad).unwrap_err().status(),
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_history_cursor_round_trips_and_a_malformed_one_does_not_decode() {
        let cursor = HistoryCursor {
            at: OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_123_456_000).unwrap(),
            row_id: 42,
            kind: 1,
        };

        let text = encode_history_cursor(cursor);

        assert_eq!(decode_history_cursor(&text), Some(cursor));
        assert!(!text.contains(['=', '+', '/']));
        for bad in [
            String::new(),
            "!!".into(),
            URL_SAFE_NO_PAD.encode("1:2"),
            URL_SAFE_NO_PAD.encode("1:2:7"),
            URL_SAFE_NO_PAD.encode("1:2:0:9"),
            URL_SAFE_NO_PAD.encode("x:2:0"),
        ] {
            assert_eq!(decode_history_cursor(&bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_display_is_masked_unless_the_caller_may_view_pii() {
        let shown = display(Some(("email".into(), "a@example.com".into())), true).unwrap();
        let masked = display(Some(("email".into(), "a@example.com".into())), false).unwrap();

        assert_eq!(shown.value.as_deref(), Some("a@example.com"));
        assert!(masked.value.is_none() && masked.masked);
        assert!(display(None, true).is_none());
    }
}
