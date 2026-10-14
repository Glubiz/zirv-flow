use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zirv_flow_connectors::runtime::RateLimits;
use zirv_flow_core::egress::EgressPolicy;
use zirv_flow_core::{HmacKey, HmacPurpose, HmacRoot, Keyring};
use zirv_flow_identity::{IdentityError, OidcSettings, Provider};
use zirv_flow_store::{EventRetention, PgPool, SchemaState, StoreError};

use crate::assistant::{AiConfig, runtime::AiRuntime};
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
    /// The master keys that open stored secrets; absent until given.
    keyring: Option<Arc<Keyring>>,
    /// The explorer's time and row limits.
    pub(crate) explorer: ExplorerLimits,
    /// How long a segment preview may run before PostgreSQL cancels it (architecture 8.3).
    pub(crate) segment_preview_timeout: Duration,
    /// The shared listener and the viewers of the event debugger.
    pub(crate) debugger: DebuggerHub,
    /// Fires when the server is shutting down, so long-lived streams end and do not hold the
    /// graceful shutdown up. Absent when nothing tells the state about shutdown.
    pub(crate) stop: Option<tokio::sync::watch::Receiver<bool>>,
    /// Which non-public destinations outbound connections may reach.
    egress: Arc<EgressPolicy>,
    /// The request-rate buckets of this process's sources, shared by every call that reaches
    /// them. Per process: see `zirv_flow_connectors::runtime::RateLimits`.
    rate_limits: Arc<RateLimits>,
    /// The deployment's model allowlist and the guarded client for model calls.
    pub(crate) ai: AiRuntime,
    /// Where browsers reach the server (`server.public_url`), without a trailing slash. Absent
    /// when the operator did not set it; an OAuth2 consent needs it for the redirect URI.
    public_url: Option<String>,
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
            keyring: None,
            explorer: ExplorerLimits::default(),
            segment_preview_timeout: crate::segments::PREVIEW_TIMEOUT,
            stop: None,
            egress: Arc::default(),
            rate_limits: Arc::default(),
            ai: AiRuntime::default(),
            public_url: None,
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

    /// Uses `limits` for the sources' request rates, so the worker pool of the same process draws
    /// from the buckets the API does.
    pub fn with_rate_limits(mut self, limits: Arc<RateLimits>) -> Self {
        self.rate_limits = limits;
        self
    }

    /// The address browsers reach the server at, for the redirect URI of an OAuth2 consent
    /// (`<public_url>/api/v1/oauth/callback`). Without it no consent can start.
    pub fn with_public_url(mut self, public_url: Option<String>) -> Self {
        self.public_url = public_url.map(|url| url.trim_end_matches('/').to_owned());
        self
    }

    pub(crate) fn public_url(&self) -> Option<&str> {
        self.public_url.as_deref()
    }

    /// The request-rate buckets of this process's sources.
    pub fn rate_limits(&self) -> &Arc<RateLimits> {
        &self.rate_limits
    }

    /// The egress policy outbound connections are checked against.
    pub fn egress_policy(&self) -> &Arc<EgressPolicy> {
        &self.egress
    }

    /// The deployment's AI allowlist; with none the assistant is unavailable.
    pub fn with_ai_config(mut self, config: AiConfig) -> Self {
        self.ai = AiRuntime::new(config);
        self
    }

    /// The deployment's AI allowlist.
    pub fn ai_config(&self) -> &AiConfig {
        &self.ai.config
    }

    /// The explorer's query timeouts and row limits.
    pub fn with_explorer_limits(mut self, limits: ExplorerLimits) -> Self {
        self.explorer = limits;
        self
    }

    /// How long a segment preview may run; 10 seconds by default.
    pub fn with_segment_preview_timeout(mut self, timeout: Duration) -> Self {
        self.segment_preview_timeout = timeout;
        self
    }

    /// Ends the debugger streams when `stop` becomes true. [`serve`](crate::serve) does this
    /// from its shutdown future.
    pub(crate) fn with_stop(mut self, stop: tokio::sync::watch::Receiver<bool>) -> Self {
        self.stop = Some(stop);
        self
    }

    /// Derives the identifier-hashing key from the deployment's HMAC root. Without it the
    /// suppression endpoints answer 503.
    pub fn with_hmac_root(mut self, root: &HmacRoot) -> Self {
        self.identifier_key = Some(Arc::new(root.hmac_key(HmacPurpose::Identifier)));
        self
    }

    /// The master keys that open stored secrets.
    pub fn with_keyring(mut self, keyring: Arc<Keyring>) -> Self {
        self.keyring = Some(keyring);
        self
    }

    /// The master keys that open stored secrets, when they were given.
    pub fn keyring(&self) -> Option<&Arc<Keyring>> {
        self.keyring.as_ref()
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
    use zirv_flow_core::MasterKey;

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
    async fn the_public_url_has_no_trailing_slash_and_is_absent_until_set() {
        assert_eq!(state().public_url(), None);
        let set = state().with_public_url(Some("https://flow.example.com/".to_owned()));
        assert_eq!(set.public_url(), Some("https://flow.example.com"));
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
    async fn clones_share_the_rate_limit_buckets_and_they_can_be_replaced() {
        let state = state();

        assert!(Arc::ptr_eq(
            state.rate_limits(),
            state.clone().rate_limits()
        ));
        let shared = Arc::new(RateLimits::new());
        let set = state.with_rate_limits(Arc::clone(&shared));
        assert!(Arc::ptr_eq(set.rate_limits(), &shared));
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
    async fn the_identifier_key_comes_from_the_hmac_root_and_is_off_until_given() {
        let root = HmacRoot::bootstrap_from(&MasterKey::generate());

        assert!(state().identifier_key.is_none());
        let with = state().with_hmac_root(&root);
        let derived = root.hmac_key(HmacPurpose::Identifier);
        assert_eq!(
            with.identifier_key.unwrap().digest(b"x"),
            derived.digest(b"x")
        );
    }

    #[tokio::test]
    async fn the_ai_allowlist_is_empty_until_given() {
        let config = AiConfig {
            max_repair_attempts: 4,
            ..AiConfig::default()
        };

        assert!(state().ai_config().providers.is_empty());
        assert_eq!(
            state()
                .with_ai_config(config)
                .ai_config()
                .max_repair_attempts,
            4
        );
    }

    #[tokio::test]
    async fn the_keyring_is_off_until_given() {
        let keyring = Arc::new(Keyring::new(MasterKey::generate(), None).unwrap());

        assert!(state().keyring().is_none());
        assert!(state().with_keyring(keyring).keyring().is_some());
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
