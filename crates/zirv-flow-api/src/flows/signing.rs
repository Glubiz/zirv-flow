//! The workspace's webhook signing secret (architecture 9.1 and 16.2, rulings 9cb0bea9, a5c8d754
//! and 43163028): the secret every `action.webhook` call is signed with. A workspace admin
//! creates it and rotates it; it is generated here, sealed in `encrypted_secrets`, and returned
//! once, by create and by rotate, never by a read, an audit entry or a log line.
//!
//! A rotation keeps the previous secret signing for [`SIGNING_GRACE_SECONDS`]: every call in the
//! window carries a signature made with each secret, so a receiver can move to the new one at
//! any time inside it. A second rotation inside the window ends the first secret at once.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use zirv_flow_core::flow::SIGNING_GRACE_SECONDS;
use zirv_flow_core::signature::generate_secret;
use zirv_flow_core::{SecretPurpose, WorkspaceId};

use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};
use crate::webhooks::ops::plaintext;

/// Whether the workspace has a signing secret, without the secret.
#[derive(Serialize, ToSchema)]
pub(crate) struct SigningSecretBody {
    /// Whether a signing secret exists. A flow with a webhook node cannot be published without
    /// one.
    configured: bool,
    /// When the current secret was made.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    created_at: Option<OffsetDateTime>,
    /// When the previous secret stops signing; null when no rotation left one inside its window.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    previous_valid_until: Option<OffsetDateTime>,
    /// How long a rotation keeps the previous secret signing, in seconds.
    grace_seconds: i64,
}

/// A signing secret, shown once.
#[derive(Serialize, ToSchema)]
pub(crate) struct IssuedSigningSecretBody {
    /// The signing secret. Shown only in this response; keep it.
    secret: String,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    /// When the previous secret stops signing; null for a new secret that replaced nothing.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    previous_valid_until: Option<OffsetDateTime>,
    /// How long a rotation keeps the previous secret signing, in seconds.
    grace_seconds: i64,
}

fn keyring(state: &AppState) -> Result<&std::sync::Arc<zirv_flow_core::Keyring>, ApiError> {
    state.keyring().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "the signing secret is unavailable until the master key is configured",
        )
    })
}

/// A response that holds a secret must not be cached.
fn no_store<T: IntoResponse>(status: StatusCode, body: T) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], body).into_response()
}

fn conflict(detail: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, detail)
}

/// Seals a new secret and makes it current, or rotates to it. Returns the plaintext once.
async fn issue(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    rotate: bool,
) -> Result<IssuedSigningSecretBody, ApiError> {
    let keyring = keyring(state)?;
    let secret = generate_secret();
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let now = state.clock.now(&mut tx).await?;
    let id = admin::create_secret_in(
        &mut tx,
        context,
        keyring,
        workspace,
        SecretPurpose::WebhookSigning,
        &plaintext(&secret),
    )
    .await?;
    let previous_valid_until = if rotate {
        // A workspace with nothing to rotate writes nothing: dropping the transaction undoes
        // the sealed secret.
        let rotation = zirv_flow_store::rotate_signing_key(&mut tx, workspace, id, now)
            .await?
            .ok_or_else(|| conflict("there is no signing secret to rotate; create one first"))?;
        admin::audit(
            &mut tx,
            context,
            Some(workspace),
            "webhook_signing.rotated",
            Some(("secret", id.as_uuid())),
            json!({
                "retired_secret_id": rotation.retired.secret_id,
                "ended_earlier_secrets": rotation.dropped,
            }),
        )
        .await?;
        rotation.retired.valid_until()
    } else {
        if !zirv_flow_store::create_signing_key(&mut tx, workspace, id, now).await? {
            return Err(conflict(
                "the workspace already has a signing secret; rotate it instead",
            ));
        }
        admin::audit(
            &mut tx,
            context,
            Some(workspace),
            "webhook_signing.created",
            Some(("secret", id.as_uuid())),
            json!({}),
        )
        .await?;
        None
    };
    zirv_flow_store::commit(tx).await?;
    Ok(IssuedSigningSecretBody {
        secret: secret.expose().to_owned(),
        created_at: now,
        previous_valid_until,
        grace_seconds: SIGNING_GRACE_SECONDS,
    })
}

