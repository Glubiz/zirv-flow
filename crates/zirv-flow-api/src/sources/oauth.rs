//! Connecting an account to an `http` source by OAuth2 consent (architecture 7.5): the two legs of
//! the authorization-code flow with PKCE.
//!
//! `start` (an admin with a session) stores a single-use server-side state, hashed, with the PKCE
//! verifier, the token binding of the manifest, the source and the user, for ten minutes, and
//! returns the provider's authorization URL. `callback` takes that state first, so a replay always
//! finds nothing and a failure later never leaves a usable state; it checks that the browser that
//! returns is the signed-in admin who left and that the manifest still has the binding the consent
//! started under (else `state_mismatch`, before the code goes anywhere), exchanges the code
//! through the guarded client and stores both tokens in one `oauth_token` secret, after checking the
//! binding again under the source's row lock. Every failure stores nothing and redirects the browser to the source
//! page (or, when no state was found, to the front page) with `?oauth=failed&reason=<code>`.
//!
//! No code, state, token, verifier or URL reaches a log line, an audit entry or a metric label:
//! only the reason, one of `unknown_state`, `state_mismatch`, `provider_error`, `exchange_failed`,
//! `no_refresh_token` and `source_gone`.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::header;
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use serde_json::json;
use url::Url;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_connectors::manifest::{self, Auth as ManifestAuth};
use zirv_flow_connectors::runtime::{CodeExchange, RuntimeError, exchange_code};
use zirv_flow_connectors::sources::{Credential, active_binding};
use zirv_flow_core::token::{random_token, sha256};
use zirv_flow_core::{SecretId, SecretPurpose, WorkspaceId};
use zirv_flow_store::{NewOAuthState, Source, StoredTokens};

use super::actions::network_context;
use super::{credential, kind_unavailable, registry};
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::oidc::{Callback, NO_STORE};
use crate::problem::{ApiError, Problem};
use crate::{AppState, request_id};

/// Where the provider sends the browser back to, for every source; registered once.
const CALLBACK_PATH: &str = "/api/v1/oauth/callback";
/// How long a consent may take.
const STATE_SECONDS: i64 = 600;

/// The redirect URI to register at the provider for a server at `public_url`.
fn redirect_uri(public_url: &str) -> String {
    format!("{}{CALLBACK_PATH}", public_url.trim_end_matches('/'))
}

/// The S256 challenge of a PKCE verifier (RFC 7636).
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(sha256(verifier))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct StartBody {
    /// Where to send the browser: the provider's consent page. The page that gets it navigates
    /// there; the server never fetches it.
    authorization_url: String,
}

/// The authorization-code manifest of a source: its authorize URL and scopes.
struct Consent {
    authorize_url: Url,
    scopes: Vec<String>,
}

fn consent_of(source: &Source) -> Result<Consent, ApiError> {
    let manifest = source
        .config
        .get("manifest")
        .and_then(|value| manifest::validate(value).ok())
        .ok_or_else(ApiError::oauth_not_configured)?;
    match manifest.auth {
        ManifestAuth::OAuth2AuthorizationCode {
            authorize_url,
            scopes,
            ..
        } => Ok(Consent {
            authorize_url,
            scopes,
        }),
        _ => Err(ApiError::oauth_not_configured()),
    }
}

/// The authorization URL: the manifest's authorize URL with the standard parameters.
fn authorization_url(
    consent: &Consent,
    client_id: &str,
    redirect: &str,
    state: &str,
    verifier: &str,
) -> String {
    let mut url = consent.authorize_url.clone();
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", client_id);
        query.append_pair("redirect_uri", redirect);
        if !consent.scopes.is_empty() {
            query.append_pair("scope", &consent.scopes.join(" "));
        }
        query.append_pair("state", state);
        query.append_pair("code_challenge", &challenge(verifier));
        query.append_pair("code_challenge_method", "S256");
    }
    url.into()
}

