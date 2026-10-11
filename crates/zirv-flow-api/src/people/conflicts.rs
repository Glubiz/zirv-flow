//! Identity conflict review: the conflicts a merge was refused for, the identities of the two
//! people, and the two ways to close one.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::people::BlockReason;
use zirv_flow_core::{ValidationError, WorkspaceId};
use zirv_flow_store::{ConflictResolution, IdentityConflict, PgPool, ResolveError};

use crate::AppState;
use crate::admin::{self, AdminError, Context};
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::jobs_admin::{DEFAULT_LIMIT, decode_cursor, encode_cursor, parse_limit};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct ConflictBody {
    id: Uuid,
    /// The identifier kind the two people share, such as `email`.
    #[schema(example = "email")]
    kind: String,
    /// The person the record attached to.
    person_id: Uuid,
    /// The other person that owns the shared identifier.
    other_person_id: Uuid,
    /// Why the people were not merged: `unique_kind`, `max_per_person`, or `merge_deferred`
    /// on conflicts recorded before merges existed.
    #[schema(example = "unique_kind")]
    reason: String,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    resolved_at: Option<OffsetDateTime>,
    /// `merged`, `kept_separate` or `identifier_moved`; null while open.
    resolution: Option<String>,
}

impl From<&IdentityConflict> for ConflictBody {
    fn from(conflict: &IdentityConflict) -> Self {
        Self {
            id: conflict.id,
            kind: conflict.kind.clone(),
            person_id: conflict.person_id,
            other_person_id: conflict.other_person_id,
            reason: conflict.reason.clone(),
            created_at: conflict.created_at,
            resolved_at: conflict.resolved_at,
            resolution: conflict.resolution.clone(),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ConflictsBody {
    /// Newest first.
    conflicts: Vec<ConflictBody>,
    /// Pass as `before` for the next page; null when there is none.
    next_before: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct IdentityBody {
    #[schema(example = "email")]
    kind: String,
    /// Null when `masked`: only workspace admins, the owner and `admin` keys see values.
    value: Option<String>,
    masked: bool,
    /// The identifier the conflict is about.
    shared: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct PersonIdentitiesBody {
    person_id: Uuid,
    identities: Vec<IdentityBody>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ConflictDetailBody {
    conflict: ConflictBody,
    /// The conflict's person first, then the other person; one entry when they have since
    /// been merged into one.
    people: Vec<PersonIdentitiesBody>,
}

/// How to close a conflict.
#[derive(Deserialize, ToSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum ResolveRequest {
    /// The two people are different people; the identifier stays where it is.
    KeepSeparate,
    /// The shared identifier belongs to `to_person_id`, one of the conflict's two people.
    MoveIdentifier { to_person_id: Uuid },
}

/// Whether to list resolved conflicts, the page size and the cursor of the previous page.
type PageQuery = (bool, u32, Option<(OffsetDateTime, Uuid)>);

fn parse_page(query: &str) -> Result<PageQuery, ApiError> {
    let (mut resolved, mut limit, mut before) = (false, DEFAULT_LIMIT, None);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "status" => {
                resolved = match value.as_ref() {
                    "open" => false,
                    "resolved" => true,
                    _ => return Err(ApiError::bad_request("status must be open or resolved")),
                };
            }
            "limit" => {
                limit = parse_limit(&value)?;
            }
            "before" => {
                before = Some(
                    decode_cursor(&value)
                        .ok_or_else(|| ApiError::bad_request("before is not a valid cursor"))?,
                );
            }
            _ => {}
        }
    }
    Ok((resolved, limit, before))
}

/// The detail of the 409 for a conflict whose people were merged since the caller looked. The
/// code is the text before the colon.
pub(crate) const CHANGED: &str = "conflict_changed: the people of this conflict changed, reload it";

fn block_message(reason: BlockReason) -> &'static str {
    match reason {
        BlockReason::UniqueKind => {
            "the move would give that person two different values of a kind it holds once"
        }
        BlockReason::MaxPerPerson => {
            "the move would give that person more identifiers of that kind than it may hold"
        }
    }
}

/// Closes an open conflict and audits it with ids only.
pub(crate) async fn resolve_identity_conflict(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    request: &ResolveRequest,
) -> Result<IdentityConflict, AdminError> {
    let (resolution, action) = match request {
        ResolveRequest::KeepSeparate => (
            ConflictResolution::KeepSeparate,
            "identity_conflict.kept_separate",
        ),
        ResolveRequest::MoveIdentifier { to_person_id } => (
            ConflictResolution::MoveIdentifier {
                to_person_id: *to_person_id,
            },
            "identity_conflict.identifier_moved",
        ),
    };
    let mut tx = zirv_flow_store::begin(pool).await?;
    let closed = zirv_flow_store::resolve_conflict(
        &mut tx,
        workspace,
        id,
        resolution,
        &crate::trace::request_trace(),
    )
    .await
    .map_err(|error| match error {
        ResolveError::NotFound => AdminError::NotFound("identity conflict"),
        ResolveError::AlreadyResolved => AdminError::Conflict("the conflict is already resolved"),
        ResolveError::NotAParty => AdminError::Invalid(ValidationError::new(
            "to_person_id",
            "must be one of the two people of the conflict",
        )),
        ResolveError::NoIdentifier => {
            AdminError::Conflict("the conflict does not name an identifier to move")
        }
        ResolveError::Changed => AdminError::Conflict(CHANGED),
        ResolveError::AlreadyThere => {
            AdminError::Conflict("the identifier already belongs to that person")
        }
        ResolveError::Blocked(reason) => AdminError::Conflict(block_message(reason)),
        ResolveError::Store(error) => error.into(),
    })?;
    let mut summary = json!({
        "person_id": closed.person_id,
        "other_person_id": closed.other_person_id,
    });
    if let ResolveRequest::MoveIdentifier { to_person_id } = request {
        summary["to_person_id"] = json!(to_person_id);
    }
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        action,
        Some(("identity_conflict", id)),
        summary,
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(closed)
}

/// Identity conflicts, newest first: those still open (the default) or already resolved.
#[utoipa::path(
    get,
    operation_id = "list_identity_conflicts",
    path = "/api/v1/workspaces/{ws}/identity-conflicts",
    tag = "people",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("status" = Option<String>, Query, description = "`open` (default) or `resolved`"),
        ("limit" = Option<u32>, Query, description = "Conflicts per page, 1 to 200; default 50"),
        ("before" = Option<String>, Query, description = "The next_before of the previous page")
    ),
    responses(
        (status = 200, description = "A page of conflicts", body = ConflictsBody),
        (status = 400, description = "status or limit is out of range, or before is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key with neither the read nor the admin scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<ConflictsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::ReadMembers).await?;
    let (resolved, limit, before) = parse_page(query.as_deref().unwrap_or_default())?;
    let (conflicts, more) =
        zirv_flow_store::list_conflicts(&state.pool, workspace.id, resolved, limit, before).await?;
    let next_before = match conflicts.last() {
        Some(last) if more => Some(encode_cursor((last.created_at, last.id))),
        _ => None,
    };
    Ok(Json(ConflictsBody {
        conflicts: conflicts.iter().map(ConflictBody::from).collect(),
        next_before,
    }))
}

/// One conflict and the identities of its two people. Values are shown to the owner,
/// workspace admins and `admin` keys, and masked for everyone else.
#[utoipa::path(
    get,
    operation_id = "get_identity_conflict",
    path = "/api/v1/workspaces/{ws}/identity-conflicts/{id}",
    tag = "people",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Conflict id")
    ),
    responses(
        (status = 200, description = "The conflict with both people's identities", body = ConflictDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key with neither the read nor the admin scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or conflict, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<ConflictDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::ReadMembers).await?;
    let id: Uuid = path_id(&id)?;
    let conflict = zirv_flow_store::find_conflict(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let reveal = may_see_values(&state, &principal, workspace.id).await;
    let mut people: Vec<PersonIdentitiesBody> = Vec::with_capacity(2);
    for person in [conflict.person_id, conflict.other_person_id] {
        let current = zirv_flow_store::current_person(&state.pool, workspace.id, person)
            .await?
            .unwrap_or(person);
        if people.iter().any(|group| group.person_id == current) {
            continue;
        }
        let identities = zirv_flow_store::list_identities(&state.pool, workspace.id, current)
            .await?
            .into_iter()
            .map(|identity| IdentityBody {
                shared: conflict.identity_id == Some(identity.id),
                kind: identity.kind,
                value: reveal.then_some(identity.value),
                masked: !reveal,
            })
            .collect();
        people.push(PersonIdentitiesBody {
            person_id: current,
            identities,
        });
    }
    Ok(Json(ConflictDetailBody {
        conflict: ConflictBody::from(&conflict),
        people,
    }))
}

/// Closes an open conflict: keep the two people separate, or move the shared identifier to
/// one of them. There is no unmerge. Audited as `identity_conflict.kept_separate` or
/// `identity_conflict.identifier_moved`, and recorded as an `identity.conflict.resolved`
/// decision.
#[utoipa::path(
    post,
    operation_id = "resolve_identity_conflict",
    path = "/api/v1/workspaces/{ws}/identity-conflicts/{id}/resolve",
    tag = "people",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Conflict id")
    ),
    request_body = ResolveRequest,
    responses(
        (status = 200, description = "The closed conflict", body = ConflictBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or conflict, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Already resolved, the move would break an identity rule, or the people changed since the conflict was loaded (detail starts with `conflict_changed`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "to_person_id is not one of the conflict's two people", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn resolve(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<ResolveRequest>,
) -> Result<Json<ConflictBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let id: Uuid = path_id(&id)?;
    let closed =
        resolve_identity_conflict(&state.pool, &principal.context(), workspace.id, id, &body)
            .await?;
    Ok(Json(ConflictBody::from(&closed)))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    #[test]
    fn the_page_defaults_to_open_and_fifty_and_refuses_bad_values() {
        assert_eq!(parse_page("").unwrap(), (false, 50, None));
        assert!(parse_page("status=resolved").unwrap().0);
        assert!(!parse_page("status=open&limit=200").unwrap().0);
        for bad in [
            "status=all",
            "limit=0",
            "limit=201",
            "limit=x",
            "before=%21",
        ] {
            assert_eq!(
                parse_page(bad).unwrap_err().status(),
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_page_cursor_is_the_one_the_list_hands_out() {
        let at = OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_000_001_000).unwrap();
        let id = Uuid::now_v7();

        let (_, _, before) = parse_page(&format!("before={}", encode_cursor((at, id)))).unwrap();

        assert_eq!(before, Some((at, id)));
    }

    #[test]
    fn a_resolve_request_is_tagged_by_its_outcome() {
        let person = Uuid::now_v7();

        let keep: ResolveRequest =
            serde_json::from_value(json!({"outcome": "keep_separate"})).unwrap();
        let moved: ResolveRequest =
            serde_json::from_value(json!({"outcome": "move_identifier", "to_person_id": person}))
                .unwrap();

        assert!(matches!(keep, ResolveRequest::KeepSeparate));
        assert!(matches!(
            moved,
            ResolveRequest::MoveIdentifier { to_person_id } if to_person_id == person
        ));
        for bad in [
            json!({"outcome": "move_identifier"}),
            json!({"outcome": "delete"}),
            json!({}),
        ] {
            assert!(serde_json::from_value::<ResolveRequest>(bad).is_err());
        }
    }

    #[test]
    fn the_changed_detail_starts_with_its_code() {
        assert!(CHANGED.starts_with("conflict_changed:"));
    }

    #[test]
    fn each_block_reason_has_its_own_message() {
        assert_ne!(
            block_message(BlockReason::UniqueKind),
            block_message(BlockReason::MaxPerPerson)
        );
    }
}
