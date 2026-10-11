//! Authentication and authorization.
//!
//! Layering, outermost first:
//!
//! 1. [`Principal`] is an extractor. It resolves the credential on the request into a
//!    session user or an API key, and for a session credential on a mutating method it
//!    also enforces the CSRF token. Handlers list it before their body extractor, so an
//!    unauthenticated or CSRF-less request is refused before its body is read.
//! 2. [`Principal::require_user`] and [`Principal::require_owner`] guard the
//!    instance-level endpoints; an API key never passes them.
//! 3. [`authorize`] guards a workspace's resources: 404 for a caller with no access (so
//!    existence does not leak), 403 for a caller who may see the workspace but not do this.

use std::str::FromStr;

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use zirv_flow_core::token::{hash_matches, sha256};
use zirv_flow_core::{Role, Scope, WorkspaceId};
use zirv_flow_store::{ApiKey, User, Workspace};

use crate::problem::ApiError;
use crate::{AppState, admin, cookies, credentials, request_id};

/// Idle timeout of a session.
pub(crate) const SESSION_IDLE_SECONDS: i64 = 12 * 3600;
/// Absolute lifetime of a session.
pub(crate) const SESSION_ABSOLUTE_SECONDS: i64 = cookies::SESSION_MAX_AGE_SECONDS;

const CSRF_HEADER: &str = "x-csrf-token";

/// The authenticated caller.
pub(crate) enum Principal {
    /// A person with a session cookie.
    Session(SessionPrincipal),
    /// A workspace API key.
    Key(KeyPrincipal),
}

pub(crate) struct SessionPrincipal {
    pub(crate) user: User,
    pub(crate) token_hash: [u8; 32],
    /// The stored hash of this session's CSRF token.
    csrf_hash: Vec<u8>,
    /// The CSRF token, re-derived from the session cookie so `GET /session` can show it.
    pub(crate) csrf_token: String,
}

pub(crate) struct KeyPrincipal {
    pub(crate) key: ApiKey,
}

/// What a workspace request needs from the caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Need {
    /// Read the workspace: any member, a workspace admin, the owner, or a `read` key.
    Read,
    /// Read the member list: as `Read`, and also an `admin` key.
    ReadMembers,
    /// Manage members and keys: a workspace admin, the owner, or an `admin` key.
    Admin,
}

impl Principal {
    pub(crate) fn context(&self) -> admin::Context {
        let context = match self {
            Self::Session(session) => admin::Context::user(session.user.id),
            Self::Key(key) => admin::Context::api_key(key.key.id),
        };
        context.with_request_id(request_id::current())
    }

    /// The signed-in user. An API key is refused: it belongs to one workspace and never
    /// reaches instance-level endpoints.
    pub(crate) fn require_user(&self) -> Result<&SessionPrincipal, ApiError> {
        match self {
            Self::Session(session) => Ok(session),
            Self::Key(_) => Err(ApiError::forbidden(
                "API keys cannot use instance-level endpoints",
            )),
        }
    }

    pub(crate) fn require_owner(&self) -> Result<&SessionPrincipal, ApiError> {
        let session = self.require_user()?;
        if session.user.is_owner {
            Ok(session)
        } else {
            Err(ApiError::forbidden("only the instance owner can do this"))
        }
    }
}

impl FromRequestParts<AppState> for Principal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(authorization) = parts.headers.get(AUTHORIZATION) {
            // An Authorization header decides the matter; a bad one never falls back to
            // the cookie.
            let value = authorization
                .to_str()
                .map_err(|_| ApiError::unauthenticated())?;
            let key = bearer(value).ok_or_else(ApiError::unauthenticated)?;
            return authenticate_key(state, key).await.map(Self::Key);
        }
        let token = cookies::read(&parts.headers, cookies::SESSION_COOKIE)
            .ok_or_else(ApiError::unauthenticated)?;
        let session = authenticate_session(state, token).await?;
        if needs_csrf(&parts.method) {
            check_csrf(&parts.headers, &session)?;
        }
        Ok(Self::Session(session))
    }
}

