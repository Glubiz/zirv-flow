use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use zirv_flow_core::egress::EgressPolicy;
use zirv_flow_core::{HmacKey, HmacPurpose, MasterKey};
use zirv_flow_identity::{IdentityError, OidcSettings, Provider};
use zirv_flow_store::{EventRetention, PgPool, SchemaState, StoreError};

use crate::events::ExplorerLimits;
use crate::events::debugger::DebuggerHub;
use crate::imports::ImportLimits;
use crate::ingest::{IngestLimits, IngestState};
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
    /// The bounds a per-name event retention override must respect.
    pub(crate) event_retention: EventRetention,
    /// The ingestion limits and the per-key rate-limit buckets.
    pub(crate) ingest: IngestState,
    /// The largest CSV file an upload takes.
    pub(crate) imports: ImportLimits,
    /// The SHA-256 of the first-run setup token while one is valid; never the token itself.
    pub(crate) setup_token: Arc<Mutex<Option<[u8; 32]>>>,
    /// Hashes identifiers for suppressions; absent until the master key is given.
    pub(crate) identifier_key: Option<Arc<HmacKey>>,
    /// The explorer's time and row limits.
    pub(crate) explorer: ExplorerLimits,
    /// The shared listener and the viewers of the event debugger.
    pub(crate) debugger: DebuggerHub,
    /// Fires when the server is shutting down, so long-lived streams end and do not hold the
    /// graceful shutdown up. Absent when nothing tells the state about shutdown.
    pub(crate) stop: Option<tokio::sync::watch::Receiver<bool>>,
    /// Which non-public destinations outbound connections may reach.
    egress: Arc<EgressPolicy>,
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
            debugger: DebuggerHub::new(pool.clone()),
            pool,
            render_metrics,
            schema_check: Arc::new(|pool| {
                Box::pin(async move { zirv_flow_store::schema_state(&pool).await })
            }),
            cookie_secure: true,
            local_sign_in: true,
            oidc: None,
            event_retention: EventRetention::default(),
            ingest: IngestState::new(IngestLimits::default()),
            imports: ImportLimits::default(),
            setup_token: Arc::default(),
            identifier_key: None,
            explorer: ExplorerLimits::default(),
            stop: None,
            egress: Arc::default(),
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

    /// The operator's event retention settings; their bounds limit per-name overrides.
    pub fn with_event_retention(mut self, settings: EventRetention) -> Self {
        self.event_retention = settings;
        self
    }

    /// The egress policy outbound connections are checked against; the default allows public
    /// addresses only.
    pub fn with_egress_policy(mut self, policy: Arc<EgressPolicy>) -> Self {
        self.egress = policy;
        self
    }

    /// The egress policy outbound connections are checked against.
    pub fn egress_policy(&self) -> &Arc<EgressPolicy> {
        &self.egress
    }

    /// The explorer's query timeouts and row limits.
    pub fn with_explorer_limits(mut self, limits: ExplorerLimits) -> Self {
        self.explorer = limits;
        self
    }

    /// Ends the debugger streams when `stop` becomes true. [`serve`](crate::serve) does this
    /// from its shutdown future.
    pub(crate) fn with_stop(mut self, stop: tokio::sync::watch::Receiver<bool>) -> Self {
        self.stop = Some(stop);
        self
    }

    /// Derives the identifier-hashing key from the operator's master key. Without it the
    /// suppression endpoints answer 503.
    pub fn with_master_key(mut self, key: &MasterKey) -> Self {
        self.identifier_key = Some(Arc::new(key.hmac_key(HmacPurpose::Identifier)));
        self
    }

    /// The ingestion limits: body size, batch size and the per-key rate, per process.
    pub fn with_ingest_limits(mut self, limits: IngestLimits) -> Self {
        self.ingest = IngestState::new(limits);
        self
    }

    /// The limits on CSV uploads.
    pub fn with_import_limits(mut self, limits: ImportLimits) -> Self {
        self.imports = limits;
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
    async fn the_egress_policy_denies_non_public_addresses_until_one_is_set() {
        let ip = "10.0.0.1".parse().unwrap();
        let allow = Arc::new(EgressPolicy::parse(&["10.0.0.0/8".to_owned()]).unwrap());

        assert!(state().egress_policy().decide(ip) != zirv_flow_core::egress::Decision::Allow);
        let set = state().with_egress_policy(allow);
        assert_eq!(
            set.egress_policy().decide(ip),
            zirv_flow_core::egress::Decision::Allow
        );
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
    async fn event_retention_bounds_default_and_can_be_set() {
        let settings = EventRetention {
            min_days: 5,
            ..EventRetention::default()
        };

        assert_eq!(state().event_retention, EventRetention::default());
        assert_eq!(
            state()
                .with_event_retention(settings)
                .event_retention
                .min_days,
            5
        );
    }

    #[tokio::test]
    async fn the_identifier_key_comes_from_the_master_key_and_is_off_until_given() {
        let key = MasterKey::generate();

        assert!(state().identifier_key.is_none());
        let with = state().with_master_key(&key);
        let derived = key.hmac_key(HmacPurpose::Identifier);
        assert_eq!(
            with.identifier_key.unwrap().digest(b"x"),
            derived.digest(b"x")
        );
    }

    #[tokio::test]
    async fn the_explorer_limits_default_and_can_be_set() {
        let limits = ExplorerLimits {
            export_row_limit: 5,
            ..ExplorerLimits::default()
        };

        assert_eq!(state().explorer, ExplorerLimits::default());
        assert_eq!(
            state()
                .with_explorer_limits(limits)
                .explorer
                .export_row_limit,
            5
        );
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