/// Starts connecting an account: stores a single-use state and returns the provider's consent
/// URL, which the page navigates to. A person's session is needed; an API key is refused.
/// Workspace admins only. Audited as `source.oauth_started`.
#[utoipa::path(
    post,
    operation_id = "start_source_oauth",
    path = "/api/v1/workspaces/{ws}/sources/{id}/oauth/start",
    tag = "sources",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Source id")
    ),
    responses(
        (status = 200, description = "Where to send the browser", body = StartBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, an API key (a consent needs a person), or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or source, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The source does not use the authorization-code grant (`code` is `oauth_not_configured`), or the server has no `server.public_url` (`code` is `public_url_not_configured`), or the type is not in this build", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The source has no `client_id` in its credential", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn start(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<StartBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let user = principal.user_id().ok_or_else(|| {
        ApiError::forbidden("connecting an account needs a signed-in person, not an API key")
    })?;
    let source_id: Uuid = path_id(&id)?;
    let source = zirv_flow_store::find_source(&state.pool, workspace.id, source_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    registry().get(&source.kind).ok_or_else(kind_unavailable)?;
    let consent = consent_of(&source)?;
    let public_url = state
        .public_url()
        .ok_or_else(ApiError::public_url_not_configured)?;
    if !source
        .credential_fields
        .iter()
        .any(|name| name == "client_id")
    {
        return Err(super::invalid("credential.client_id", "is required"));
    }
    let credential = credential::load(&state, workspace.id, source.credential_id).await?;
    let client_id = credential
        .get("client_id")
        .ok_or_else(|| super::invalid("credential.client_id", "is required"))?;
    let binding =
        active_binding(&source.config, &credential).ok_or_else(ApiError::oauth_not_configured)?;

    let consent_state = random_token(32);
    let verifier = random_token(32);
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    zirv_flow_store::delete_expired_oauth_states(&mut *tx).await?;
    zirv_flow_store::create_oauth_state(
        &mut *tx,
        &NewOAuthState {
            workspace: workspace.id,
            source: source_id,
            user: user.as_uuid(),
            state_hash: &sha256(&consent_state),
            pkce_verifier: &verifier,
            binding: &binding,
            ttl_seconds: STATE_SECONDS,
        },
    )
    .await?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "source.oauth_started",
        Some(("source", source_id)),
        json!({}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;

    Ok(Json(StartBody {
        authorization_url: authorization_url(
            &consent,
            client_id.expose(),
            &redirect_uri(public_url),
            &consent_state,
            &verifier,
        ),
    }))
}

/// Why a callback did not connect an account, and where the consent was started when that is
/// known.
struct Failure {
    reason: &'static str,
    at: Option<(WorkspaceId, Uuid)>,
    error: Option<Box<ApiError>>,
}

impl Failure {
    fn new(reason: &'static str) -> Self {
        Self {
            reason,
            at: None,
            error: None,
        }
    }

    fn at(mut self, workspace: WorkspaceId, source: Uuid) -> Self {
        self.at = Some((workspace, source));
        self
    }

    /// A server fault: answered as one, not as a failed consent.
    fn fault(error: impl Into<ApiError>) -> Self {
        Self {
            reason: "exchange_failed",
            at: None,
            error: Some(Box::new(error.into())),
        }
    }
}

fn count(outcome: &'static str) {
    metrics::counter!("zirv_flow_source_oauth_total", "outcome" => outcome).increment(1);
}

/// Where the browser lands after the consent: the source page, or the front page when no state
/// was found and so no workspace is known.
fn landing(public_url: &str, at: Option<(WorkspaceId, Uuid)>, outcome: &str) -> String {
    let base = public_url.trim_end_matches('/');
    match at {
        Some((workspace, source)) => format!(
            "{base}/w/{}/sources/{source}?oauth={outcome}",
            workspace.as_uuid()
        ),
        None => format!("{base}/?oauth={outcome}"),
    }
}

fn redirect(target: &str) -> Response {
    let mut response = Redirect::to(target).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, NO_STORE);
    response
}

/// Finishes connecting an account: the provider sends the browser here with `code` and `state`.
///
/// Always answers `303` to the source page (or the front page when the state is unknown), with
/// `?oauth=connected`, or `?oauth=failed&reason=<code>` and nothing stored. The browser must hold
/// the session of the admin who started.
#[utoipa::path(
    get,
    operation_id = "finish_source_oauth",
    path = "/api/v1/oauth/callback",
    tag = "sources",
    params(
        ("code" = Option<String>, Query, description = "The authorization code"),
        ("state" = Option<String>, Query, description = "The single-use state `start` issued"),
        ("error" = Option<String>, Query, description = "Set by the provider when it refused")
    ),
    responses(
        (status = 303, description = "Redirect to the source page with `?oauth=connected`, or `?oauth=failed&reason=` one of `unknown_state`, `state_mismatch` (also when the manifest's authentication changed since the consent started), `provider_error`, `exchange_failed`, `no_refresh_token`, `source_gone`"),
        (status = 503, description = "The server has no master key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []))
)]
pub(crate) async fn callback(
    State(state): State<AppState>,
    principal: Result<Principal, ApiError>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let public_url = state.public_url().unwrap_or_default().to_owned();
    let query = Callback::parse(query.as_deref().unwrap_or_default());
    match complete(&state, principal.as_ref().ok(), query).await {
        Ok((workspace, source)) => {
            count("connected");
            Ok(redirect(&landing(
                &public_url,
                Some((workspace, source)),
                "connected",
            )))
        }
        Err(Failure {
            error: Some(error), ..
        }) => Err(*error),
        Err(failure) => {
            count(failure.reason);
            if let Some((workspace, source)) = failure.at {
                record_failure(
                    &state,
                    principal.as_ref().ok(),
                    workspace,
                    source,
                    failure.reason,
                )
                .await?;
            }
            let outcome = format!("failed&reason={}", failure.reason);
            Ok(redirect(&landing(&public_url, failure.at, &outcome)))
        }
    }
}

async fn complete(
    state: &AppState,
    principal: Option<&Principal>,
    query: Callback,
) -> Result<(WorkspaceId, Uuid), Failure> {
    // Taking the state deletes it, so a replayed callback finds nothing, and a failure below
    // never leaves a usable state behind.
    let presented = query
        .state
        .filter(|state| !state.is_empty())
        .ok_or(Failure::new("unknown_state"))?;
    let consent = zirv_flow_store::take_oauth_state(&state.pool, &sha256(&presented))
        .await
        .map_err(Failure::fault)?
        .ok_or(Failure::new("unknown_state"))?;
    let (workspace, source_id) = (consent.workspace_id, consent.source_id);
    let failed = |reason: &'static str| Failure::new(reason).at(workspace, source_id);

    // The browser that returns must be the signed-in admin who left (a SameSite=Lax session
    // cookie rides this top-level GET), and must still be an admin of the workspace.
    let Some(principal @ Principal::Session(session)) = principal else {
        return Err(failed("state_mismatch"));
    };
    if session.user.id.as_uuid() != consent.user_id
        || authorize(state, principal, workspace, Need::Admin)
            .await
            .is_err()
    {
        return Err(failed("state_mismatch"));
    }
    if query.error.is_some() {
        return Err(failed("provider_error"));
    }
    let code = query
        .code
        .filter(|code| !code.is_empty())
        .ok_or_else(|| failed("provider_error"))?;

    let source = zirv_flow_store::find_source(&state.pool, workspace, source_id)
        .await
        .map_err(Failure::fault)?
        .ok_or_else(|| failed("source_gone"))?;
    let driver = registry()
        .get(&source.kind)
        .ok_or_else(|| failed("source_gone"))?;
    let manifest = source
        .config
        .get("manifest")
        .and_then(|value| manifest::validate(value).ok())
        .ok_or_else(|| failed("source_gone"))?;
    let ManifestAuth::OAuth2AuthorizationCode {
        token_url,
        scopes,
        client_auth,
        ..
    } = &manifest.auth
    else {
        return Err(failed("source_gone"));
    };
    let public_url = state
        .public_url()
        .ok_or_else(|| failed("exchange_failed"))?;
    let credential = credential::load(state, workspace, source.credential_id)
        .await
        .map_err(Failure::fault)?;
    let client_id = credential
        .get("client_id")
        .ok_or_else(|| failed("exchange_failed"))?;
    // The configuration may have changed since the consent started: the code and the verifier go
    // only to the token endpoint the consent was for.
    if active_binding(&source.config, &credential).as_deref() != Some(consent.binding.as_str()) {
        return Err(failed("state_mismatch"));
    }
    let context = network_context(
        state,
        driver.spec().kind,
        driver.rate_limit(&source.config),
        source_id,
    )
    .map_err(Failure::fault)?;

    let token = exchange_code(
        &context,
        CodeExchange {
            token_url,
            client_id: client_id.expose(),
            client_secret: credential.get("client_secret"),
            code: &code,
            redirect_uri: &redirect_uri(public_url),
            pkce_verifier: &consent.pkce_verifier,
            scopes,
            client_auth: *client_auth,
        },
    )
    .await
    .map_err(|error| match error {
        RuntimeError::NoRefreshToken => failed("no_refresh_token"),
        _ => failed("exchange_failed"),
    })?;
    let stored = super::tokens::to_stored(&token).map_err(|_| failed("no_refresh_token"))?;
    let saved = store_tokens(state, principal, workspace, &source, &credential, &stored)
        .await
        .map_err(|error| Failure {
            error: Some(Box::new(error)),
            ..failed("exchange_failed")
        })?;
    if !saved {
        return Err(failed("state_mismatch"));
    }
    Ok((workspace, source_id))
}

/// Seals the tokens into the source's `oauth_token` secret (replacing it on a reconnect), links
/// it, and audits `source.oauth_connected`, all in one transaction. The locked source row is checked
/// again: tokens issued under a configuration a save has since replaced are not stored (`false`).
async fn store_tokens(
    state: &AppState,
    principal: &Principal,
    workspace: WorkspaceId,
    source: &Source,
    credential: &Credential,
    tokens: &StoredTokens,
) -> Result<bool, ApiError> {
    let plaintext = tokens.to_plaintext().map_err(|_| ApiError::internal())?;
    let keyring = credential::keyring(state)?;
    let context = principal.context();
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    // Lock the source first: two consents finishing together replace one secret in turn.
    let locked = zirv_flow_store::find_source_for_update(&mut tx, workspace, source.id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if locked.credential_secret_id != source.credential_secret_id
        || active_binding(&locked.config, credential).as_deref() != Some(tokens.binding.as_str())
    {
        return Ok(false);
    }
    let linked = match locked.oauth_secret_id {
        Some(secret) => {
            admin::replace_secret_in(
                &mut tx,
                &context,
                keyring,
                workspace,
                SecretId::from_uuid(secret),
                &plaintext,
            )
            .await?;
            None
        }
        None => Some(
            admin::create_secret_in(
                &mut tx,
                &context,
                keyring,
                workspace,
                SecretPurpose::OAuthToken,
                &plaintext,
            )
            .await?,
        ),
    };
    zirv_flow_store::link_tokens(&mut tx, workspace, source.id, linked).await?;
    admin::audit(
        &mut tx,
        &context,
        Some(workspace),
        "source.oauth_connected",
        Some(("source", source.id)),
        json!({}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(true)
}

/// Audits `source.oauth_failed` with the reason, for a consent whose state was found.
async fn record_failure(
    state: &AppState,
    principal: Option<&Principal>,
    workspace: WorkspaceId,
    source: Uuid,
    reason: &'static str,
) -> Result<(), ApiError> {
    let context = match principal {
        Some(principal) => principal.context(),
        None => Context::anonymous().with_request_id(request_id::current()),
    };
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    admin::audit(
        &mut tx,
        &context,
        Some(workspace),
        "source.oauth_failed",
        Some(("source", source)),
        json!({"reason": reason}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_s256_challenge_matches_the_rfc_7636_example() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn the_redirect_uri_is_the_public_url_plus_the_one_callback_path() {
        assert_eq!(
            redirect_uri("https://flow.example.com/"),
            "https://flow.example.com/api/v1/oauth/callback"
        );
    }

    #[test]
    fn the_authorization_url_carries_the_standard_parameters_and_keeps_the_providers_own() {
        let consent = Consent {
            authorize_url: "https://auth.example.com/authorize?tenant=7"
                .parse()
                .unwrap(),
            scopes: vec!["contacts.read".into(), "offline_access".into()],
        };

        let url = authorization_url(
            &consent,
            "the-client",
            "https://flow.example.com/api/v1/oauth/callback",
            "the-state",
            "the-verifier-0123456789-0123456789-0123456789",
        );

        let url = Url::parse(&url).unwrap();
        assert_eq!(url.host_str(), Some("auth.example.com"));
        assert_eq!(url.path(), "/authorize");
        let query: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["tenant"], "7");
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["client_id"], "the-client");
        assert_eq!(
            query["redirect_uri"],
            "https://flow.example.com/api/v1/oauth/callback"
        );
        assert_eq!(query["scope"], "contacts.read offline_access");
        assert_eq!(query["state"], "the-state");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(
            query["code_challenge"],
            challenge("the-verifier-0123456789-0123456789-0123456789")
        );
        assert!(!query.contains_key("code_verifier"));
    }

    #[test]
    fn a_landing_is_the_source_page_or_the_front_page_when_no_state_was_found() {
        let ws = WorkspaceId::new();
        let source = Uuid::nil();

        assert_eq!(
            landing("https://f.example/", Some((ws, source)), "connected"),
            format!(
                "https://f.example/w/{}/sources/{source}?oauth=connected",
                ws.as_uuid()
            )
        );
        assert_eq!(
            landing("https://f.example", None, "failed&reason=unknown_state"),
            "https://f.example/?oauth=failed&reason=unknown_state"
        );
        assert_eq!(landing("", None, "connected"), "/?oauth=connected");
    }

    #[test]
    fn the_callback_query_takes_the_first_of_a_repeated_parameter() {
        let parsed = Callback::parse("code=a&state=s1&state=s2&error=denied&other=1");

        assert_eq!(parsed.code.as_deref(), Some("a"));
        assert_eq!(parsed.state.as_deref(), Some("s1"));
        assert_eq!(parsed.error.as_deref(), Some("denied"));
    }
}
