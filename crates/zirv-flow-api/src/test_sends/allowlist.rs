//! The test-send allowlist: addresses a test may go to besides the members'. Admin only. The list
//! shows a masked hint, and the audit entries hold no address and no hash.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::people::{Identifier, identifier_hash};
use zirv_flow_store::AllowlistEntry;

use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct AllowlistEntryBody {
    id: Uuid,
    /// The first character, `***`, then `@domain`. The address itself is not stored.
    #[schema(example = "q***@example.com")]
    display_hint: String,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
}

impl From<AllowlistEntry> for AllowlistEntryBody {
    fn from(entry: AllowlistEntry) -> Self {
        Self {
            id: entry.id,
            display_hint: entry.display_hint,
            created_at: entry.created_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AllowlistBody {
    entries: Vec<AllowlistEntryBody>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct AllowlistAddRequest {
    /// The email address a test send may go to.
    #[schema(example = "qa@example.com")]
    address: String,
}

/// The workspace's test-send allowlist, oldest first.
#[utoipa::path(
    get,
    operation_id = "list_test_send_allowlist",
    path = "/api/v1/workspaces/{ws}/test-send-allowlist",
    tag = "templates",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The entries", body = AllowlistBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a key without the admin scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn allowlist_list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<AllowlistBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let entries = zirv_flow_store::list_allowlist(&state.pool, workspace.id).await?;
    Ok(Json(AllowlistBody {
        entries: entries.into_iter().map(Into::into).collect(),
    }))
}

/// Allows test sends to an address. The address is hashed here and only the hash and a masked hint
/// are stored. Needs the workspace admin role and the CSRF token. Audited as
/// `test_send.allowlist_added`, with the entry id and nothing else.
#[utoipa::path(
    post,
    operation_id = "add_test_send_allowlist_entry",
    path = "/api/v1/workspaces/{ws}/test-send-allowlist",
    tag = "templates",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = AllowlistAddRequest,
    responses(
        (status = 201, description = "The entry", body = AllowlistEntryBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The address is on the list already", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The address is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn allowlist_add(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<AllowlistAddRequest>,
) -> Result<(StatusCode, Json<AllowlistEntryBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let key = state.identifier_key.as_deref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "test sends are unavailable: the server has no master key",
        )
    })?;
    let identifier = Identifier::parse("email", &body.address)
        .map_err(|_| ApiError::unprocessable("the address is not a valid email address"))?;
    let hash = identifier_hash(key, &identifier);
    let hint = zirv_flow_store::masked_hint(&body.address);
    let id = Uuid::now_v7();
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let entry = zirv_flow_store::add_allowlist_entry(&mut tx, workspace.id, id, &hash, &hint)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::CONFLICT,
                "the address is on the allowlist already",
            )
        })?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "test_send.allowlist_added",
        Some(("test_send_allowlist_entry", id)),
        json!({}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((StatusCode::CREATED, Json(entry.into())))
}

/// Removes an entry from the allowlist. Needs the workspace admin role and the CSRF token. Audited
/// as `test_send.allowlist_removed`, with the entry id and nothing else.
#[utoipa::path(
    delete,
    operation_id = "delete_test_send_allowlist_entry",
    path = "/api/v1/workspaces/{ws}/test-send-allowlist/{id}",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Entry id")
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or entry, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn allowlist_delete(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let id: Uuid = path_id(&id)?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    if !zirv_flow_store::delete_allowlist_entry(&mut tx, workspace.id, id).await? {
        return Err(ApiError::not_found());
    }
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "test_send.allowlist_removed",
        Some(("test_send_allowlist_entry", id)),
        json!({}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_is_shown_with_its_hint_and_nothing_else() {
        let body = AllowlistEntryBody::from(AllowlistEntry {
            id: Uuid::nil(),
            display_hint: "q***@example.com".into(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        });

        let json = serde_json::to_value(&body).unwrap();

        assert_eq!(json["display_hint"], "q***@example.com");
        assert_eq!(json.as_object().unwrap().len(), 3);
    }
}