/// Whether the workspace has a webhook signing secret and when a rotation's previous secret
/// stops signing. Never returns a secret. Workspace admins only.
#[utoipa::path(
    get,
    operation_id = "get_webhook_signing_secret",
    path = "/api/v1/workspaces/{ws}/webhook-signing-secret",
    tag = "flows",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The signing secret's state", body = SigningSecretBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a key without the admin scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<SigningSecretBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let mut connection = zirv_flow_store::begin(&state.pool).await?;
    let now = state.clock.now(&mut connection).await?;
    let keys = zirv_flow_store::live_signing_keys(&mut connection, workspace.id, now).await?;
    Ok(Json(SigningSecretBody {
        configured: keys.is_some(),
        created_at: keys.map(|keys| keys.current.created_at),
        previous_valid_until: keys
            .and_then(|keys| keys.previous)
            .and_then(|previous| previous.valid_until()),
        grace_seconds: SIGNING_GRACE_SECONDS,
    }))
}

/// Creates the workspace's webhook signing secret and returns it once. Workspace admins only.
/// Audited as `webhook_signing.created`.
#[utoipa::path(
    post,
    operation_id = "create_webhook_signing_secret",
    path = "/api/v1/workspaces/{ws}/webhook-signing-secret",
    tag = "flows",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 201, description = "The new secret, shown only now", body = IssuedSigningSecretBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The workspace already has a signing secret, or the master key is outdated", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to seal the secret", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let issued = issue(&state, &principal.context(), workspace.id, false).await?;
    Ok(no_store(StatusCode::CREATED, Json(issued)))
}

