//! Workspace invitations: an admin invites an address, and the invitee accepts through a one-time
//! link.
//!
//! The server sends no mail. The create response carries the link once; only the SHA-256 of its
//! token is stored. The two routes that take the token are public and share one rate limiter.
//! Every dead token (unknown, revoked, expired or used) answers the same 404. Neither the address
//! nor the token reaches an audit row, a metric label or a log line.

use std::time::Instant;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::Role;
use zirv_flow_core::token::sha256;
use zirv_flow_store::Invitation;

use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::ingest::RateLimiter;
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::{request_id, session};

/// Lookups and accepts allowed in a burst, and the steady rate per second after it. No client
/// address is available, so one budget covers the whole process.
const BURST: u32 = 10;
const PER_SECOND: u32 = 1;

/// The limiter of the two token routes.
pub(crate) fn limiter() -> RateLimiter {
    RateLimiter::new(PER_SECOND, BURST, Instant::now())
}

fn take_budget(state: &AppState) -> Result<(), ApiError> {
    state
        .invitation_limiter
        .take(Uuid::nil(), 1, Instant::now())
        .map_err(|wait| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too many invitation requests; try again shortly",
            )
            .with_code("rate_limited")
            .with_retry_after(wait)
        })
}

/// `c***@acme.com` for `cy@acme.com`: enough for the invitee to recognise their address, never the
/// address itself.
fn email_hint(email: &str) -> String {
    let (local, domain) = email.split_once('@').unwrap_or((email, ""));
    let first: String = local.chars().take(1).collect();
    format!("{first}***@{domain}")
}

#[derive(Serialize, ToSchema)]
pub(crate) struct InvitationBody {
    id: Uuid,
    email: String,
    /// `admin`, `publisher`, `editor` or `viewer`.
    #[schema(value_type = String, example = "viewer")]
    role: Role,
    /// Whether the member sees personal values unmasked. Always false for an admin, who sees them
    /// anyway.
    view_pii: bool,
    /// The user who made the invitation, when they still exist.
    invited_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    expires_at: OffsetDateTime,
}

