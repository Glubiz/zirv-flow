//! Workspace API keys.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{ApiKeyId, Scope};
use zirv_flow_store::ApiKey;

use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct ApiKeyBody {
    id: Uuid,
    name: String,
    /// Any of `ingest`, `read`, `write`, `admin`.
    #[schema(value_type = Vec<String>, example = json!(["read"]))]
    scopes: Vec<Scope>,
    created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    last_used_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    revoked_at: Option<OffsetDateTime>,
}

impl From<&ApiKey> for ApiKeyBody {
    fn from(key: &ApiKey) -> Self {
        Self {
            id: key.id.as_uuid(),
            name: key.name.clone(),
            scopes: key.scopes.clone(),
            created_by: key.created_by.map(|user| user.as_uuid()),
            created_at: key.created_at,
            last_used_at: key.last_used_at,
            revoked_at: key.revoked_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CreatedApiKeyBody {
    #[serde(flatten)]
    metadata: ApiKeyBody,
    /// The full key (`zfk_...`) to send as `Authorization: Bearer`. Shown once; only a hash
    /// of its secret is stored.
    key: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateApiKeyRequest {
    /// 1 to 100 characters.
    #[schema(example = "ingest worker")]
    name: String,
    /// At least one of `ingest`, `read`, `write`, `admin`.
    #[schema(value_type = Vec<String>, example = json!(["read"]))]
    scopes: Vec<String>,
}

/// The keys of a workspace, never including secrets. Workspace admins, the owner and
/// `admin` keys only.
#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/api-keys",
    tag = "api-keys",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The keys", body = Vec<ApiKeyBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a key without the admin scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<Vec<ApiKeyBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let keys = zirv_flow_store::list_api_keys(&state.pool, workspace.id).await?;
    Ok(Json(keys.iter().map(ApiKeyBody::from).collect()))
}

/// Creates a key and returns its secret, once.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{ws}/api-keys",
    tag = "api-keys",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateApiKeyRequest,
    responses(
        (status = 201, description = "The new key, with its secret", body = CreatedApiKeyBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid name or scopes", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateApiKeyRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let scopes = body
        .scopes
        .iter()
        .map(|scope| scope.parse::<Scope>())
        .collect::<Result<Vec<_>, _>>()?;
    let created = admin::create_api_key(
        &state.pool,
        &principal.context(),
        workspace.id,
        &body.name,
        &scopes,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(CreatedApiKeyBody {
            metadata: ApiKeyBody::from(&created.key),
            key: created.plaintext,
        }),
    ))
}

/// Revokes a key. Revoked keys are refused with 401 from then on.
#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{ws}/api-keys/{id}",
    tag = "api-keys",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "API key id")
    ),
    responses(
        (status = 204, description = "Revoked"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or live key, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn revoke(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let id: ApiKeyId = path_id(&id)?;
    admin::revoke_api_key(&state.pool, &principal.context(), workspace.id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::WorkspaceId;

    use super::*;

    fn key() -> ApiKey {
        ApiKey {
            id: ApiKeyId::new(),
            workspace_id: WorkspaceId::new(),
            name: "ci".into(),
            scopes: vec![Scope::Read, Scope::Ingest],
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            last_used_at: None,
            revoked_at: None,
        }
    }

    #[test]
    fn a_listed_key_shows_scopes_and_no_secret() {
        let json = serde_json::to_value(ApiKeyBody::from(&key())).unwrap();

        assert_eq!(json["scopes"], serde_json::json!(["read", "ingest"]));
        assert!(json["last_used_at"].is_null());
        assert!(json.get("key").is_none());
        assert!(json.get("secret").is_none());
        assert!(json.get("secret_hash").is_none());
    }

    #[test]
    fn a_created_key_carries_the_plaintext_beside_the_metadata() {
        let created = CreatedApiKeyBody {
            metadata: ApiKeyBody::from(&key()),
            key: "zfk_x".into(),
        };

        let json = serde_json::to_value(created).unwrap();

        assert_eq!(json["key"], "zfk_x");
        assert_eq!(json["name"], "ci");
    }

    #[test]
    fn an_unknown_scope_in_a_request_is_a_validation_error_naming_the_field() {
        let request: CreateApiKeyRequest =
            serde_json::from_str(r#"{"name":"x","scopes":["root"]}"#).unwrap();

        let error = request.scopes[0].parse::<Scope>().unwrap_err();

        assert_eq!(error.field(), "scopes");
    }
}
