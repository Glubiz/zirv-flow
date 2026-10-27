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
use zirv_flow_core::{Capability, Role, Scope, UserId, WorkspaceId};
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
    /// Change drafts and working data, such as lists and previews: an editor, a publisher, a
    /// workspace admin, the owner, or a `write` key.
    Edit,
    /// Publish, pause or archive, and the operator actions on running work: a publisher, a
    /// workspace admin, the owner, or a `publish` key.
    Publish,
    /// Manage members, keys, sources and governance: a workspace admin, the owner, or an
    /// `admin` key.
    Admin,
    /// Send events and profile updates: an API key with the `ingest` scope, and nobody
    /// else.
    Ingest,
}

impl Need {
    /// The role the 403 names, for a need a role can fail.
    const fn role_name(self) -> &'static str {
        match self {
            Self::Publish => "publisher",
            Self::Edit => "editor",
            Self::Admin => "admin",
            Self::Read | Self::ReadMembers | Self::Ingest => "member",
        }
    }
}

impl Principal {
    /// The signed-in user, or none for an API key.
    pub(crate) fn user_id(&self) -> Option<UserId> {
        match self {
            Self::Session(session) => Some(session.user.id),
            Self::Key(_) => None,
        }
    }

    pub(crate) fn context(&self) -> admin::Context {
        let context = match self {
            Self::Session(session) => {
                admin::Context::user(session.user.id).with_owner(session.user.is_owner)
            }
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

    /// The API key of an ingestion request. A session is refused: ingestion is for keys
    /// only, which also keeps it free of CSRF. A key without the `ingest` scope is refused.
    pub(crate) fn require_ingest_key(&self) -> Result<&KeyPrincipal, ApiError> {
        match self {
            Self::Key(key) if scopes_allow(&key.key.scopes, Need::Ingest) => Ok(key),
            Self::Key(_) => Err(ApiError::forbidden("the API key lacks the required scope")),
            Self::Session(_) => Err(ApiError::forbidden(
                "ingestion needs an API key with the ingest scope",
            )),
        }
    }
}

#[cfg(test)]
impl Principal {
    /// A signed-in principal without a request, for tests of code that takes one.
    pub(crate) fn session_for_tests(user: User) -> Self {
        Self::Session(SessionPrincipal {
            user,
            token_hash: [0; 32],
            csrf_hash: Vec::new(),
            csrf_token: String::new(),
        })
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
        Need::Edit => scopes.contains(&Scope::Write),
        Need::Publish => scopes.contains(&Scope::Publish),
        Need::Admin => scopes.contains(&Scope::Admin),
        Need::Ingest => scopes.contains(&Scope::Ingest),
    }
}

/// Whether a workspace role (or the owner flag) allows the request.
pub(crate) fn role_allows(role: Role, need: Need) -> bool {
    match need {
        Need::Read | Need::ReadMembers => true,
        Need::Edit => role.allows(Capability::Edit),
        Need::Publish => role.allows(Capability::Publish),
        Need::Admin => role.allows(Capability::Admin),
        Need::Ingest => false,
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
                return Err(ApiError::forbidden(format!(
                    "this needs the {} role in the workspace",
                    need.role_name()
                )));
            }
        }
    }
    zirv_flow_store::find_workspace(&state.pool, id)
        .await?
        .ok_or_else(ApiError::not_found)
}

/// Who is asking, as far as seeing personal values goes.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PiiHolder<'a> {
    /// The instance owner.
    Owner,
    /// A workspace member with their role and `view_pii` grant.
    Member { role: Role, view_pii: bool },
    /// An API key with its scopes.
    Key(&'a [Scope]),
}

/// Whether the holder sees personal values (ruling ca362e00): the owner, a workspace admin and an
/// `admin` key always; a publisher, editor or viewer with the grant; any other key with the `pii`
/// scope. `pii` alone reads nothing, because it does not imply `read`.
pub(crate) fn reveals_pii(holder: PiiHolder<'_>) -> bool {
    match holder {
        PiiHolder::Owner => true,
        PiiHolder::Member { role, view_pii } => role == Role::Admin || view_pii,
        PiiHolder::Key(scopes) => scopes.contains(&Scope::Admin) || scopes.contains(&Scope::Pii),
    }
}