impl From<&Invitation> for InvitationBody {
    fn from(invitation: &Invitation) -> Self {
        Self {
            id: invitation.id,
            email: invitation.email.clone(),
            role: invitation.role,
            view_pii: invitation.view_pii,
            invited_by: invitation.invited_by.map(|user| user.as_uuid()),
            created_at: invitation.created_at,
            expires_at: invitation.expires_at,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateInvitationRequest {
    #[schema(example = "cy@acme.com")]
    email: String,
    /// `admin`, `publisher`, `editor` or `viewer`.
    role: String,
    /// Grants the personal-values grant to a publisher, editor or viewer; ignored for an admin.
    #[serde(default)]
    view_pii: Option<bool>,
    /// 1, 7 or 30; 7 when left out.
    #[serde(default)]
    expires_in_days: Option<i32>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CreatedInvitationBody {
    id: Uuid,
    email: String,
    #[schema(value_type = String, example = "viewer")]
    role: Role,
    view_pii: bool,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    expires_at: OffsetDateTime,
    /// The link to send to the invitee. It is shown here once and never again.
    link: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct InvitationLookupBody {
    workspace_name: String,
    #[schema(value_type = String, example = "viewer")]
    role: Role,
    /// The invited address with all but its first letter hidden, such as `c***@acme.com`.
    email_hint: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct AcceptInvitationRequest {
    /// Only for someone with no session: creates the account for the invited address with this
    /// password (12 to 1024 characters), signs it in and accepts. Ignored when a session is sent.
    #[serde(default)]
    password: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AcceptedInvitationBody {
    workspace_id: Uuid,
    #[schema(value_type = String, example = "viewer")]
    role: Role,
}

/// The pending invitations of a workspace. Workspace admins and the owner only.
#[utoipa::path(
    get,
    operation_id = "list_invitations",
    path = "/api/v1/workspaces/{ws}/invitations",
    tag = "invitations",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The pending invitations, newest first", body = Vec<InvitationBody>),
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
) -> Result<Json<Vec<InvitationBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let mut connection = zirv_flow_store::begin(&state.pool).await?;
    let pending = zirv_flow_store::list_pending_invitations(&mut connection, workspace.id).await?;
    Ok(Json(pending.iter().map(InvitationBody::from).collect()))
}

/// Invites an address and returns the link, once. Workspace admins and the owner only.
///
/// The server sends no mail: copy the `link` to the invitee. A workspace admin may invite up to
/// the admin role. The invitation is valid for `expires_in_days` (1, 7 or 30; default 7) and for
/// one use. An address with a live invitation is `409 invitation_pending`; one that already
/// belongs to a member is `409 already_member`.
#[utoipa::path(
    post,
    operation_id = "create_invitation",
    path = "/api/v1/workspaces/{ws}/invitations",
    tag = "invitations",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateInvitationRequest,
    responses(
        (status = 201, description = "The invitation, with its link", body = CreatedInvitationBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`invitation_pending` or `already_member`", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid email, role or expiry", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateInvitationRequest>,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let role: Role = body.role.parse()?;
    let created = admin::create_invitation(
        &state.pool,
        &principal.context(),
        workspace.id,
        &body.email,
        role,
        body.view_pii.unwrap_or(false),
        body.expires_in_days,
    )
    .await?;
    let invitation = &created.invitation;
    let response = CreatedInvitationBody {
        id: invitation.id,
        email: invitation.email.clone(),
        role: invitation.role,
        view_pii: invitation.view_pii,
        expires_at: invitation.expires_at,
        link: format!(
            "{}/invite/{}",
            state.public_url().unwrap_or_default(),
            created.token
        ),
    };
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(response),
    )
        .into_response())
}

/// Revokes a pending invitation. Its link stops working at once; the address may be invited again.
#[utoipa::path(
    delete,
    operation_id = "revoke_invitation",
    path = "/api/v1/workspaces/{ws}/invitations/{id}",
    tag = "invitations",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Invitation id")
    ),
    responses(
        (status = 204, description = "Revoked"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or pending invitation, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn revoke(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let id: Uuid = path_id(&id)?;
    admin::revoke_invitation(&state.pool, &principal.context(), workspace.id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// What a link is for, so its page can name the workspace. Public: the token is the authority.
///
/// A token that is unknown, revoked, expired or used answers one identical 404. Past a burst of 10
/// requests the two token routes share, they answer `429 rate_limited`.
#[utoipa::path(
    get,
    operation_id = "get_invitation",
    path = "/api/v1/invitations/{token}",
    tag = "invitations",
    params(("token" = String, Path, description = "The token of the invitation link")),
    responses(
        (status = 200, description = "The invitation", body = InvitationLookupBody),
        (status = 404, description = "The link is not valid", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many requests", body = Problem, content_type = "application/problem+json")
    ),
    security(())
)]
pub(crate) async fn lookup(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Response, ApiError> {
    take_budget(&state)?;
    let mut connection = zirv_flow_store::begin(&state.pool).await?;
    let invitation = zirv_flow_store::find_live_invitation(&mut connection, &sha256(&token))
        .await?
        .ok_or_else(ApiError::not_found)?;
    let workspace = zirv_flow_store::find_workspace(&mut *connection, invitation.workspace_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(InvitationLookupBody {
            workspace_name: workspace.name,
            role: invitation.role,
            email_hint: email_hint(&invitation.email),
        }),
    )
        .into_response())
}

/// Accepts an invitation and makes the signed-in user a member with the invited role.
///
/// The session's address must equal the invited one, ignoring case; otherwise
/// `403 invitation_address_mismatch` and the invitation stays usable. With no session and a
/// `password`, an address that has no account gets one, is signed in (the `zf_session` cookie is
/// set) and accepts, all in one transaction; that needs local sign-in (`403
/// local_sign_in_disabled` otherwise) and fails with `409 account_exists` for an address that
/// already has an account. A dead link is the same 404 as on lookup.
#[utoipa::path(
    post,
    operation_id = "accept_invitation",
    path = "/api/v1/invitations/{token}/accept",
    tag = "invitations",
    params(("token" = String, Path, description = "The token of the invitation link")),
    request_body = AcceptInvitationRequest,
    responses(
        (status = 200, description = "Accepted", body = AcceptedInvitationBody),
        (status = 401, description = "Not signed in", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "An API key, a missing CSRF token, `invitation_address_mismatch` or `local_sign_in_disabled`", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "The link is not valid", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`already_member` or `account_exists`", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The password is too short or too long", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many requests", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ())
)]
pub(crate) async fn accept(
    State(state): State<AppState>,
    principal: Result<Principal, ApiError>,
    Path(token): Path<String>,
    ApiJson(body): ApiJson<AcceptInvitationRequest>,
) -> Result<Response, ApiError> {
    let principal = match principal {
        Ok(principal) => Some(principal),
        // Nobody signed in may still create the account the link is for.
        Err(_) if body.password.is_some() => None,
        Err(error) => return Err(error),
    };
    let user = match &principal {
        Some(principal) => Some(&principal.require_user()?.user),
        None => None,
    };
    take_budget(&state)?;
    let context = match &principal {
        Some(principal) => principal.context(),
        None => admin::Context::anonymous().with_request_id(request_id::current()),
    };
    let accepted = admin::accept_invitation(
        &state.pool,
        &context,
        user,
        &token,
        body.password.as_deref(),
        state.local_sign_in,
    )
    .await?;
    let response = Json(AcceptedInvitationBody {
        workspace_id: accepted.workspace.as_uuid(),
        role: accepted.role,
    });
    let headers = [(header::CACHE_CONTROL, "no-store")];
    match accepted.created {
        Some(user) => {
            let new = session::start(&state, user.id, "invitation").await?;
            Ok((
                headers,
                [(
                    header::SET_COOKIE,
                    crate::cookies::session(&new.token, state.cookie_secure),
                )],
                response,
            )
                .into_response())
        }
        None => Ok((headers, response).into_response()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use zirv_flow_core::UserId;

    use super::*;
    use crate::admin::Context;
    use crate::router;

    const PASSWORD: &str = "correct horse battery";
    const OWNER: &str = "owner@example.com";
    const INVITEE: &str = "cy@acme.test";

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Value,
    }

    impl Reply {
        /// The body without the trace id, which differs on every request.
        fn problem(&self) -> Value {
            let mut body = self.body.clone();
            if let Some(object) = body.as_object_mut() {
                object.remove("trace_id");
            }
            body
        }
    }

    #[derive(Clone)]
    struct Login {
        cookie: String,
        csrf: String,
    }

    enum Auth<'a> {
        Anonymous,
        Session(&'a Login),
        Key(&'a str),
    }

    async fn app_with(pool: &PgPool, local: bool) -> Router {
        zirv_flow_store::migrate(pool).await.unwrap();
        router(
            AppState::new(pool.clone(), Arc::new(String::new))
                .with_public_url(Some("https://flow.test/".into()))
                .with_local_sign_in(local),
        )
    }

    async fn app(pool: &PgPool) -> Router {
        app_with(pool, true).await
    }

    async fn call(
        router: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
        auth: Auth<'_>,
    ) -> Reply {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-request-id", "test-request");
        request = match auth {
            Auth::Anonymous => request,
            Auth::Session(login) => request
                .header(header::COOKIE, &login.cookie)
                .header("x-csrf-token", &login.csrf),
            Auth::Key(key) => request.header(header::AUTHORIZATION, format!("Bearer {key}")),
        };
        let body = match body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    fn login_of(reply: &Reply) -> Login {
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        Login {
            cookie: reply.headers[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned(),
            csrf: reply.body["csrf_token"].as_str().unwrap().to_owned(),
        }
    }

    async fn login(router: &Router, email: &str) -> Login {
        let reply = call(
            router,
            "POST",
            "/api/v1/session",
            Some(json!({"email": email, "password": PASSWORD})),
            Auth::Anonymous,
        )
        .await;
        login_of(&reply)
    }

    async fn owner(pool: &PgPool, router: &Router) -> Login {
        crate::admin::create_owner(pool, &Context::system(), OWNER, PASSWORD)
            .await
            .unwrap();
        login(router, OWNER).await
    }

    async fn user(pool: &PgPool, router: &Router, email: &str) -> (UserId, Login) {
        let user = crate::admin::create_user(pool, &Context::system(), email, PASSWORD)
            .await
            .unwrap();
        (user.id, login(router, email).await)
    }

    async fn workspace(router: &Router, owner: &Login, name: &str) -> String {
        let reply = call(
            router,
            "POST",
            "/api/v1/workspaces",
            Some(json!({"name": name})),
            Auth::Session(owner),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
        reply.body["id"].as_str().unwrap().to_owned()
    }

    async fn add_member(router: &Router, owner: &Login, ws: &str, user: UserId, role: &str) {
        let reply = call(
            router,
            "PUT",
            &format!("/api/v1/workspaces/{ws}/members/{user}"),
            Some(json!({"role": role})),
            Auth::Session(owner),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    }

    async fn invite(router: &Router, who: Auth<'_>, ws: &str, email: &str, role: &str) -> Reply {
        call(
            router,
            "POST",
            &format!("/api/v1/workspaces/{ws}/invitations"),
            Some(json!({"email": email, "role": role})),
            who,
        )
        .await
    }

    /// Invites as the owner and returns `(id, token)`.
    async fn invited(router: &Router, owner: &Login, ws: &str, email: &str) -> (String, String) {
        let reply = invite(router, Auth::Session(owner), ws, email, "viewer").await;
        assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
        (
            reply.body["id"].as_str().unwrap().to_owned(),
            token_of(&reply.body),
        )
    }

    fn token_of(body: &Value) -> String {
        body["link"]
            .as_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap()
            .to_owned()
    }

    async fn accept_as(router: &Router, token: &str, who: Auth<'_>) -> Reply {
        call(
            router,
            "POST",
            &format!("/api/v1/invitations/{token}/accept"),
            Some(json!({})),
            who,
        )
        .await
    }

    async fn look_up(router: &Router, token: &str) -> Reply {
        call(
            router,
            "GET",
            &format!("/api/v1/invitations/{token}"),
            None,
            Auth::Anonymous,
        )
        .await
    }

    async fn role_of(pool: &PgPool, ws: &str, user: UserId) -> Option<String> {
        sqlx::query_scalar("SELECT role FROM memberships WHERE workspace_id = $1 AND user_id = $2")
            .bind(ws.parse::<Uuid>().unwrap())
            .bind(user.as_uuid())
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    async fn audit_actions(pool: &PgPool) -> Vec<String> {
        sqlx::query_scalar("SELECT action FROM audit_log ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[test]
    fn the_hint_keeps_the_first_letter_and_the_domain_only() {
        assert_eq!(email_hint("cy@acme.com"), "c***@acme.com");
        assert_eq!(email_hint("x@y.z"), "x***@y.z");
        assert!(!email_hint("cy@acme.com").contains("cy@"));
    }

    #[test]
    fn the_token_budget_is_a_burst_of_ten_then_one_a_second() {
        let limiter = limiter();
        let now = Instant::now();

        let allowed = (0..12)
            .filter(|_| limiter.take(Uuid::nil(), 1, now).is_ok())
            .count();

        assert_eq!(allowed, 10);
        assert!(limiter.take(Uuid::nil(), 1, now).is_err());
    }

    #[test]
    fn an_accept_request_may_leave_the_password_out() {
        let without: AcceptInvitationRequest = serde_json::from_str("{}").unwrap();
        let with: AcceptInvitationRequest =
            serde_json::from_str(r#"{"password":"a long enough password"}"#).unwrap();

        assert!(without.password.is_none());
        assert!(with.password.is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_link_appears_once_and_only_its_hash_is_stored(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;

        let created = invite(
            &router,
            Auth::Session(&owner),
            &ws,
            "Cy@Acme.test",
            "editor",
        )
        .await;

        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let link = created.body["link"].as_str().unwrap();
        assert!(link.starts_with("https://flow.test/invite/"), "{link}");
        let token = token_of(&created.body);
        assert_eq!(created.body["email"], INVITEE);
        assert_eq!(created.body["role"], "editor");
        assert_eq!(created.headers[header::CACHE_CONTROL], "no-store");
        let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM invitations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, sha256(&token));
        crate::test_support::tables_hold_no(&pool, &[token.as_str()]).await;
        let listed = call(
            &router,
            "GET",
            &format!("/api/v1/workspaces/{ws}/invitations"),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(listed.status, StatusCode::OK);
        assert_eq!(listed.body.as_array().unwrap().len(), 1);
        assert_eq!(listed.body[0]["email"], INVITEE);
        assert!(!listed.body.to_string().contains(&token));
        assert!(listed.body[0].get("link").is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_an_admin_or_an_admin_key_may_invite_list_or_revoke(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (admin_id, admin) = user(&pool, &router, "admin@acme.test").await;
        add_member(&router, &owner, &ws, admin_id, "admin").await;
        let mut others = Vec::new();
        for role in ["publisher", "editor", "viewer"] {
            let (id, login) = user(&pool, &router, &format!("{role}@acme.test")).await;
            add_member(&router, &owner, &ws, id, role).await;
            others.push(login);
        }
        let (_, stranger) = user(&pool, &router, "stranger@acme.test").await;

        for login in &others {
            let reply = invite(&router, Auth::Session(login), &ws, INVITEE, "viewer").await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{:?}", reply.body);
            let listed = call(
                &router,
                "GET",
                &format!("/api/v1/workspaces/{ws}/invitations"),
                None,
                Auth::Session(login),
            )
            .await;
            assert_eq!(listed.status, StatusCode::FORBIDDEN);
        }
        let reply = invite(&router, Auth::Session(&stranger), &ws, INVITEE, "viewer").await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        let reply = invite(&router, Auth::Session(&admin), &ws, INVITEE, "admin").await;
        assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
        assert_eq!(reply.body["role"], "admin");
        let id = reply.body["id"].as_str().unwrap().to_owned();

        for (scope, status) in [
            ("read", StatusCode::FORBIDDEN),
            ("admin", StatusCode::CREATED),
        ] {
            let key = call(
                &router,
                "POST",
                &format!("/api/v1/workspaces/{ws}/api-keys"),
                Some(json!({"name": "k", "scopes": [scope]})),
                Auth::Session(&owner),
            )
            .await;
            let secret = key.body["key"].as_str().unwrap();
            let reply = invite(
                &router,
                Auth::Key(secret),
                &ws,
                &format!("key-{scope}@acme.test"),
                "viewer",
            )
            .await;
            assert_eq!(reply.status, status, "{scope}: {:?}", reply.body);
        }
        let revoked = call(
            &router,
            "DELETE",
            &format!("/api/v1/workspaces/{ws}/invitations/{id}"),
            None,
            Auth::Session(&others[0]),
        )
        .await;
        assert_eq!(revoked.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_link_shows_the_workspace_and_only_a_hint_of_the_address(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (_, token) = invited(&router, &owner, &ws, INVITEE).await;

        let reply = look_up(&router, &token).await;

        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        assert_eq!(
            reply.body,
            json!({"workspace_name": "Acme", "role": "viewer", "email_hint": "c***@acme.test"})
        );
        assert!(!reply.body.to_string().contains(INVITEE));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_dead_link_answers_one_identical_404(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (revoked_id, revoked) = invited(&router, &owner, &ws, "a@acme.test").await;
        let (_, expired) = invited(&router, &owner, &ws, "b@acme.test").await;
        let (_, used) = invited(&router, &owner, &ws, "c@acme.test").await;
        call(
            &router,
            "DELETE",
            &format!("/api/v1/workspaces/{ws}/invitations/{revoked_id}"),
            None,
            Auth::Session(&owner),
        )
        .await;
        sqlx::query("UPDATE invitations SET expires_at = now() - interval '1 second' WHERE email = 'b@acme.test'")
            .execute(&pool)
            .await
            .unwrap();
        let (_, login_c) = user(&pool, &router, "c@acme.test").await;
        assert_eq!(
            accept_as(&router, &used, Auth::Session(&login_c))
                .await
                .status,
            StatusCode::OK
        );

        let unknown = look_up(&router, "no-such-token").await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        for token in [&revoked, &expired, &used] {
            let lookup = look_up(&router, token).await;
            assert_eq!(lookup.status, StatusCode::NOT_FOUND);
            assert_eq!(lookup.problem(), unknown.problem());
            let accept = accept_as(&router, token, Auth::Session(&login_c)).await;
            assert_eq!(accept.status, StatusCode::NOT_FOUND);
            assert_eq!(accept.problem(), unknown.problem());
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_expired_address_can_be_invited_again(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (_, old) = invited(&router, &owner, &ws, INVITEE).await;
        sqlx::query("UPDATE invitations SET expires_at = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();

        let (_, fresh) = invited(&router, &owner, &ws, INVITEE).await;

        assert_eq!(look_up(&router, &old).await.status, StatusCode::NOT_FOUND);
        assert_eq!(look_up(&router, &fresh).await.status, StatusCode::OK);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_expiry_is_one_seven_or_thirty_days_and_nothing_else(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;

        for days in [1, 7, 30] {
            let reply = call(
                &router,
                "POST",
                &format!("/api/v1/workspaces/{ws}/invitations"),
                Some(json!({"email": format!("d{days}@acme.test"), "role": "viewer", "expires_in_days": days})),
                Auth::Session(&owner),
            )
            .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{days}");
        }
        let seven: i64 = sqlx::query_scalar(
            "SELECT round(extract(epoch FROM expires_at - created_at) / 86400)::bigint \
             FROM invitations WHERE email = 'd7@acme.test'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(seven, 7);
        for body in [
            json!({"email": "e@acme.test", "role": "viewer", "expires_in_days": 3}),
            json!({"email": "not an address", "role": "viewer"}),
            json!({"email": "f@acme.test", "role": "owner"}),
        ] {
            let reply = call(
                &router,
                "POST",
                &format!("/api/v1/workspaces/{ws}/invitations"),
                Some(body),
                Auth::Session(&owner),
            )
            .await;
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{:?}",
                reply.body
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_revoked_link_is_dead_once_and_the_address_can_be_invited_again(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let other = workspace(&router, &owner, "Other").await;
        let (id, token) = invited(&router, &owner, &ws, INVITEE).await;
        let revoke = |ws: String, id: String| {
            let router = router.clone();
            let owner = owner.clone();
            async move {
                call(
                    &router,
                    "DELETE",
                    &format!("/api/v1/workspaces/{ws}/invitations/{id}"),
                    None,
                    Auth::Session(&owner),
                )
                .await
            }
        };

        assert_eq!(
            revoke(other.clone(), id.clone()).await.status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            revoke(ws.clone(), id.clone()).await.status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            revoke(ws.clone(), id.clone()).await.status,
            StatusCode::NOT_FOUND
        );

        assert_eq!(look_up(&router, &token).await.status, StatusCode::NOT_FOUND);
        invited(&router, &owner, &ws, INVITEE).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_accepts_at_once_make_one_membership(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (cy, login_cy) = user(&pool, &router, INVITEE).await;
        let (_, token) = invited(&router, &owner, &ws, INVITEE).await;

        let (first, second) = tokio::join!(
            accept_as(&router, &token, Auth::Session(&login_cy)),
            accept_as(&router, &token, Auth::Session(&login_cy)),
        );

        let mut statuses = [first.status, second.status];
        statuses.sort();
        assert_eq!(statuses, [StatusCode::OK, StatusCode::NOT_FOUND]);
        assert_eq!(role_of(&pool, &ws, cy).await.as_deref(), Some("viewer"));
        let members: i64 =
            sqlx::query_scalar("SELECT count(*) FROM memberships WHERE user_id = $1")
                .bind(cy.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(members, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_address_match_ignores_case_and_a_mismatch_keeps_the_invitation(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (cy, login_cy) = user(&pool, &router, INVITEE).await;
        sqlx::query("UPDATE users SET email = 'Cy@Acme.Test' WHERE id = $1")
            .bind(cy.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let (_, login_other) = user(&pool, &router, "other@acme.test").await;
        let (_, token) = invited(&router, &owner, &ws, INVITEE).await;

        let wrong = accept_as(&router, &token, Auth::Session(&login_other)).await;

        assert_eq!(wrong.status, StatusCode::FORBIDDEN, "{:?}", wrong.body);
        assert_eq!(wrong.body["code"], "invitation_address_mismatch");
        assert_eq!(look_up(&router, &token).await.status, StatusCode::OK);
        let right = accept_as(&router, &token, Auth::Session(&login_cy)).await;
        assert_eq!(right.status, StatusCode::OK, "{:?}", right.body);
        assert_eq!(right.body["role"], "viewer");
        assert_eq!(role_of(&pool, &ws, cy).await.as_deref(), Some("viewer"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn accepting_needs_a_session_and_refuses_a_key(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (_, token) = invited(&router, &owner, &ws, INVITEE).await;
        let key = call(
            &router,
            "POST",
            &format!("/api/v1/workspaces/{ws}/api-keys"),
            Some(json!({"name": "k", "scopes": ["admin"]})),
            Auth::Session(&owner),
        )
        .await;

        let anonymous = accept_as(&router, &token, Auth::Anonymous).await;
        let keyed = accept_as(
            &router,
            &token,
            Auth::Key(key.body["key"].as_str().unwrap()),
        )
        .await;

        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(keyed.status, StatusCode::FORBIDDEN);
        assert_eq!(look_up(&router, &token).await.status, StatusCode::OK);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_new_address_creates_its_account_signs_in_and_joins_in_one_step(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (_, token) = invited(&router, &owner, &ws, INVITEE).await;

        let reply = call(
            &router,
            "POST",
            &format!("/api/v1/invitations/{token}/accept"),
            Some(json!({"password": PASSWORD})),
            Auth::Anonymous,
        )
        .await;

        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        assert_eq!(reply.body["workspace_id"], ws.as_str());
        let cookie = reply.headers[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.starts_with("zf_session="), "{cookie}");
        let user = zirv_flow_store::find_user_by_email(
            &pool,
            &zirv_flow_core::Email::parse(INVITEE).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(user.password_hash.is_some());
        assert!(!user.is_owner);
        assert_eq!(
            role_of(&pool, &ws, user.id).await.as_deref(),
            Some("viewer")
        );
        let session = call(
            &router,
            "GET",
            "/api/v1/session",
            None,
            Auth::Session(&Login {
                cookie: cookie.split(';').next().unwrap().to_owned(),
                csrf: String::new(),
            }),
        )
        .await;
        assert_eq!(session.status, StatusCode::OK);
        assert_eq!(session.body["user"]["email"], INVITEE);
        assert_eq!(look_up(&router, &token).await.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn creating_an_account_needs_local_sign_in_a_new_address_and_a_good_password(
        pool: PgPool,
    ) {
        let off = app_with(&pool, false).await;
        let owner_login = {
            crate::admin::create_owner(&pool, &Context::system(), OWNER, PASSWORD)
                .await
                .unwrap();
            // Local sign-in is off, so the owner's session is made directly.
            let on = app(&pool).await;
            login(&on, OWNER).await
        };
        let ws = workspace(&off, &owner_login, "Acme").await;
        let (_, token) = invited(&off, &owner_login, &ws, INVITEE).await;
        let create = |router: &Router, password: &str, token: &str| {
            let router = router.clone();
            let body = json!({"password": password});
            let uri = format!("/api/v1/invitations/{token}/accept");
            async move { call(&router, "POST", &uri, Some(body), Auth::Anonymous).await }
        };

        let disabled = create(&off, PASSWORD, &token).await;
        let on = app(&pool).await;
        let short = create(&on, "short", &token).await;
        user(&pool, &on, "taken@acme.test").await;
        let (_, taken) = invited(&on, &owner_login, &ws, "taken@acme.test").await;
        let exists = create(&on, PASSWORD, &taken).await;

        assert_eq!(
            disabled.status,
            StatusCode::FORBIDDEN,
            "{:?}",
            disabled.body
        );
        assert_eq!(disabled.body["code"], "local_sign_in_disabled");
        assert_eq!(
            short.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            short.body
        );
        assert_eq!(exists.status, StatusCode::CONFLICT, "{:?}", exists.body);
        assert_eq!(exists.body["code"], "account_exists");
        let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE email = $1")
            .bind(INVITEE)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(users, 0);
        assert_eq!(look_up(&on, &token).await.status, StatusCode::OK);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_admin_may_invite_an_admin_and_accepting_does_not_bypass_the_last_admin_rule(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (cy, login_cy) = user(&pool, &router, INVITEE).await;
        let reply = call(
            &router,
            "POST",
            &format!("/api/v1/workspaces/{ws}/invitations"),
            Some(json!({"email": INVITEE, "role": "admin", "view_pii": true})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
        assert_eq!(reply.body["view_pii"], false);

        let accepted = accept_as(&router, &token_of(&reply.body), Auth::Session(&login_cy)).await;
        assert_eq!(accepted.status, StatusCode::OK, "{:?}", accepted.body);
        assert_eq!(role_of(&pool, &ws, cy).await.as_deref(), Some("admin"));

        let demoted = call(
            &router,
            "PUT",
            &format!("/api/v1/workspaces/{ws}/members/{cy}"),
            Some(json!({"role": "viewer"})),
            Auth::Session(&login_cy),
        )
        .await;
        assert_eq!(demoted.status, StatusCode::CONFLICT, "{:?}", demoted.body);
        assert_eq!(demoted.body["code"], "last_admin");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_member_or_a_pending_address_cannot_be_invited_again(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (cy, login_cy) = user(&pool, &router, INVITEE).await;
        let (_, token) = invited(&router, &owner, &ws, INVITEE).await;

        let pending = invite(
            &router,
            Auth::Session(&owner),
            &ws,
            "CY@acme.test",
            "editor",
        )
        .await;
        assert_eq!(pending.status, StatusCode::CONFLICT);
        assert_eq!(pending.body["code"], "invitation_pending");
        add_member(&router, &owner, &ws, cy, "editor").await;

        let at_accept = accept_as(&router, &token, Auth::Session(&login_cy)).await;
        assert_eq!(
            at_accept.status,
            StatusCode::CONFLICT,
            "{:?}",
            at_accept.body
        );
        assert_eq!(at_accept.body["code"], "already_member");
        let (second, _) = user(&pool, &router, "second@acme.test").await;
        add_member(&router, &owner, &ws, second, "viewer").await;
        let at_create = invite(
            &router,
            Auth::Session(&owner),
            &ws,
            "second@acme.test",
            "viewer",
        )
        .await;
        assert_eq!(at_create.status, StatusCode::CONFLICT);
        assert_eq!(at_create.body["code"], "already_member");
        assert_eq!(role_of(&pool, &ws, cy).await.as_deref(), Some("editor"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_three_actions_are_audited_with_ids_only_and_no_address_or_token(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (id_a, token_a) = invited(&router, &owner, &ws, "revoked@acme.test").await;
        call(
            &router,
            "DELETE",
            &format!("/api/v1/workspaces/{ws}/invitations/{id_a}"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let (_, token_b) = invited(&router, &owner, &ws, INVITEE).await;
        let accepted = call(
            &router,
            "POST",
            &format!("/api/v1/invitations/{token_b}/accept"),
            Some(json!({"password": PASSWORD})),
            Auth::Anonymous,
        )
        .await;
        assert_eq!(accepted.status, StatusCode::OK, "{:?}", accepted.body);

        let actions = audit_actions(&pool).await;
        for expected in [
            "invitation.created",
            "invitation.revoked",
            "invitation.accepted",
            "membership.set",
            "user.created",
        ] {
            assert!(
                actions.iter().any(|action| action == expected),
                "{expected}"
            );
        }
        let created: Value = sqlx::query_scalar(
            "SELECT summary FROM audit_log WHERE action = 'invitation.accepted'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            created,
            json!({"role": "viewer", "view_pii": false, "account_created": true})
        );
        let via: Value = sqlx::query_scalar(
            "SELECT summary FROM audit_log WHERE action = 'user.created' ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(via["via"], "invitation");
        let log: String = sqlx::query_scalar(
            "SELECT COALESCE(string_agg(summary::text || ' ' || COALESCE(request_id, '') || ' ' || action, ' '), '') FROM audit_log",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        for secret in [
            INVITEE,
            "revoked@acme.test",
            "acme.test",
            token_a.as_str(),
            token_b.as_str(),
        ] {
            assert!(!log.contains(secret), "{secret} in the audit log");
        }
        crate::test_support::assert_audit_log_holds_ids_only(&pool).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_eleventh_request_in_a_burst_is_rate_limited(pool: PgPool) {
        let router = app(&pool).await;

        let mut statuses = Vec::new();
        for _ in 0..12 {
            statuses.push(look_up(&router, "no-such-token").await.status);
        }

        assert!(
            statuses[..10]
                .iter()
                .all(|status| *status == StatusCode::NOT_FOUND)
        );
        let limited = look_up(&router, "no-such-token").await;
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(limited.body["code"], "rate_limited");
        assert!(limited.headers.contains_key(header::RETRY_AFTER));
    }
}
