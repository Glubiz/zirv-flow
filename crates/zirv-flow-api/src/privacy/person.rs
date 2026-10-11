//! Deleting a person. Deletion removes the person and every row that refers to them; it is
//! not erasure: no suppression and no ledger entry.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, WorkspaceId};
use zirv_flow_store::{DeleteReason, PgPool};

use crate::AppState;
use crate::admin::{self, AdminError, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};
use crate::trace;

/// Deletes the person (or the survivor they were merged into) with the people merged into
/// them, and audits it as `person.deleted` with the person id and nothing else.
pub(crate) async fn delete(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    person: Uuid,
    trace: &TraceContext,
) -> Result<(), AdminError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let current = zirv_flow_store::current_person(&mut *tx, workspace, person)
        .await?
        .ok_or(AdminError::NotFound("person"))?;
    zirv_flow_store::delete_person(&mut tx, workspace, current, DeleteReason::Requested, trace)
        .await?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "person.deleted",
        Some(("person", current)),
        json!({}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

/// Deletes a person with every row that refers to them: identities, source records, events,
/// consent, list memberships and their history, conflicts, merges and decisions. It adds no
/// suppression and no erasure ledger entry; use an erasure request for a data-subject request.
/// Needs the workspace admin role and the CSRF token. Audited as `person.deleted`.
#[utoipa::path(
    delete,
    operation_id = "delete_person",
    path = "/api/v1/workspaces/{ws}/people/{id}",
    tag = "privacy",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Person id")
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or person, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let trace = trace::current().ok_or_else(ApiError::internal)?;
    delete(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        &trace,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reason_of_an_operator_deletion_is_requested() {
        assert_eq!(DeleteReason::Requested.as_str(), "requested");
    }
}
