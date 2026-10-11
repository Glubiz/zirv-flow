//! A person's lists and the history of their memberships.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde::Serialize;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_store::{MembershipChange, PersonList};

use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::jobs_admin::{encode_cursor, parse_page};
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct PersonListBody {
    list_id: Uuid,
    name: String,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    added_at: OffsetDateTime,
    /// `ui`, `api`, `import`, `merge` or `flow`.
    source: String,
}

impl From<PersonList> for PersonListBody {
    fn from(list: PersonList) -> Self {
        Self {
            list_id: list.list_id,
            name: list.name,
            added_at: list.added_at,
            source: list.source,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MembershipChangeBody {
    id: Uuid,
    list_id: Uuid,
    list_name: String,
    /// `added` or `removed`.
    #[schema(example = "added")]
    action: String,
    /// `ui`, `api`, `import`, `merge` or `flow`.
    source: String,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    occurred_at: OffsetDateTime,
    /// The user who made the change; null when an API key, an import job or a flow did.
    actor_user_id: Option<Uuid>,
    /// Look it up with `GET /api/v1/workspaces/{ws}/traces/{trace_id}`.
    trace_id: Uuid,
}

impl From<MembershipChange> for MembershipChangeBody {
    fn from(change: MembershipChange) -> Self {
        Self {
            id: change.id,
            list_id: change.list_id,
            list_name: change.list_name,
            action: change.action,
            source: change.source,
            occurred_at: change.occurred_at,
            actor_user_id: change.actor_user_id,
            trace_id: change.trace_id,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MembershipHistoryBody {
    /// Newest first.
    changes: Vec<MembershipChangeBody>,
    /// Pass as `before` for the next page; null when there is none.
    next_before: Option<String>,
}

/// The lists a person is in, ordered by name. A person merged into another is read as the
/// person they were merged into.
#[utoipa::path(
    get,
    operation_id = "list_person_lists",
    path = "/api/v1/workspaces/{ws}/people/{id}/lists",
    tag = "lists",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Person id")
    ),
    responses(
        (status = 200, description = "The lists", body = Vec<PersonListBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or person, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn lists(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<Vec<PersonListBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let person: Uuid = path_id(&id)?;
    zirv_flow_store::find_person(&state.pool, workspace.id, person)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let lists = zirv_flow_store::lists_for(&state.pool, workspace.id, person).await?;
    Ok(Json(lists.into_iter().map(PersonListBody::from).collect()))
}

/// Every time a person was added to or removed from a list, newest first. Merges keep the
/// history of both people under the survivor.
#[utoipa::path(
    get,
    operation_id = "list_person_list_history",
    path = "/api/v1/workspaces/{ws}/people/{id}/lists/history",
    tag = "lists",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Person id"),
        ("limit" = Option<u32>, Query, description = "Changes per page, 1 to 200; default 50"),
        ("before" = Option<String>, Query, description = "The next_before of the previous page")
    ),
    responses(
        (status = 200, description = "A page of changes", body = MembershipHistoryBody),
        (status = 400, description = "limit is out of range, or before is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or person, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn history(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<MembershipHistoryBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let person: Uuid = path_id(&id)?;
    let (limit, before) = parse_page(query.as_deref().unwrap_or_default())?;
    zirv_flow_store::find_person(&state.pool, workspace.id, person)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let (changes, more) =
        zirv_flow_store::membership_history(&state.pool, workspace.id, person, limit, before)
            .await?;
    let next_before = match changes.last() {
        Some(last) if more => Some(encode_cursor((last.occurred_at, last.id))),
        _ => None,
    };
    Ok(Json(MembershipHistoryBody {
        changes: changes
            .into_iter()
            .map(MembershipChangeBody::from)
            .collect(),
        next_before,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_shows_its_list_actor_and_trace_and_no_person_data() {
        let change = MembershipChange {
            id: Uuid::now_v7(),
            list_id: Uuid::now_v7(),
            list_name: "VIPs".into(),
            person_id: Uuid::now_v7(),
            action: "added".into(),
            source: "ui".into(),
            occurred_at: OffsetDateTime::UNIX_EPOCH,
            actor_user_id: None,
            trace_id: Uuid::now_v7(),
        };

        let json = serde_json::to_value(MembershipChangeBody::from(change)).unwrap();

        assert_eq!(json["list_name"], "VIPs");
        assert_eq!(json["action"], "added");
        assert!(json["actor_user_id"].is_null());
        assert!(json.get("person_id").is_none());
        assert_eq!(json["occurred_at"], "1970-01-01T00:00:00Z");
    }

    #[test]
    fn a_persons_list_shows_when_and_how_they_joined() {
        let list = PersonList {
            list_id: Uuid::now_v7(),
            name: "VIPs".into(),
            added_at: OffsetDateTime::UNIX_EPOCH,
            source: "merge".into(),
        };

        let json = serde_json::to_value(PersonListBody::from(list)).unwrap();

        assert_eq!(json["source"], "merge");
        assert_eq!(json["name"], "VIPs");
    }
}