/// The token of an `Authorization: Bearer <token>` value; the scheme is case-insensitive.
fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

async fn authenticate_key(state: &AppState, presented: &str) -> Result<KeyPrincipal, ApiError> {
    let (id, secret) = credentials::parse_key(presented).ok_or_else(ApiError::unauthenticated)?;
    let stored = zirv_flow_store::find_api_key_for_auth(&state.pool, id)
        .await?
        .ok_or_else(ApiError::unauthenticated)?;
    if !hash_matches(secret, &stored.secret_hash) || stored.key.revoked_at.is_some() {
        return Err(ApiError::unauthenticated());
    }
    zirv_flow_store::touch_api_key(&state.pool, id).await?;
    Ok(KeyPrincipal { key: stored.key })
}

async fn authenticate_session(state: &AppState, token: &str) -> Result<SessionPrincipal, ApiError> {
    let token_hash = sha256(token);
    let session =
        zirv_flow_store::find_active_session(&state.pool, &token_hash, SESSION_IDLE_SECONDS)
            .await?
            .ok_or_else(ApiError::unauthenticated)?;
    zirv_flow_store::touch_session(&state.pool, &token_hash).await?;
    Ok(SessionPrincipal {
        user: session.user,
        token_hash,
        csrf_hash: session.csrf_hash,
        csrf_token: credentials::csrf_token(token),
    })
}

fn needs_csrf(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// The `x-csrf-token` header must hash to the stored CSRF hash, compared in constant time.
fn check_csrf(headers: &HeaderMap, session: &SessionPrincipal) -> Result<(), ApiError> {
    let presented = headers
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok());
    let valid = presented.is_some_and(|presented| hash_matches(presented, &session.csrf_hash));
    if valid {
        Ok(())
    } else {
        Err(ApiError::forbidden("missing or invalid CSRF token"))
    }
}

/// Whether a key's scopes allow the request. Scopes do not imply one another.
pub(crate) fn scopes_allow(scopes: &[Scope], need: Need) -> bool {
    match need {
        Need::Read => scopes.contains(&Scope::Read),
        Need::ReadMembers => scopes.contains(&Scope::Read) || scopes.contains(&Scope::Admin),
        Need::Admin => scopes.contains(&Scope::Admin),
    }
}

/// Whether a workspace role (or the owner flag) allows the request.
pub(crate) fn role_allows(role: Role, need: Need) -> bool {
    match need {
        Need::Read | Need::ReadMembers => true,
        Need::Admin => role == Role::Admin,
    }
}

/// Decides whether `principal` may do `need` in workspace `id`, and returns the workspace.
///
/// A caller with no access at all gets 404, whether or not the workspace exists, so the
/// response does not reveal it. Only a caller who may see the workspace gets a 403.
pub(crate) async fn authorize(
    state: &AppState,
    principal: &Principal,
    id: WorkspaceId,
    need: Need,
) -> Result<Workspace, ApiError> {
    match principal {
        Principal::Key(key) => {
            if key.key.workspace_id != id {
                return Err(ApiError::not_found());
            }
            if !scopes_allow(&key.key.scopes, need) {
                return Err(ApiError::forbidden("the API key lacks the required scope"));
            }
        }
        Principal::Session(session) if session.user.is_owner => {}
        Principal::Session(session) => {
            let role = zirv_flow_store::membership_role(&state.pool, id, session.user.id)
                .await?
                .ok_or_else(ApiError::not_found)?;
            if !role_allows(role, need) {
                return Err(ApiError::forbidden(
                    "this needs the admin role in the workspace",
                ));
            }
        }
    }
    zirv_flow_store::find_workspace(&state.pool, id)
        .await?
        .ok_or_else(ApiError::not_found)
}

