//! The members of a list: the page of them, and adding and removing by person id or by
//! identifier. Identifiers resolve to existing people only; nobody is created here.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::people::Identifier;
use zirv_flow_store::{ListMember, MAX_PEOPLE_PER_CALL};

use super::{capped, invalid, membership_error, source_of};
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, may_view_pii, path_id};
use crate::jobs_admin::{encode_cursor, parse_page};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct MemberIdentifierBody {
    #[schema(example = "email")]
    kind: String,
    /// Null when `masked`: only workspace admins, the owner and `admin` keys see values.
    value: Option<String>,
    masked: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ListMemberBody {
    person_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    added_at: OffsetDateTime,
    /// `ui`, `api`, `import`, `merge`, `flow` or `sync`.
    #[schema(example = "ui")]
    source: String,
    /// The strongest identifier the person holds; null for a person with none.
    identifier: Option<MemberIdentifierBody>,
}

impl ListMemberBody {
    fn new(member: ListMember, reveal: bool) -> Self {
        Self {
            person_id: member.person_id,
            added_at: member.added_at,
            source: member.source,
            identifier: member.identifier.map(|identifier| MemberIdentifierBody {
                kind: identifier.kind,
                value: reveal.then_some(identifier.value),
                masked: !reveal,
            }),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ListMembersBody {
    /// Newest first.
    members: Vec<ListMemberBody>,
    /// Pass as `cursor` for the next page; null when there is none.
    next_cursor: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct IdentifierInput {
    /// `email`, `phone`, `anonymous_id` or `external:<namespace>`.
    #[schema(example = "email")]
    kind: String,
    #[schema(example = "ada@example.com")]
    value: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct AddMembersRequest {
    /// People by id. Together with `identifiers`, at most 1000.
    #[serde(default)]
    person_ids: Vec<Uuid>,
    /// People by identifier; one that no person owns is reported, never created.
    #[serde(default)]
    identifiers: Vec<IdentifierInput>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct RemoveMembersRequest {
    /// At most 1000.
    person_ids: Vec<Uuid>,
}

/// An input that named no person.
#[derive(Debug, PartialEq, Serialize, ToSchema)]
pub(crate) struct NotFoundBody {
    /// The request field: `person_ids` or `identifiers`.
    #[schema(example = "identifiers")]
    field: &'static str,
    /// Position in that field, from 0.
    index: usize,
    #[schema(example = "not_found")]
    reason: &'static str,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AddMembersBody {
    /// People newly added.
    added: usize,
    /// People who were members already, duplicates in the request included.
    already_member: usize,
    /// The first 1000 inputs that named no person.
    not_found: Vec<NotFoundBody>,
    /// How many inputs named no person, however many `not_found` lists.
    not_found_count: usize,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RemoveMembersBody {
    /// People removed.
    removed: usize,
    /// People who were not members.
    not_member: usize,
    /// The first 1000 inputs that named no person.
    not_found: Vec<NotFoundBody>,
    /// How many inputs named no person, however many `not_found` lists.
    not_found_count: usize,
}

fn not_found(field: &'static str, index: usize) -> NotFoundBody {
    NotFoundBody {
        field,
        index,
        reason: "not_found",
    }
}

/// Parses each identifier. A malformed one is a 422 that names its position and the rule, never
/// the value.
fn parse_identifiers(inputs: &[IdentifierInput]) -> Result<Vec<Identifier>, ApiError> {
    inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            Identifier::parse(&input.kind, &input.value).map_err(|error| {
                ApiError::unprocessable(format!("identifiers[{index}]: {}", error.reason()))
            })
        })
        .collect()
}

/// A page of the members, newest first, with each one's strongest identifier. Values are shown
/// to the owner, workspace admins and `admin` keys, and masked for everyone else.
#[utoipa::path(
    get,
    operation_id = "list_list_members",
    path = "/api/v1/workspaces/{ws}/lists/{id}/members",
    tag = "lists",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "List id"),
        ("limit" = Option<u32>, Query, description = "Members per page, 1 to 200; default 50"),
        ("cursor" = Option<String>, Query, description = "The next_cursor of the previous page")
    ),
    responses(
        (status = 200, description = "A page of members", body = ListMembersBody),
        (status = 400, description = "limit is out of range, or cursor is malformed; the removed before and after answer paging_parameter_renamed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or list, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn page(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<ListMembersBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let list: Uuid = path_id(&id)?;
    let (limit, before) = parse_page(query.as_deref().unwrap_or_default())?;
    zirv_flow_store::find_list(&state.pool, workspace.id, list)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let (members, more) =
        zirv_flow_store::members(&state.pool, workspace.id, list, limit, before).await?;
    let next_cursor = match members.last() {
        Some(last) if more => Some(encode_cursor((last.added_at, last.person_id))),
        _ => None,
    };
    let reveal = may_view_pii(&state, &principal, workspace.id).await;
    Ok(Json(ListMembersBody {
        members: members
            .into_iter()
            .map(|member| ListMemberBody::new(member, reveal))
            .collect(),
        next_cursor,
    }))
}

/// Adds people by id or by identifier. Adding a member again changes nothing. Identifiers
/// that no person owns come back in `not_found` and never create a person. Membership changes
/// go to the people's history and the decision log, not the audit log.
#[utoipa::path(
    post,
    operation_id = "add_list_members",
    path = "/api/v1/workspaces/{ws}/lists/{id}/members",
    tag = "lists",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "List id")
    ),
    request_body = AddMembersRequest,
    responses(
        (status = 200, description = "What was added", body = AddMembersBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or list, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The people changed at the same moment; try again", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "More than 1000 people, or a malformed identifier (the detail names its position and the rule)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn add(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<AddMembersRequest>,
) -> Result<Json<AddMembersBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Edit).await?;
    let list: Uuid = path_id(&id)?;
    if body.person_ids.len() + body.identifiers.len() > MAX_PEOPLE_PER_CALL {
        return Err(invalid("people", "a call takes at most 1000 people").into());
    }
    let identifiers = parse_identifiers(&body.identifiers)?;

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let owners =
        zirv_flow_store::find_people_by_identifiers(&mut *tx, workspace.id, &identifiers).await?;
    let mut people = body.person_ids.clone();
    let mut unknown = Vec::new();
    for (index, owner) in owners.into_iter().enumerate() {
        match owner {
            Some(person) => people.push(person),
            None => unknown.push(not_found("identifiers", index)),
        }
    }
    let outcome = zirv_flow_store::add_members(
        &mut tx,
        workspace.id,
        list,
        &people,
        source_of(&principal),
        principal.user_id(),
        &crate::trace::request_trace(),
    )
    .await
    .map_err(|error| ApiError::from(membership_error(error)))?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "list.members_changed",
        Some(("list", list)),
        json!({"change": "added", "count": outcome.changed}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;

    let mut not_found_list: Vec<NotFoundBody> = outcome
        .missing
        .iter()
        .map(|&position| not_found("person_ids", position))
        .collect();
    not_found_list.extend(unknown);
    let (not_found, not_found_count) = capped(not_found_list);
    Ok(Json(AddMembersBody {
        added: outcome.changed,
        already_member: outcome.unchanged,
        not_found,
        not_found_count,
    }))
}

/// Removes people by id. Removing someone who is not a member changes nothing.
#[utoipa::path(
    post,
    operation_id = "remove_list_members",
    path = "/api/v1/workspaces/{ws}/lists/{id}/members/remove",
    tag = "lists",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "List id")
    ),
    request_body = RemoveMembersRequest,
    responses(
        (status = 200, description = "What was removed", body = RemoveMembersBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or list, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The people changed at the same moment; try again", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "More than 1000 people", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<RemoveMembersRequest>,
) -> Result<Json<RemoveMembersBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Edit).await?;
    let list: Uuid = path_id(&id)?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let outcome = zirv_flow_store::remove_members(
        &mut tx,
        workspace.id,
        list,
        &body.person_ids,
        source_of(&principal),
        principal.user_id(),
        &crate::trace::request_trace(),
    )
    .await
    .map_err(|error| ApiError::from(membership_error(error)))?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "list.members_changed",
        Some(("list", list)),
        json!({"change": "removed", "count": outcome.changed}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    let missing = outcome
        .missing
        .iter()
        .map(|&position| not_found("person_ids", position))
        .collect();
    let (not_found, not_found_count) = capped(missing);
    Ok(Json(RemoveMembersBody {
        removed: outcome.changed,
        not_member: outcome.unchanged,
        not_found,
        not_found_count,
    }))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use zirv_flow_store::MemberIdentifier;

    use super::*;

    fn member() -> ListMember {
        ListMember {
            person_id: Uuid::now_v7(),
            added_at: OffsetDateTime::UNIX_EPOCH,
            source: "ui".into(),
            identifier: Some(MemberIdentifier {
                kind: "email".into(),
                value: "ada@example.com".into(),
            }),
        }
    }

    #[test]
    fn a_masked_member_shows_the_kind_and_no_value() {
        let json = serde_json::to_value(ListMemberBody::new(member(), false)).unwrap();

        assert_eq!(json["identifier"]["kind"], "email");
        assert!(json["identifier"]["value"].is_null());
        assert_eq!(json["identifier"]["masked"], true);
        assert!(!json.to_string().contains("ada@example.com"));
    }

    #[test]
    fn a_revealed_member_shows_the_value() {
        let json = serde_json::to_value(ListMemberBody::new(member(), true)).unwrap();

        assert_eq!(json["identifier"]["value"], "ada@example.com");
        assert_eq!(json["identifier"]["masked"], false);
    }

    #[test]
    fn a_member_without_an_identifier_has_a_null_one() {
        let mut bare = member();
        bare.identifier = None;

        let json = serde_json::to_value(ListMemberBody::new(bare, false)).unwrap();

        assert!(json["identifier"].is_null());
    }

    #[test]
    fn identifiers_are_normalized_and_a_bad_one_is_named_by_position_and_rule_only() {
        let inputs = [
            IdentifierInput {
                kind: "email".into(),
                value: " Ada@Example.com ".into(),
            },
            IdentifierInput {
                kind: "email".into(),
                value: "secret-not-an-email".into(),
            },
        ];

        assert_eq!(
            parse_identifiers(&inputs[..1]).unwrap()[0].value(),
            "ada@example.com"
        );
        let error = parse_identifiers(&inputs).unwrap_err();
        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.detail(), "identifiers[1]: invalid_email");
        let unknown = [IdentifierInput {
            kind: "fax".into(),
            value: "x".into(),
        }];
        assert_eq!(
            parse_identifiers(&unknown).unwrap_err().detail(),
            "identifiers[0]: unknown_identifier_kind"
        );
    }

    #[test]
    fn an_add_request_may_leave_either_field_out() {
        let request: AddMembersRequest = serde_json::from_str("{}").unwrap();

        assert!(request.person_ids.is_empty());
        assert!(request.identifiers.is_empty());
        assert!(serde_json::from_str::<RemoveMembersRequest>("{}").is_err());
    }

    #[test]
    fn a_not_found_input_names_its_field_and_position() {
        assert_eq!(
            serde_json::to_value(not_found("identifiers", 2)).unwrap(),
            serde_json::json!({"field": "identifiers", "index": 2, "reason": "not_found"})
        );
    }
}