/// Issues a new signing secret and returns it once. The previous secret keeps signing for the
/// grace window; an earlier retired secret stops at once. Workspace admins only. Audited as
/// `webhook_signing.rotated`.
#[utoipa::path(
    post,
    operation_id = "rotate_webhook_signing_secret",
    path = "/api/v1/workspaces/{ws}/webhook-signing-secret/rotate",
    tag = "flows",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The new secret, shown only now", body = IssuedSigningSecretBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The workspace has no signing secret yet, or the master key is outdated", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to seal the secret", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn rotate(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let issued = issue(&state, &principal.context(), workspace.id, true).await?;
    Ok(no_store(StatusCode::OK, Json(issued)))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use time::macros::datetime;
    use tower::ServiceExt;
    use zirv_flow_core::{
        HmacRoot, Keyring, MasterKey, Role, Scope, Slug, WorkspaceId, WorkspaceName,
        deployment_key_aad, seal,
    };
    use zirv_flow_store::{Clock, ManualClock, secrets};

    use super::*;
    use crate::admin::{self, Context};
    use crate::test_support::tables_hold_no;
    use crate::{AppState, router};

    const PASSWORD: &str = "correct horse battery";
    const START: OffsetDateTime = datetime!(2031-05-06 07:08:09 UTC);

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Value,
        text: String,
    }

    enum Auth<'a> {
        Anonymous,
        Key(&'a str),
        Session(&'a str, &'a str),
    }

    struct Fixture {
        pool: PgPool,
        router: Router,
        clock: ManualClock,
        ws: WorkspaceId,
        admin: String,
        read: String,
        other_admin: String,
        /// A workspace member's session cookie and CSRF token.
        member: (String, String),
    }

    async fn send(router: &Router, method: &str, uri: &str, auth: Auth<'_>) -> Reply {
        let mut request = Request::builder().method(method).uri(uri);
        request = match auth {
            Auth::Anonymous => request,
            Auth::Key(key) => request.header(header::AUTHORIZATION, format!("Bearer {key}")),
            Auth::Session(cookie, csrf) => request
                .header(header::COOKIE, cookie)
                .header("x-csrf-token", csrf),
        };
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            text: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn fixture(pool: &PgPool, with_keyring: bool) -> Fixture {
        zirv_flow_store::migrate(pool).await.unwrap();
        let mut workspaces = Vec::new();
        for slug in ["ours", "theirs"] {
            let name = WorkspaceName::parse(slug).unwrap();
            let slug = Slug::parse(slug).unwrap();
            workspaces.push(
                zirv_flow_store::create_workspace(pool, &name, &slug)
                    .await
                    .unwrap()
                    .id,
            );
        }
        let (ws, other_ws) = (workspaces[0], workspaces[1]);
        let clock = ManualClock::new(START);
        let mut state = AppState::new(pool.clone(), Arc::new(String::new))
            .with_clock(Clock::Manual(clock.clone()));
        if with_keyring {
            let keyring = Arc::new(Keyring::new(MasterKey::generate(), None).unwrap());
            let root = HmacRoot::bootstrap_from(keyring.current()).to_secret_bytes();
            let sealed = seal(
                &keyring,
                root.expose(),
                &deployment_key_aad(secrets::HMAC_ROOT),
            );
            secrets::insert_hmac_root_if_absent(pool, &sealed)
                .await
                .unwrap();
            state = state.with_keyring(keyring);
        }
        let router = router(state);
        let key = |workspace: WorkspaceId, scope: Scope| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), workspace, "signing", &[scope])
                    .await
                    .unwrap()
                    .plaintext
            }
        };
        let admin_key = key(ws, Scope::Admin).await;
        let read = key(ws, Scope::Read).await;
        let other_admin = key(other_ws, Scope::Admin).await;
        let user = admin::create_user(pool, &Context::system(), "member@example.com", PASSWORD)
            .await
            .unwrap();
        admin::set_member(pool, &Context::system(), ws, user.id, Role::Editor)
            .await
            .unwrap();
        let login = {
            let request = Request::builder()
                .method("POST")
                .uri("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"email": "member@example.com", "password": PASSWORD}).to_string(),
                ))
                .unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            let cookie = response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            (cookie, body["csrf_token"].as_str().unwrap().to_owned())
        };
        Fixture {
            pool: pool.clone(),
            router,
            clock,
            ws,
            admin: admin_key,
            read,
            other_admin,
            member: login,
        }
    }

    impl Fixture {
        fn path(&self, tail: &str) -> String {
            format!(
                "/api/v1/workspaces/{}/webhook-signing-secret{tail}",
                self.ws.as_uuid()
            )
        }

        async fn call(&self, method: &str, tail: &str, auth: Auth<'_>) -> Reply {
            send(&self.router, method, &self.path(tail), auth).await
        }

        async fn as_admin(&self, method: &str, tail: &str) -> Reply {
            self.call(method, tail, Auth::Key(&self.admin)).await
        }

        async fn secret_rows(&self) -> i64 {
            sqlx::query_scalar(
                "SELECT count(*) FROM encrypted_secrets WHERE purpose = 'webhook_signing'",
            )
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_a_workspace_admin_may_read_create_or_rotate_the_secret(pool: PgPool) {
        let fx = fixture(&pool, true).await;

        for (method, tail) in [("GET", ""), ("POST", ""), ("POST", "/rotate")] {
            let anonymous = fx.call(method, tail, Auth::Anonymous).await;
            let read = fx.call(method, tail, Auth::Key(&fx.read)).await;
            let plain_member = fx
                .call(method, tail, Auth::Session(&fx.member.0, &fx.member.1))
                .await;
            let foreign = fx.call(method, tail, Auth::Key(&fx.other_admin)).await;

            assert_eq!(
                anonymous.status,
                StatusCode::UNAUTHORIZED,
                "{method} {tail}"
            );
            assert_eq!(read.status, StatusCode::FORBIDDEN, "{method} {tail}");
            assert_eq!(
                plain_member.status,
                StatusCode::FORBIDDEN,
                "{method} {tail}"
            );
            assert_eq!(foreign.status, StatusCode::NOT_FOUND, "{method} {tail}");
        }
        assert_eq!(fx.secret_rows().await, 0, "no refusal wrote a secret");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_server_without_a_master_key_refuses_and_writes_nothing(pool: PgPool) {
        let fx = fixture(&pool, false).await;

        let created = fx.as_admin("POST", "").await;

        assert_eq!(created.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(fx.secret_rows().await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_secret_is_generated_shown_once_sealed_and_never_read_back(pool: PgPool) {
        let fx = fixture(&pool, true).await;
        let before = fx.as_admin("GET", "").await;

        let created = fx.as_admin("POST", "").await;
        let again = fx.as_admin("POST", "").await;
        let after = fx.as_admin("GET", "").await;

        assert_eq!(before.status, StatusCode::OK);
        assert_eq!(
            before.body,
            json!({"configured": false, "created_at": null, "previous_valid_until": null,
                   "grace_seconds": 86_400})
        );
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        assert_eq!(created.headers[header::CACHE_CONTROL], "no-store");
        let secret = created.body["secret"].as_str().unwrap().to_owned();
        assert!(secret.starts_with("zfwh_") && secret.len() == 69);
        assert_eq!(created.body["grace_seconds"], 86_400);
        assert_eq!(again.status, StatusCode::CONFLICT);
        assert!(!again.text.contains(&secret));
        assert_eq!(after.body["configured"], true);
        assert_eq!(after.body["previous_valid_until"], Value::Null);
        assert!(!after.text.contains(&secret));
        assert_eq!(
            fx.secret_rows().await,
            1,
            "the refused create stored nothing"
        );
        let audited: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_log WHERE action = 'webhook_signing.created'",
        )
        .fetch_one(&fx.pool)
        .await
        .unwrap();
        assert_eq!(audited, 1);
        tables_hold_no(&fx.pool, &[&secret]).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rotation_needs_a_secret_and_keeps_the_previous_one_for_the_grace_window(
        pool: PgPool,
    ) {
        let fx = fixture(&pool, true).await;
        let early = fx.as_admin("POST", "/rotate").await;
        let created = fx.as_admin("POST", "").await;
        let first = created.body["secret"].as_str().unwrap().to_owned();
        fx.clock.advance(time::Duration::hours(1));

        let rotated = fx.as_admin("POST", "/rotate").await;
        let status = fx.as_admin("GET", "").await;

        assert_eq!(early.status, StatusCode::CONFLICT);
        assert_eq!(rotated.status, StatusCode::OK, "{:?}", rotated.body);
        assert_eq!(rotated.headers[header::CACHE_CONTROL], "no-store");
        let second = rotated.body["secret"].as_str().unwrap().to_owned();
        assert_ne!(first, second);
        let until = START + time::Duration::hours(1) + time::Duration::seconds(86_400);
        assert_eq!(
            rotated.body["previous_valid_until"],
            until
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        );
        assert_eq!(
            status.body["previous_valid_until"],
            rotated.body["previous_valid_until"]
        );
        assert_eq!(fx.secret_rows().await, 2);

        // Inside the window the previous secret is still listed; at its end it is not.
        fx.clock.advance(time::Duration::seconds(86_400));
        let ended = fx.as_admin("GET", "").await;
        assert_eq!(ended.body["configured"], true);
        assert_eq!(ended.body["previous_valid_until"], Value::Null);

        let audited: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_log WHERE action = 'webhook_signing.rotated'",
        )
        .fetch_one(&fx.pool)
        .await
        .unwrap();
        assert_eq!(audited, 1);
        tables_hold_no(&fx.pool, &[&first, &second]).await;
    }
}
