//! The workspace's privacy settings: today the orphan policy of architecture section 7.4.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use zirv_flow_core::{OrphanPolicy, WorkspaceId};
use zirv_flow_store::PgPool;

use crate::AppState;
use crate::admin::{self, AdminError, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct PrivacySettingsBody {
    /// What happens to a person left with no source record, event or explicit consent after a
    /// source retracts a record: `keep` (the default) or `delete`.
    #[schema(example = "keep")]
    orphan_policy: String,
}

/// Sets the orphan policy and audits it as `workspace.privacy_settings_updated`.
pub(crate) async fn set_orphan_policy(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    policy: &str,
) -> Result<OrphanPolicy, AdminError> {
    let policy: OrphanPolicy = policy.parse()?;
    let mut tx = zirv_flow_store::begin(pool).await?;
    if !zirv_flow_store::set_orphan_policy(&mut *tx, workspace, policy).await? {
        return Err(AdminError::NotFound("workspace"));
    }
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "workspace.privacy_settings_updated",
        Some(("workspace", workspace.as_uuid())),
        json!({ "orphan_policy": policy.as_str() }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(policy)
}

/// The workspace's privacy settings.
#[utoipa::path(
    get,
    operation_id = "get_privacy_settings",
    path = "/api/v1/workspaces/{ws}/privacy/settings",
    tag = "privacy",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The settings", body = PrivacySettingsBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<PrivacySettingsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let policy = zirv_flow_store::orphan_policy(&state.pool, workspace.id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(PrivacySettingsBody {
        orphan_policy: policy.as_str().to_owned(),
    }))
}

/// Sets the orphan policy. Needs the workspace admin role and the CSRF token. Audited as
/// `workspace.privacy_settings_updated`.
#[utoipa::path(
    put,
    operation_id = "put_privacy_settings",
    path = "/api/v1/workspaces/{ws}/privacy/settings",
    tag = "privacy",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = PrivacySettingsBody,
    responses(
        (status = 200, description = "The saved settings", body = PrivacySettingsBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The policy is not keep or delete", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn put(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<PrivacySettingsBody>,
) -> Result<Json<PrivacySettingsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let policy = set_orphan_policy(
        &state.pool,
        &principal.context(),
        workspace.id,
        &body.orphan_policy,
    )
    .await?;
    Ok(Json(PrivacySettingsBody {
        orphan_policy: policy.as_str().to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_needs_an_orphan_policy() {
        let body: PrivacySettingsBody =
            serde_json::from_str(r#"{"orphan_policy":"delete"}"#).unwrap();

        assert_eq!(body.orphan_policy, "delete");
        assert!(serde_json::from_str::<PrivacySettingsBody>("{}").is_err());
    }
}
