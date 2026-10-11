//! An in-process identity provider for tests: discovery, the key set and a token endpoint
//! that signs ID tokens with a test key. Not for production use.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use openidconnect::core::{
    CoreIdToken, CoreIdTokenClaims, CoreIdTokenFields, CoreJsonWebKeySet, CoreJwsSigningAlgorithm,
    CoreRsaPrivateSigningKey, CoreTokenResponse, CoreTokenType,
};
use openidconnect::{
    AccessToken, Audience, EmptyAdditionalClaims, EmptyExtraTokenFields, EndUserEmail, IssuerUrl,
    JsonWebKeyId, Nonce, PrivateSigningKey, StandardClaims, SubjectIdentifier,
};
use rsa::pkcs1::EncodeRsaPrivateKey;
use serde_json::json;

/// The client id the mock puts in the audience of a valid token.
pub const CLIENT_ID: &str = "zirv-flow";

/// What the token endpoint puts in the next ID tokens it issues.
#[derive(Clone, Debug)]
pub struct Token {
    pub audience: String,
    pub nonce: String,
    pub expires_in_seconds: i64,
    /// Sign with a key the key set does not contain.
    pub signed_with_unknown_key: bool,
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: Option<bool>,
}

impl Token {
    /// A token that verifies, for `nonce`.
    pub fn valid(nonce: &str) -> Self {
        Self {
            audience: CLIENT_ID.into(),
            nonce: nonce.into(),
            expires_in_seconds: 300,
            signed_with_unknown_key: false,
            subject: "subject-1".into(),
            email: Some("jane@example.com".into()),
            email_verified: Some(true),
        }
    }
}

#[derive(Clone)]
struct Shared {
    issuer: String,
    discoveries: Arc<AtomicUsize>,
    token: Arc<Mutex<Token>>,
    exchange_works: Arc<AtomicBool>,
}

/// A running mock provider. It stops when the test's runtime does.
pub struct MockProvider {
    shared: Shared,
}

impl MockProvider {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let shared = Shared {
            issuer: format!("http://{}", listener.local_addr().unwrap()),
            discoveries: Arc::new(AtomicUsize::new(0)),
            token: Arc::new(Mutex::new(Token::valid("unset"))),
            exchange_works: Arc::new(AtomicBool::new(true)),
        };
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/token", post(token))
            .with_state(shared.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { shared }
    }

    pub fn issuer(&self) -> &str {
        &self.shared.issuer
    }

    /// How many times discovery was fetched.
    pub fn discoveries(&self) -> usize {
        self.shared.discoveries.load(Ordering::SeqCst)
    }

    /// Replaces what the next ID tokens contain.
    pub fn set_token(&self, token: Token) {
        *self.shared.token.lock().unwrap() = token;
    }

    /// Makes the token endpoint refuse every code.
    pub fn refuse_codes(&self) {
        self.shared.exchange_works.store(false, Ordering::SeqCst);
    }
}

/// Generated once per test process: RSA key generation is slow in a debug build.
fn pem(slot: &'static OnceLock<String>) -> &'static str {
    slot.get_or_init(|| {
        let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
        key.to_pkcs1_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap()
            .to_string()
    })
}

fn signing_key(unknown: bool) -> CoreRsaPrivateSigningKey {
    static KNOWN: OnceLock<String> = OnceLock::new();
    static UNKNOWN: OnceLock<String> = OnceLock::new();
    let pem = if unknown { pem(&UNKNOWN) } else { pem(&KNOWN) };
    CoreRsaPrivateSigningKey::from_pem(pem, Some(JsonWebKeyId::new("test-key".into()))).unwrap()
}

async fn discovery(State(shared): State<Shared>) -> impl IntoResponse {
    shared.discoveries.fetch_add(1, Ordering::SeqCst);
    axum::Json(json!({
        "issuer": shared.issuer,
        "authorization_endpoint": format!("{}/auth", shared.issuer),
        "token_endpoint": format!("{}/token", shared.issuer),
        "jwks_uri": format!("{}/jwks", shared.issuer),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256", "none"],
    }))
}

async fn jwks() -> impl IntoResponse {
    let key = signing_key(false);
    axum::Json(CoreJsonWebKeySet::new(vec![key.as_verification_key()]))
}

async fn token(State(shared): State<Shared>) -> Response {
    if !shared.exchange_works.load(Ordering::SeqCst) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "invalid_grant"})),
        )
            .into_response();
    }
    let spec = shared.token.lock().unwrap().clone();
    let now = chrono::Utc::now();
    let mut standard = StandardClaims::new(SubjectIdentifier::new(spec.subject.clone()));
    if let Some(email) = &spec.email {
        standard = standard.set_email(Some(EndUserEmail::new(email.clone())));
    }
    let claims = CoreIdTokenClaims::new(
        IssuerUrl::new(shared.issuer.clone()).unwrap(),
        vec![Audience::new(spec.audience.clone())],
        now + chrono::Duration::seconds(spec.expires_in_seconds),
        now,
        standard.set_email_verified(spec.email_verified),
        EmptyAdditionalClaims {},
    )
    .set_nonce(Some(Nonce::new(spec.nonce.clone())));
    let access_token = AccessToken::new("the-access-token".into());
    let id_token = CoreIdToken::new(
        claims,
        &signing_key(spec.signed_with_unknown_key),
        CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
        Some(&access_token),
        None,
    )
    .unwrap();
    let response = CoreTokenResponse::new(
        access_token,
        CoreTokenType::Bearer,
        CoreIdTokenFields::new(Some(id_token), EmptyExtraTokenFields {}),
    );
    axum::Json(response).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_mock_serves_discovery_the_key_set_and_counts_discoveries() {
        let mock = MockProvider::start().await;
        let client = openidconnect::reqwest::Client::new();

        let metadata = client
            .get(format!(
                "{}/.well-known/openid-configuration",
                mock.issuer()
            ))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
        let keys = client
            .get(metadata["jwks_uri"].as_str().unwrap())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        let keys: serde_json::Value = serde_json::from_str(&keys).unwrap();
        assert_eq!(metadata["issuer"], mock.issuer());
        assert_eq!(keys["keys"][0]["kty"], "RSA");
        assert_eq!(mock.discoveries(), 1);
    }

    #[test]
    fn a_default_token_is_valid_for_the_given_nonce() {
        let token = Token::valid("n");

        assert_eq!(token.nonce, "n");
        assert_eq!(token.audience, CLIENT_ID);
        assert!(token.expires_in_seconds > 0);
        assert!(!token.signed_with_unknown_key);
    }
}
