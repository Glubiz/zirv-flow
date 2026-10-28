//! Workspace API keys.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{ApiKeyId, Scope};
use zirv_flow_store::ApiKey;

use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::idempotency::{self, ConflictProblem, Entry, Keyed, RequestHash, lease};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

/// What a key may do. The schema twin of `zirv_flow_core::Scope`, so the generated client knows
/// the closed set.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ApiKeyScope {
    Ingest,
    Read,
    Write,
    Publish,
    Admin,
    Pii,
}

impl From<Scope> for ApiKeyScope {
    fn from(scope: Scope) -> Self {
        match scope {
            Scope::Ingest => Self::Ingest,
            Scope::Read => Self::Read,
            Scope::Write => Self::Write,
            Scope::Publish => Self::Publish,
            Scope::Admin => Self::Admin,
            Scope::Pii => Self::Pii,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ApiKeyBody {
    id: Uuid,
    name: String,
    #[schema(example = json!(["read"]))]
    scopes: Vec<ApiKeyScope>,
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
            scopes: key.scopes.iter().copied().map(ApiKeyScope::from).collect(),
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

#[derive(Deserialize, Serialize, ToSchema)]
pub(crate) struct CreateApiKeyRequest {
    /// 1 to 100 characters.
    #[schema(example = "ingest worker")]
    name: String,
    /// At least one of `ingest`, `read`, `write`, `publish`, `admin`, `pii`.
    #[schema(value_type = Vec<String>, example = json!(["read"]))]
    scopes: Vec<String>,
}

/// The keys of a workspace, never including secrets. Workspace admins, the owner and
/// `admin` keys only.
#[utoipa::path(
    get,
    operation_id = "list_api_keys",
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
///
/// Send an `Idempotency-Key` header (1 to 255 characters) to make a retry safe for 24 hours:
/// while the first request runs, the same key
/// answers `409 idempotency_key_in_flight` with `Retry-After`; once it has created the key, the
/// same key with the same body answers `409 idempotency_key_replayed` with the id of the key it
/// created and never shows the secret again; the same key with another body is
/// `422 idempotency_key_reused`. The key belongs to the caller that sent it.
#[utoipa::path(
    post,
    operation_id = "create_api_key",
    path = "/api/v1/workspaces/{ws}/api-keys",
    tag = "api-keys",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 255 characters; makes a retry safe for 24 hours. A key first used for another request answers 422 `idempotency_key_reused`")
    ),
    request_body = CreateApiKeyRequest,
    responses(
        (status = 201, description = "The new key, with its secret", body = CreatedApiKeyBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The Idempotency-Key is in use: `idempotency_key_in_flight` while the first request runs (see Retry-After), or `idempotency_key_replayed` once it created a key, with resource_id as its id and no secret", body = ConflictProblem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid name or scopes", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ApiJson(body): ApiJson<CreateApiKeyRequest>,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let scopes = body
        .scopes
        .iter()
        .map(|scope| scope.parse::<Scope>())
        .collect::<Result<Vec<_>, _>>()?;
    let request_hash = RequestHash::of_json(&method, &uri, &body)?;
    let keyed = Keyed::of(&headers, "create_api_key", &principal)?;
    let reservation =
        match idempotency::reserve(&state.pool, workspace.id, keyed, request_hash, lease::QUICK)
            .await?
        {
            Entry::Go(reservation) => reservation,
            Entry::Answer(response) => return Ok(response),
        };
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let created = admin::create_api_key_in(
        &mut tx,
        &principal.context(),
        workspace.id,
        &body.name,
        &scopes,
    )
    .await?;
    idempotency::complete(
        reservation.as_ref(),
        &mut tx,
        StatusCode::CREATED,
        created.key.id.as_uuid(),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    idempotency::finish(reservation);
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(CreatedApiKeyBody {
            metadata: ApiKeyBody::from(&created.key),
            key: created.plaintext,
        }),
    )
        .into_response())
}

/// Revokes a key. Revoked keys are refused with 401 from then on.
#[utoipa::path(
    delete,
    operation_id = "revoke_api_key",
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
    fn every_scope_in_the_schema_is_a_scope_the_server_parses() {
        for scope in [
            Scope::Ingest,
            Scope::Read,
            Scope::Write,
            Scope::Publish,
            Scope::Admin,
            Scope::Pii,
        ] {
            let name = serde_json::to_value(ApiKeyScope::from(scope)).unwrap();

            assert_eq!(name, scope.as_str());
        }
    }

    #[test]
    fn an_unknown_scope_in_a_request_is_a_validation_error_naming_the_field() {
        let request: CreateApiKeyRequest =
            serde_json::from_str(r#"{"name":"x","scopes":["root"]}"#).unwrap();

        let error = request.scopes[0].parse::<Scope>().unwrap_err();

        assert_eq!(error.field(), "scopes");
    }
}
