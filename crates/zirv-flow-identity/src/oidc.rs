//! OpenID Connect authorization-code flow with PKCE, for any compliant provider.
//!
//! Discovery is lazy and cached, so constructing a [`Provider`] never touches the network
//! and the server starts even while the provider is down.

use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use openidconnect::core::{
    CoreAuthenticationFlow, CoreClient, CoreJwsSigningAlgorithm, CoreProviderMetadata,
};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, IssuerUrl, Nonce, OAuth2TokenResponse, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, reqwest,
};

use crate::IdentityError;

/// How long discovered metadata and the key set are reused.
const METADATA_TTL: Duration = Duration::from_secs(15 * 60);
/// The longest any one call to the provider may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// A secret that never prints.
#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("***")
    }
}

/// How to reach and identify ourselves to the provider.
#[derive(Clone, Debug)]
pub struct OidcSettings {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: SecretString,
    /// Requested scopes; `openid` is always requested, listed or not.
    pub scopes: Vec<String>,
    /// Where the provider sends the browser back to; must be registered at the provider.
    pub redirect_url: String,
}

/// A started login. `state`, `nonce` and `pkce_verifier` are kept on the server side of the
/// login; only `url` goes to the browser, and `state` rides along as a cookie.
pub struct Authorization {
    pub url: String,
    pub state: String,
    pub nonce: String,
    pub pkce_verifier: String,
}

impl fmt::Debug for Authorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Authorization")
            .finish_non_exhaustive()
    }
}

/// Who the provider says the user is, after the ID token verified.
#[derive(Clone, Eq, PartialEq)]
pub struct VerifiedIdentity {
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
}

// The subject and the address identify a person: neither is printed.
impl fmt::Debug for VerifiedIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedIdentity")
            .finish_non_exhaustive()
    }
}

/// One configured identity provider.
pub struct Provider {
    settings: OidcSettings,
    issuer: IssuerUrl,
    redirect: RedirectUrl,
    http: reqwest::Client,
    cache: Mutex<Option<(Instant, CoreProviderMetadata)>>,
}