/// Whether the caller may see personal values such as identifiers and consent evidence. Everyone
/// else who may read the workspace sees them masked.
pub(crate) async fn may_view_pii(
    state: &AppState,
    principal: &Principal,
    workspace: WorkspaceId,
) -> bool {
    match principal {
        Principal::Key(key) => {
            key.key.workspace_id == workspace && reveals_pii(PiiHolder::Key(&key.key.scopes))
        }
        Principal::Session(session) if session.user.is_owner => reveals_pii(PiiHolder::Owner),
        Principal::Session(session) => {
            zirv_flow_store::find_membership(&state.pool, workspace, session.user.id)
                .await
                .ok()
                .flatten()
                .is_some_and(|member| {
                    reveals_pii(PiiHolder::Member {
                        role: member.role,
                        view_pii: member.view_pii,
                    })
                })
        }
    }
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
        assert!(scopes_allow(&[Scope::Write], Need::Edit));
        assert!(!scopes_allow(&[Scope::Read, Scope::Admin], Need::Edit));
        assert!(scopes_allow(&[Scope::Admin], Need::Admin));
        assert!(!scopes_allow(&[Scope::Read, Scope::Write], Need::Admin));
        assert!(!scopes_allow(&[], Need::Read));
        assert!(scopes_allow(&[Scope::Write], Need::Edit));
        assert!(!scopes_allow(
            &[Scope::Read, Scope::Admin, Scope::Ingest],
            Need::Edit
        ));
        assert!(scopes_allow(&[Scope::Ingest], Need::Ingest));
        for other in [Scope::Read, Scope::Write, Scope::Publish, Scope::Admin] {
            assert!(!scopes_allow(&[other], Need::Ingest));
        }
    }

    #[test]
    fn the_truth_table_of_who_sees_personal_values() {
        assert!(reveals_pii(PiiHolder::Owner));
        for (role, view_pii, expected) in [
            (Role::Admin, false, true),
            (Role::Admin, true, true),
            (Role::Publisher, false, false),
            (Role::Publisher, true, true),
            (Role::Editor, false, false),
            (Role::Editor, true, true),
            (Role::Viewer, false, false),
            (Role::Viewer, true, true),
        ] {
            assert_eq!(
                reveals_pii(PiiHolder::Member { role, view_pii }),
                expected,
                "{role:?} with the grant {view_pii}"
            );
        }
        let all = [
            Scope::Ingest,
            Scope::Read,
            Scope::Write,
            Scope::Publish,
            Scope::Admin,
            Scope::Pii,
        ];
        for mask in 0..(1_u32 << all.len()) {
            let scopes: Vec<Scope> = all
                .iter()
                .enumerate()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(_, scope)| *scope)
                .collect();
            let expected = scopes.contains(&Scope::Admin) || scopes.contains(&Scope::Pii);
            assert_eq!(reveals_pii(PiiHolder::Key(&scopes)), expected, "{scopes:?}");
        }
    }

    #[test]
    fn the_pii_scope_alone_reads_nothing_and_adds_nothing_to_another_scope() {
        for need in [
            Need::Read,
            Need::ReadMembers,
            Need::Edit,
            Need::Publish,
            Need::Admin,
            Need::Ingest,
        ] {
            assert!(!scopes_allow(&[Scope::Pii], need), "{need:?}");
        }
        assert!(scopes_allow(&[Scope::Read, Scope::Pii], Need::Read));
        assert!(!scopes_allow(&[Scope::Read, Scope::Pii], Need::Edit));
    }

    #[test]
    fn a_write_key_cannot_publish_and_a_publish_key_cannot_edit() {
        assert!(scopes_allow(&[Scope::Publish], Need::Publish));
        assert!(!scopes_allow(&[Scope::Write], Need::Publish));
        assert!(!scopes_allow(&[Scope::Read, Scope::Admin], Need::Publish));
        assert!(!scopes_allow(&[Scope::Publish], Need::Edit));
        assert!(!scopes_allow(&[Scope::Publish], Need::Read));
        assert!(!scopes_allow(&[Scope::Publish], Need::Admin));
        assert!(scopes_allow(&[Scope::Write, Scope::Publish], Need::Edit));
        assert!(scopes_allow(&[Scope::Write, Scope::Publish], Need::Publish));
    }

    #[test]
    fn an_editor_may_edit_and_a_key_belongs_to_no_user() {
        assert!(role_allows(Role::Editor, Need::Edit));
        assert!(role_allows(Role::Admin, Need::Edit));
        let member = session(false);
        let id = member.user.id;
        assert_eq!(Principal::Session(member).user_id(), Some(id));
        let key = Principal::Key(KeyPrincipal {
            key: ApiKey {
                id: zirv_flow_core::ApiKeyId::new(),
                workspace_id: WorkspaceId::new(),
                name: "ci".into(),
                scopes: vec![Scope::Write],
                created_by: None,
                created_at: OffsetDateTime::now_utc(),
                last_used_at: None,
                revoked_at: None,
            },
        });
        assert_eq!(key.user_id(), None);
    }

    #[test]
    fn each_role_holds_its_own_needs_and_those_below() {
        let table = [
            (Role::Viewer, [true, true, false, false, false]),
            (Role::Editor, [true, true, true, false, false]),
            (Role::Publisher, [true, true, true, true, false]),
            (Role::Admin, [true, true, true, true, true]),
        ];
        for (role, expected) in table {
            let needs = [
                Need::Read,
                Need::ReadMembers,
                Need::Edit,
                Need::Publish,
                Need::Admin,
            ];
            for (need, allowed) in needs.into_iter().zip(expected) {
                assert_eq!(role_allows(role, need), allowed, "{role} {need:?}");
            }
            assert!(!role_allows(role, Need::Ingest), "{role}");
        }
    }

    #[test]
    fn the_refusal_names_the_role_the_need_takes() {
        assert_eq!(Need::Edit.role_name(), "editor");
        assert_eq!(Need::Publish.role_name(), "publisher");
        assert_eq!(Need::Admin.role_name(), "admin");
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

    #[test]
    fn only_a_key_with_the_ingest_scope_passes_the_ingest_guard() {
        let key = |scopes: Vec<Scope>| {
            Principal::Key(KeyPrincipal {
                key: ApiKey {
                    id: zirv_flow_core::ApiKeyId::new(),
                    workspace_id: WorkspaceId::new(),
                    name: "ci".into(),
                    scopes,
                    created_by: None,
                    created_at: OffsetDateTime::now_utc(),
                    last_used_at: None,
                    revoked_at: None,
                },
            })
        };

        assert!(key(vec![Scope::Ingest]).require_ingest_key().is_ok());
        assert!(
            key(vec![Scope::Read, Scope::Write, Scope::Admin])
                .require_ingest_key()
                .is_err()
        );
        assert!(
            Principal::Session(session(true))
                .require_ingest_key()
                .is_err()
        );
    }
}
