//! Calls into a destination: checking that its settings and credential work. The check opens the
//! stored credential right before the call and passes it to the connector as a typed credential.
//! It delivers nothing and takes no token from the rate bucket.

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Serialize;
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_connectors::destinations::DestinationContext;
use zirv_flow_connectors::http::HttpLimits;
use zirv_flow_core::WorkspaceId;

use super::{archived, credential, kind_unavailable, registry};
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};

/// The longest a check may take.
pub(super) const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Serialize, ToSchema)]
pub(crate) struct DestinationCheckBody {
    /// Whether the settings and the credential work.
    ok: bool,
    /// Why they do not. Never holds a credential, an address or a URL.
    error: Option<String>,
}

/// Runs the destination's check and audits `destination.checked` with the outcome.
pub(crate) async fn check_destination(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<DestinationCheckBody, ApiError> {
    let destination = zirv_flow_store::find_destination(&state.pool, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if destination.archived_at.is_some() {
        return Err(archived());
    }
    let driver = registry()
        .get(&destination.kind)
        .ok_or_else(kind_unavailable)?;
    let credential = credential::load(state, workspace, destination.credential_secret_id).await?;
    let network = DestinationContext::new(
        state.egress_policy().clone(),
        HttpLimits::default(),
        driver.spec().kind,
        id.to_string(),
    )
    .map_err(|_| ApiError::internal())?;
    let result = tokio::time::timeout(
        CALL_TIMEOUT,
        driver.check(&network, &destination.config, &credential),
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "the destination did not answer in time",
        )
    })?;
    let body = DestinationCheckBody {
        ok: result.is_ok(),
        error: result.err().map(|error| error.to_string()),
    };
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "destination.checked",
        Some(("destination", id)),
        json!({"ok": body.ok}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(body)
}

/// Checks the destination's settings and stored credential without delivering anything. A failed
/// check is a 200 with `ok` false and the reason. Workspace admins only. Audited as
/// `destination.checked`.
#[utoipa::path(
    post,
    operation_id = "check_destination",
    path = "/api/v1/workspaces/{ws}/destinations/{id}/check",
    tag = "destinations",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Destination id")
    ),
    responses(
        (status = 200, description = "The outcome of the check", body = DestinationCheckBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or destination, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The destination is archived (`code` is `destination_archived`), or its type is not in this build (`destination_kind_unavailable`)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to open the credential", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "The destination did not answer in time", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn check(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<DestinationCheckBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let body = check_destination(&state, &principal.context(), workspace.id, path_id(&id)?).await?;
    Ok(Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_check_body_carries_the_outcome_and_the_reason_only() {
        let failed = serde_json::to_value(DestinationCheckBody {
            ok: false,
            error: Some("the destination refused the credential".into()),
        })
        .unwrap();
        let passed = serde_json::to_value(DestinationCheckBody {
            ok: true,
            error: None,
        })
        .unwrap();

        assert_eq!(
            failed,
            json!({"ok": false, "error": "the destination refused the credential"})
        );
        assert_eq!(passed, json!({"ok": true, "error": null}));
    }
}