impl fmt::Debug for Provider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Provider")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl Provider {
    /// Validates the settings and builds the HTTP client. Does no network I/O.
    pub fn new(settings: OidcSettings) -> Result<Self, IdentityError> {
        let issuer = IssuerUrl::new(settings.issuer_url.clone())
            .map_err(|_| IdentityError::Settings("the issuer URL is not a URL"))?;
        let redirect = RedirectUrl::new(settings.redirect_url.clone())
            .map_err(|_| IdentityError::Settings("the redirect URL is not a URL"))?;
        let http = reqwest::Client::builder()
            // Following redirects would let the provider's responses steer us at other hosts.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| IdentityError::Settings("the HTTP client could not be built"))?;
        Ok(Self {
            settings,
            issuer,
            redirect,
            http,
            cache: Mutex::new(None),
        })
    }

    /// Starts a login: the provider URL to send the browser to, and the values to keep until
    /// the browser comes back.
    pub async fn begin(&self) -> Result<Authorization, IdentityError> {
        let client = self.client().await?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut request = client.authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        );
        for scope in self
            .settings
            .scopes
            .iter()
            .filter(|scope| *scope != "openid")
        {
            request = request.add_scope(Scope::new(scope.clone()));
        }
        let (url, state, nonce) = request.set_pkce_challenge(challenge).url();
        Ok(Authorization {
            url: url.to_string(),
            state: state.into_secret(),
            nonce: nonce.secret().clone(),
            pkce_verifier: verifier.into_secret(),
        })
    }

    /// Exchanges the authorization `code` and verifies the ID token: signature against the
    /// provider's keys, issuer, audience, expiry, `nonce` and the access-token hash.
    pub async fn finish(
        &self,
        code: &str,
        pkce_verifier: &str,
        nonce: &str,
    ) -> Result<VerifiedIdentity, IdentityError> {
        let (client, verifier_algs) = self.client_and_algorithms().await?;
        let response = client
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .map_err(|_| IdentityError::Exchange)?
            .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier.to_owned()))
            .request_async(&self.http)
            .await
            .map_err(|_| IdentityError::Exchange)?;

        let id_token = response.id_token().ok_or(IdentityError::InvalidToken)?;
        let mut verifier = client.id_token_verifier();
        if !verifier_algs.is_empty() {
            verifier = verifier.set_allowed_algs(verifier_algs);
        }
        let claims = id_token
            .claims(&verifier, &Nonce::new(nonce.to_owned()))
            .map_err(|_| IdentityError::InvalidToken)?;

        if let Some(expected) = claims.access_token_hash() {
            let algorithm = id_token
                .signing_alg()
                .map_err(|_| IdentityError::InvalidToken)?;
            let key = id_token
                .signing_key(&verifier)
                .map_err(|_| IdentityError::InvalidToken)?;
            let actual = AccessTokenHash::from_token(response.access_token(), algorithm, key)
                .map_err(|_| IdentityError::InvalidToken)?;
            if actual != *expected {
                return Err(IdentityError::InvalidToken);
            }
        }

        Ok(VerifiedIdentity {
            issuer: claims.issuer().as_str().to_owned(),
            subject: claims.subject().as_str().to_owned(),
            email: claims.email().map(|email| email.as_str().to_owned()),
            email_verified: claims.email_verified().unwrap_or(false),
        })
    }

    async fn client(&self) -> Result<Client, IdentityError> {
        Ok(self.client_and_algorithms().await?.0)
    }

    /// The client built from the (cached) metadata, and the ID-token signing algorithms the
    /// provider advertises, without `none`.
    async fn client_and_algorithms(
        &self,
    ) -> Result<(Client, Vec<CoreJwsSigningAlgorithm>), IdentityError> {
        let metadata = self.metadata().await?;
        let algorithms = metadata
            .id_token_signing_alg_values_supported()
            .iter()
            .filter(|algorithm| **algorithm != CoreJwsSigningAlgorithm::None)
            .cloned()
            .collect();
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(self.settings.client_id.clone()),
            Some(ClientSecret::new(
                self.settings.client_secret.expose().to_owned(),
            )),
        )
        .set_redirect_uri(self.redirect.clone());
        Ok((client, algorithms))
    }

    /// Discovered metadata, including the key set. A failed discovery is not cached, so the
    /// next request tries again.
    async fn metadata(&self) -> Result<CoreProviderMetadata, IdentityError> {
        if let Some((fetched, metadata)) = &*self.lock()
            && fetched.elapsed() < METADATA_TTL
        {
            return Ok(metadata.clone());
        }
        let metadata = CoreProviderMetadata::discover_async(self.issuer.clone(), &self.http)
            .await
            .map_err(|_| IdentityError::Discovery)?;
        *self.lock() = Some((Instant::now(), metadata.clone()));
        Ok(metadata)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(Instant, CoreProviderMetadata)>> {
        // The guard is never held across an await, and a poisoned cache is still a cache.
        self.cache.lock().unwrap_or_else(|error| error.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::mock::{CLIENT_ID, MockProvider, Token};

    const CLIENT_SECRET: &str = "the-client-secret";
    const CODE: &str = "the-authorization-code";
    const NONCE: &str = "the-nonce";
    const VERIFIER: &str = "the-pkce-verifier";

    fn settings(issuer: &str) -> OidcSettings {
        OidcSettings {
            issuer_url: issuer.to_owned(),
            client_id: CLIENT_ID.into(),
            client_secret: SecretString::new(CLIENT_SECRET),
            scopes: vec!["openid".into(), "email".into(), "profile".into()],
            redirect_url: "http://localhost:8080/api/v1/session/oidc/callback".into(),
        }
    }

    async fn provider() -> (MockProvider, Provider) {
        let mock = MockProvider::start().await;
        mock.set_token(Token::valid(NONCE));
        let provider = Provider::new(settings(mock.issuer())).unwrap();
        (mock, provider)
    }

    async fn refused_with(change: impl FnOnce(&mut Token)) -> IdentityError {
        let (mock, provider) = provider().await;
        let mut token = Token::valid(NONCE);
        change(&mut token);
        mock.set_token(token);
        provider.finish(CODE, VERIFIER, NONCE).await.unwrap_err()
    }

    #[tokio::test]
    async fn a_valid_token_yields_the_verified_identity() {
        let (mock, provider) = provider().await;

        let identity = provider.finish(CODE, VERIFIER, NONCE).await.unwrap();

        assert_eq!(identity.issuer, mock.issuer());
        assert_eq!(identity.subject, "subject-1");
        assert_eq!(identity.email.as_deref(), Some("jane@example.com"));
        assert!(identity.email_verified);
    }

    #[tokio::test]
    async fn a_missing_email_verified_claim_counts_as_unverified() {
        let (mock, provider) = provider().await;
        let mut token = Token::valid(NONCE);
        token.email_verified = None;
        mock.set_token(token);

        let identity = provider.finish(CODE, VERIFIER, NONCE).await.unwrap();

        assert!(!identity.email_verified);
    }

    #[tokio::test]
    async fn a_token_for_another_nonce_is_refused() {
        let (_mock, provider) = provider().await;

        let error = provider
            .finish(CODE, VERIFIER, "a-different-nonce")
            .await
            .unwrap_err();

        assert_eq!(error, IdentityError::InvalidToken);
    }

    #[tokio::test]
    async fn a_token_signed_with_another_key_is_refused() {
        let error = refused_with(|token| token.signed_with_unknown_key = true).await;

        assert_eq!(error, IdentityError::InvalidToken);
    }

    #[tokio::test]
    async fn a_token_for_another_audience_is_refused() {
        let error = refused_with(|token| token.audience = "someone-else".into()).await;

        assert_eq!(error, IdentityError::InvalidToken);
    }

    #[tokio::test]
    async fn an_expired_token_is_refused() {
        let error = refused_with(|token| token.expires_in_seconds = -60).await;

        assert_eq!(error, IdentityError::InvalidToken);
    }

    #[tokio::test]
    async fn a_refused_code_exchange_is_an_exchange_error() {
        let (mock, provider) = provider().await;
        mock.refuse_codes();

        let error = provider.finish(CODE, VERIFIER, NONCE).await.unwrap_err();

        assert_eq!(error, IdentityError::Exchange);
    }

    #[tokio::test]
    async fn an_unreachable_provider_is_a_discovery_error_each_time() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let provider = Provider::new(settings(&format!("http://{address}"))).unwrap();

        let first = provider.begin().await.unwrap_err();
        let second = provider.finish(CODE, VERIFIER, NONCE).await.unwrap_err();

        assert_eq!(first, IdentityError::Discovery);
        assert_eq!(second, IdentityError::Discovery);
    }

    #[tokio::test]
    async fn constructing_a_provider_does_no_io_and_discovery_is_cached() {
        let (mock, provider) = provider().await;
        assert_eq!(mock.discoveries(), 0);

        provider.begin().await.unwrap();
        provider.begin().await.unwrap();

        assert_eq!(mock.discoveries(), 1);
    }

    #[tokio::test]
    async fn a_started_login_asks_for_the_code_flow_with_pkce_state_and_nonce() {
        let (_mock, provider) = provider().await;

        let authorization = provider.begin().await.unwrap();

        let url = url::Url::parse(&authorization.url).unwrap();
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.path(), "/auth");
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["client_id"], CLIENT_ID);
        assert_eq!(query["code_challenge_method"], "S256");
        assert!(!query["code_challenge"].is_empty());
        assert_eq!(query["state"], authorization.state);
        assert_eq!(query["nonce"], authorization.nonce);
        assert_eq!(query["scope"], "openid email profile");
        assert_eq!(
            query["redirect_uri"],
            "http://localhost:8080/api/v1/session/oidc/callback"
        );
        assert!(
            !query
                .values()
                .any(|value| *value == authorization.pkce_verifier)
        );
    }

    #[test]
    fn unusable_settings_are_refused_without_network_access() {
        let bad_issuer = settings("not a url");
        let mut bad_redirect = settings("https://idp.example.com");
        bad_redirect.redirect_url = "nope".into();

        assert!(matches!(
            Provider::new(bad_issuer),
            Err(IdentityError::Settings(_))
        ));
        assert!(matches!(
            Provider::new(bad_redirect),
            Err(IdentityError::Settings(_))
        ));
    }

    #[test]
    fn debug_output_never_shows_secrets_or_identities() {
        let provider = Provider::new(settings("https://idp.example.com")).unwrap();
        let authorization = Authorization {
            url: "https://idp/auth?state=s3cret".into(),
            state: "s3cret".into(),
            nonce: "n0nce".into(),
            pkce_verifier: "v3rifier".into(),
        };
        let identity = VerifiedIdentity {
            issuer: "https://idp".into(),
            subject: "subject-1".into(),
            email: Some("jane@example.com".into()),
            email_verified: true,
        };

        let debug = format!(
            "{provider:?} {authorization:?} {identity:?} {:?}",
            settings("https://x")
        );

        for secret in [
            CLIENT_SECRET,
            "s3cret",
            "n0nce",
            "v3rifier",
            "subject-1",
            "jane",
        ] {
            assert!(!debug.contains(secret), "{secret} leaked: {debug}");
        }
        assert!(debug.contains("***"));
    }
}
