//! Listing, reading and deleting the images of a workspace.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde_json::json;
use zirv_flow_store::{
    AssetOwner, DeleteAssetError, asset_owners, begin, commit, delete_asset, find_asset,
    list_assets, workspace_asset_bytes,
};

use super::{AssetBody, AssetDetailBody, AssetsBody, parse_page};
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};

/// The refusal to delete an asset that published versions use, naming them.
fn in_use(owners: &[AssetOwner]) -> ApiError {
    let users: Vec<String> = owners
        .iter()
        .map(|owner| format!("{} {} v{}", owner.owner_kind, owner.owner_id, owner.version))
        .collect();
    ApiError::new(
        StatusCode::CONFLICT,
        format!("used by {}", users.join(", ")),
    )
    .with_code("asset_in_use")
}

/// The workspace's images, newest first, with the bytes they take and the limit for them.
#[utoipa::path(
    get,
    operation_id = "list_assets",
    path = "/api/v1/workspaces/{ws}/assets",
    tag = "assets",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("limit" = Option<u32>, Query, description = "Assets per page, 1 to 200; default 50"),
        ("cursor" = Option<Uuid>, Query, description = "The next_cursor of the previous page")
    ),
    responses(
        (status = 200, description = "A page of assets and the quota", body = AssetsBody),
        (status = 400, description = "limit is out of range or cursor is malformed; the removed before and after answer paging_parameter_renamed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<AssetsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let (limit, before) = parse_page(&query.unwrap_or_default())?;
    let (listed, more) = list_assets(&state.pool, workspace.id, before, limit).await?;
    let used = workspace_asset_bytes(&state.pool, workspace.id).await?;
    let next_cursor = match listed.last() {
        Some(last) if more => Some(last.asset.id),
        _ => None,
    };
    let public_url = state.public_url();
    Ok(Json(AssetsBody::new(
        listed
            .iter()
            .map(|item| AssetBody::new(&item.asset, item.references, public_url))
            .collect(),
        next_cursor,
        used,
        state.assets,
    )))
}

/// One image, with the published versions that use it.
#[utoipa::path(
    get,
    operation_id = "get_asset",
    path = "/api/v1/workspaces/{ws}/assets/{id}",
    tag = "assets",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Asset id")
    ),
    responses(
        (status = 200, description = "The asset and its users", body = AssetDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or asset, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<AssetDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id = path_id(&id)?;
    let asset = find_asset(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let owners = asset_owners(&state.pool, id).await?;
    Ok(Json(AssetDetailBody::new(
        &asset,
        &owners,
        state.public_url(),
    )))
}

/// Deletes an image: it is marked deleted and its bytes are dropped. Refused with 409
/// (`asset_in_use`, the users in the detail) while a published template, theme or snippet
/// version uses it. Workspace admins only. Audited as `asset.deleted`.
#[utoipa::path(
    delete,
    operation_id = "delete_asset",
    path = "/api/v1/workspaces/{ws}/assets/{id}",
    tag = "assets",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Asset id")
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or asset, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "asset_in_use: published versions use the asset", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let id = path_id(&id)?;
    let mut tx = begin(&state.pool).await?;
    let asset = delete_asset(&mut tx, workspace.id, id)
        .await
        .map_err(|error| match error {
            DeleteAssetError::NotFound => ApiError::not_found(),
            DeleteAssetError::InUse(owners) => in_use(&owners),
            DeleteAssetError::Store(error) => error.into(),
        })?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "asset.deleted",
        Some(("asset", asset.id)),
        json!({ "id": asset.id, }),
    )
    .await?;
    commit(tx).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[test]
    fn an_asset_in_use_is_a_409_naming_its_users() {
        let id = Uuid::now_v7();
        let error = in_use(&[AssetOwner {
            owner_kind: "theme".into(),
            owner_id: id,
            version: 2,
            name: Some("Brand".into()),
            archived: false,
        }]);

        assert_eq!(error.status(), StatusCode::CONFLICT);
        assert_eq!(error.code(), Some("asset_in_use"));
        assert_eq!(error.detail(), format!("used by theme {id} v2"));
    }
}
