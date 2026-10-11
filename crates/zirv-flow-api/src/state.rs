use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use zirv_flow_identity::{IdentityError, OidcSettings, Provider};
use zirv_flow_store::{PgPool, SchemaState, StoreError};

use crate::oidc;

pub(crate) type SchemaCheck = Arc<
    dyn Fn(PgPool) -> Pin<Box<dyn Future<Output = Result<SchemaState, StoreError>> + Send>>
        + Send
        + Sync,
>;

/// Shared state for every handler.
#[derive(Clone)]
pub struct AppState {
    pub(crate) pool: PgPool,
    pub(crate) render_metrics: Arc<dyn Fn() -> String + Send + Sync>,
    pub(crate) schema_check: SchemaCheck,
    pub(crate) cookie_secure: bool,
    pub(crate) local_sign_in: bool,
    pub(crate) oidc: Option<OidcState>,
    /// The SHA-256 of the first-run setup token while one is valid; never the token itself.
    pub(crate) setup_token: Arc<Mutex<Option<[u8; 32]>>>,
}

/// OIDC sign-in, when it is configured.
#[derive(Clone)]
pub(crate) struct OidcState {
    pub(crate) provider: Arc<Provider>,
    pub(crate) auto_create_users: bool,
    /// Where the browser lands after signing in: the public URL of the server.
    pub(crate) home_url: String,
}

impl AppState {
    /// `render_metrics` returns the Prometheus text exposition, so this crate stays
    /// independent of the exporter.
    pub fn new(pool: PgPool, render_metrics: Arc<dyn Fn() -> String + Send + Sync>) -> Self {
        Self {
            pool,
            render_metrics,
            schema_check: Arc::new(|pool| {
                Box::pin(async move { zirv_flow_store::schema_state(&pool).await })
            }),
            cookie_secure: true,
            local_sign_in: true,
            oidc: None,
            setup_token: Arc::default(),
        }
    }

    /// Whether the session cookie carries the `Secure` attribute. On by default; turn it
    /// off only for plain-HTTP development.
    pub fn with_cookie_secure(mut self, secure: bool) -> Self {
        self.cookie_secure = secure;
        self
    }

    /// Whether `POST /api/v1/session` (e-mail and password) is available. On by default.
    pub fn with_local_sign_in(mut self, enabled: bool) -> Self {
        self.local_sign_in = enabled;
        self
    }

    /// Turns on OIDC sign-in. Builds no connection: the provider is contacted on the first
    /// sign-in. `settings.redirect_url` must be [`crate::oidc_redirect_url`] of the public URL.
    pub fn with_oidc(
        mut self,
        settings: OidcSettings,
        auto_create_users: bool,
    ) -> Result<Self, IdentityError> {
        let home_url = oidc::home_url(&settings.redirect_url);
        self.oidc = Some(OidcState {
            provider: Arc::new(Provider::new(settings)?),
            auto_create_users,
            home_url,
        });
        Ok(self)
    }

    /// Replaces the readiness schema check; lets tests report states the embedded migrator cannot.
    #[cfg(test)]
    pub(crate) fn with_schema_check(mut self, check: SchemaCheck) -> Self {
        self.schema_check = check;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap();
        AppState::new(pool, Arc::new(String::new))
    }

    #[tokio::test]
    async fn cookies_are_secure_unless_turned_off() {
        assert!(state().cookie_secure);
        assert!(!state().with_cookie_secure(false).cookie_secure);
    }

    #[tokio::test]
    async fn there_is_no_setup_token_until_one_is_minted_and_clones_share_it() {
        let state = state();
        let clone = state.clone();

        assert!(state.setup_token().is_none());
        *clone.setup_token() = Some([7; 32]);

        assert_eq!(*state.setup_token(), Some([7; 32]));
    }

    #[tokio::test]
    async fn local_sign_in_is_on_and_oidc_is_off_by_default() {
        let state = state();

        assert!(state.local_sign_in);
        assert!(state.oidc.is_none());
        assert!(!state.with_local_sign_in(false).local_sign_in);
    }

    #[tokio::test]
    async fn oidc_settings_set_the_home_url_from_the_redirect_url() {
        let settings = OidcSettings {
            issuer_url: "https://idp.example.com/realms/x".into(),
            client_id: "zirv-flow".into(),
            client_secret: zirv_flow_identity::SecretString::new("s"),
            scopes: vec![],
            redirect_url: oidc::redirect_url("https://flow.example.com/"),
        };

        let state = state().with_oidc(settings, true).unwrap();

        let oidc = state.oidc.unwrap();
        assert_eq!(oidc.home_url, "https://flow.example.com/");
        assert!(oidc.auto_create_users);
    }
}