/// Parses an id from a path segment; a malformed id is a plain 404.
pub(crate) fn path_id<T: FromStr>(segment: &str) -> Result<T, ApiError> {
    segment.parse().map_err(|_| ApiError::not_found())
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use time::OffsetDateTime;
    use zirv_flow_core::UserId;

    use super::*;

    fn user(is_owner: bool) -> User {
        User {
            id: UserId::new(),
            email: "a@example.com".into(),
            password_hash: None,
            is_owner,
            failed_sign_ins: 0,
            locked_until: None,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    fn session(is_owner: bool) -> SessionPrincipal {
        SessionPrincipal {
            user: user(is_owner),
            token_hash: [0; 32],
            csrf_hash: sha256(&credentials::csrf_token("token")).to_vec(),
            csrf_token: credentials::csrf_token("token"),
        }
    }

    #[test]
    fn the_bearer_scheme_is_case_insensitive_and_needs_a_token() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer abc"), Some("abc"));
        assert_eq!(bearer("BEARER  abc "), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearer"), None);
        assert_eq!(bearer("Bearer   "), None);
        assert_eq!(bearer(""), None);
    }

    #[test]
    fn only_mutating_methods_need_a_csrf_token() {
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert!(needs_csrf(&method), "{method}");
        }
        for method in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(!needs_csrf(&method), "{method}");
        }
    }

    #[test]
    fn the_csrf_header_must_match_the_session_token() {
        let session = session(false);
        let with = |value: Option<&str>| {
            let mut headers = HeaderMap::new();
            if let Some(value) = value {
                headers.insert(CSRF_HEADER, HeaderValue::from_str(value).unwrap());
            }
            headers
        };

        assert!(check_csrf(&with(Some(&session.csrf_token)), &session).is_ok());
        assert!(check_csrf(&with(None), &session).is_err());
        assert!(check_csrf(&with(Some("wrong")), &session).is_err());
        assert!(check_csrf(&with(Some("")), &session).is_err());
        let truncated = &session.csrf_token[..session.csrf_token.len() - 1];
        assert!(check_csrf(&with(Some(truncated)), &session).is_err());
    }

    #[test]
    fn scopes_do_not_imply_each_other() {
        assert!(scopes_allow(&[Scope::Read], Need::Read));
        assert!(!scopes_allow(&[Scope::Write], Need::Read));
        assert!(!scopes_allow(&[Scope::Ingest], Need::Read));
        assert!(!scopes_allow(&[Scope::Admin], Need::Read));
        assert!(scopes_allow(&[Scope::Admin], Need::ReadMembers));
        assert!(scopes_allow(&[Scope::Read], Need::ReadMembers));
        assert!(!scopes_allow(&[Scope::Write], Need::ReadMembers));
        assert!(scopes_allow(&[Scope::Admin], Need::Admin));
        assert!(!scopes_allow(&[Scope::Read, Scope::Write], Need::Admin));
        assert!(!scopes_allow(&[], Need::Read));
    }

    #[test]
    fn members_read_but_only_admins_manage() {
        assert!(role_allows(Role::Member, Need::Read));
        assert!(role_allows(Role::Member, Need::ReadMembers));
        assert!(!role_allows(Role::Member, Need::Admin));
        assert!(role_allows(Role::Admin, Need::Admin));
    }

    #[test]
    fn an_api_key_never_passes_the_instance_level_guards() {
        let key = Principal::Key(KeyPrincipal {
            key: ApiKey {
                id: zirv_flow_core::ApiKeyId::new(),
                workspace_id: WorkspaceId::new(),
                name: "ci".into(),
                scopes: vec![Scope::Admin, Scope::Write, Scope::Read],
                created_by: None,
                created_at: OffsetDateTime::now_utc(),
                last_used_at: None,
                revoked_at: None,
            },
        });

        assert!(key.require_user().is_err());
        assert!(key.require_owner().is_err());
    }

    #[test]
    fn only_the_owner_passes_the_owner_guard() {
        assert!(Principal::Session(session(true)).require_owner().is_ok());
        let member = Principal::Session(session(false));
        assert!(member.require_user().is_ok());
        assert!(member.require_owner().is_err());
    }

    #[test]
    fn a_malformed_workspace_id_is_a_404() {
        let error = path_id::<WorkspaceId>("not-a-uuid").err().unwrap();

        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
        assert!(path_id::<WorkspaceId>(&WorkspaceId::new().to_string()).is_ok());
    }
}
