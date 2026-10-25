//! The world the end-to-end send test (#232) runs in: one PostgreSQL database, the API router in
//! process, a real worker pool on a `ManualClock`, the render host, the local SMTP sink, the fake
//! email provider and the fixture DNS resolver.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tower::ServiceExt;
use uuid::Uuid;
use zirv_flow_api::AppState;
use zirv_flow_api::admin::{self, Context};
use zirv_flow_connectors::destinations::{
    FakeProvider, FakeProviderOptions, SMTP_SINK_PASSWORD, SMTP_SINK_USER, SmtpSink,
    SmtpSinkOptions,
};
use zirv_flow_connectors::dns::FixtureResolver;
use zirv_flow_core::{
    HmacPurpose, HmacRoot, Keyring, MasterKey, Scope, Slug, WorkspaceId, WorkspaceName,
    deployment_key_aad,
};
use zirv_flow_jobs::{Registry, Worker, WorkerConfig};
use zirv_flow_render::{PoolConfig, RenderPool};
use zirv_flow_store::{Clock, ManualClock, secrets};

/// Where browsers reach the server in this world: tracking links are built on it.
pub(super) const PUBLIC_URL: &str = "https://flow.example.test";

/// The provider's signing secret and API token. They only ever appear inside the sealed
/// credential, so the privacy scan can look for them.
pub(super) const PROVIDER_TOKEN: &str = "zz-e2e-provider-token-0815";
pub(super) const PROVIDER_SECRET: &str = "zz-e2e-feedback-secret-4711";

/// The provider manifest the fake provider stands in for.
const PROVIDER_MANIFEST: &str =
    include_str!("../../../zirv-flow-connectors/tests/fixtures/providers/bearer_timestamped.json");

/// What one API call answered.
pub(super) struct Reply {
    pub(super) status: StatusCode,
    pub(super) headers: HeaderMap,
    pub(super) body: Value,
    pub(super) text: String,
}

/// One request through the router, with an API key when `key` is given.
pub(super) async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    key: Option<&str>,
) -> Reply {
    let (content_type, body) = match body {
        Some(body) => (Some("application/json"), Body::from(body.to_string())),
        None => (None, Body::empty()),
    };
    send(router, method, uri, content_type, key, body).await
}

/// One request with a form body, as a browser or a mail client posts it.
pub(super) async fn call_form(router: &Router, method: &str, uri: &str, form: &str) -> Reply {
    send(
        router,
        method,
        uri,
        Some("application/x-www-form-urlencoded"),
        None,
        Body::from(form.to_owned()),
    )
    .await
}

async fn send(
    router: &Router,
    method: &str,
    uri: &str,
    content_type: Option<&str>,
    key: Option<&str>,
    body: Body,
) -> Reply {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(content_type) = content_type {
        request = request.header(header::CONTENT_TYPE, content_type);
    }
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(body).expect("a well-formed request"))
        .await
        .expect("the router answers");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("the body is read")
        .to_bytes();
    Reply {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

pub(super) struct World {
    pub(super) pool: PgPool,
    pub(super) router: Router,
    pub(super) ws: WorkspaceId,
    /// A key with every scope but ingest.
    pub(super) admin: String,
    /// A write-only key for the ingest endpoints.
    pub(super) ingest: String,
    pub(super) clock: ManualClock,
    pub(super) sink: SmtpSink,
    pub(super) provider: FakeProvider,
    pub(super) dns: FixtureResolver,
    render: RenderPool,
    stop: watch::Sender<bool>,
    worker: Option<JoinHandle<()>>,
}

impl World {
    /// Builds the database content every scenario starts from, and starts the worker.
    pub(super) async fn start(pool: PgPool) -> Self {
        zirv_flow_store::migrate(&pool).await.expect("migrations");
        let master = MasterKey::parse("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=").expect("key");
        let keyring = Arc::new(Keyring::new(master, None).expect("a keyring"));
        let root = HmacRoot::bootstrap_from(keyring.current());
        let sealed = zirv_flow_store::seal_secret(
            &keyring,
            root.to_secret_bytes().expose(),
            &deployment_key_aad(secrets::HMAC_ROOT),
        );
        secrets::insert_hmac_root_if_absent(&pool, &sealed)
            .await
            .expect("the HMAC root is stored");
        let ws = zirv_flow_store::create_workspace(
            &pool,
            &WorkspaceName::parse("Acme").expect("a name"),
            &Slug::parse("acme").expect("a slug"),
        )
        .await
        .expect("a workspace")
        .id;

        let clock = ManualClock::new(OffsetDateTime::now_utc());
        let render = RenderPool::start(
            crate::render_host_command(std::env::current_exe().expect("this executable"), 0),
            PoolConfig {
                size: 2,
                deadline: Duration::from_secs(60),
            },
        )
        .await
        .expect("the render host starts");
        let loopback = Arc::new(
            zirv_flow_api::EgressPolicy::parse(&["127.0.0.1/32".to_owned()])
                .expect("a loopback policy"),
        );
        let state = AppState::new(pool.clone(), Arc::new(String::new))
            .with_hmac_root(&root)
            .with_keyring(Arc::clone(&keyring))
            .with_public_url(Some(PUBLIC_URL.to_owned()))
            .with_clock(Clock::Manual(clock.clone()))
            .with_render_pool(render.clone())
            .with_egress_policy(Arc::clone(&loopback));
        let router = zirv_flow_api::router(state);

        let key = |name: &'static str, scopes: &'static [Scope]| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), ws, name, scopes)
                    .await
                    .expect("an API key")
                    .plaintext
            }
        };
        let admin_key = key("e2e", &[Scope::Admin, Scope::Write, Scope::Read]).await;
        let ingest = key("e2e ingest", &[Scope::Ingest]).await;

        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let provider = FakeProvider::start(FakeProviderOptions::default()).await;
        let dns = FixtureResolver::new();

        let mut config = WorkerConfig::new(1);
        config.poll_interval = Duration::from_millis(50);
        config.clock = Clock::Manual(clock.clone());
        config.render_pool = Some(render.clone());
        config.dns = Some(Arc::new(dns.clone()));
        config.public_url = Some(PUBLIC_URL.to_owned());
        config.keyring = Some(keyring);
        config.egress = loopback;
        config.redaction_key = Some(Arc::new(root.hmac_key(HmacPurpose::Redaction)));
        config.identifier_key = Some(Arc::new(root.hmac_key(HmacPurpose::Identifier)));
        config.tracking_key = Some(Arc::new(root.hmac_key(HmacPurpose::Tracking)));
        let (stop, stopped) = watch::channel(false);
        let worker =
            tokio::spawn(Worker::new(pool.clone(), Registry::built_in(), config).run(stopped));

        Self {
            pool,
            router,
            ws,
            admin: admin_key,
            ingest,
            clock,
            sink,
            provider,
            dns,
            render,
            stop,
            worker: Some(worker),
        }
    }

    /// Stops the worker and the render host.
    pub(super) async fn finish(mut self) {
        let _ = self.stop.send(true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
        self.render.shutdown().await;
    }

    pub(super) fn path(&self, tail: &str) -> String {
        format!("/api/v1/workspaces/{}{tail}", self.ws.as_uuid())
    }

    /// A call as the workspace admin key.
    pub(super) async fn api(&self, method: &str, tail: &str, body: Option<Value>) -> Reply {
        call(
            &self.router,
            method,
            &self.path(tail),
            body,
            Some(&self.admin),
        )
        .await
    }

    /// As [`Self::api`], and the answer must be `expected`.
    pub(super) async fn api_ok(
        &self,
        method: &str,
        tail: &str,
        body: Option<Value>,
        expected: StatusCode,
    ) -> Value {
        let reply = self.api(method, tail, body).await;
        assert_eq!(reply.status, expected, "{method} {tail}: {}", reply.text);
        reply.body
    }

    /// Waits until the worker has nothing left to do at this instant: no job is running and none
    /// is ready and due. A job that died fails the test with the reason it recorded.
    pub(super) async fn settle(&self) {
        let mut quiet = 0;
        for _ in 0..1200 {
            let dead: Vec<(String, Option<String>)> =
                sqlx::query_as("SELECT kind, last_error FROM jobs WHERE state = 'dead'")
                    .fetch_all(&self.pool)
                    .await
                    .expect("dead jobs");
            assert!(dead.is_empty(), "dead jobs: {dead:?}");
            let busy: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM jobs \
                 WHERE state = 'running' OR (state = 'ready' AND run_at <= now() + interval '3 seconds')",
            )
            .fetch_one(&self.pool)
            .await
            .expect("busy jobs");
            quiet = if busy == 0 { quiet + 1 } else { 0 };
            if quiet >= 4 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let waiting: Vec<(String, String)> =
            sqlx::query_as("SELECT kind, state FROM jobs ORDER BY created_at")
                .fetch_all(&self.pool)
                .await
                .expect("jobs");
        panic!("the worker never went quiet: {waiting:?}");
    }

    /// One row count.
    pub(super) async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(&self.pool)
            .await
            .expect("a count")
    }

    // --- setup through the API ---

    /// A sending domain whose records are published in the fixture DNS and verified by the worker,
    /// and a sender identity on it. Returns the identity id and the DKIM record the domain
    /// publishes.
    pub(super) async fn verified_sender(&self, domain: &str, address: &str) -> (String, String) {
        let created = self
            .api_ok(
                "POST",
                "/sending-domains",
                Some(json!({"domain": domain})),
                StatusCode::CREATED,
            )
            .await;
        let id = created["id"].as_str().expect("a domain id").to_owned();
        assert_eq!(created["status"], "unverified");
        let mut dkim = String::new();
        for record in created["records"].as_array().expect("records") {
            let host = record["host"].as_str().expect("a host");
            let text = record["expected_text"].as_str().expect("a text");
            if record["kind"] == "dkim" {
                dkim = text.to_owned();
            }
            self.dns.set(host, &[text]);
        }
        // The check queued at creation ran before the records were published; ask for another.
        self.settle().await;
        self.api_ok(
            "POST",
            &format!("/sending-domains/{id}/verify"),
            None,
            StatusCode::ACCEPTED,
        )
        .await;
        self.settle().await;
        let domain = self
            .api_ok(
                "GET",
                &format!("/sending-domains/{id}"),
                None,
                StatusCode::OK,
            )
            .await;
        assert_eq!(domain["status"], "verified", "{domain}");
        let identity = self
            .api_ok(
                "POST",
                "/sender-identities",
                Some(json!({"domain_id": id, "from_name": "News", "from_address": address})),
                StatusCode::CREATED,
            )
            .await;
        (
            identity["id"].as_str().expect("an identity id").to_owned(),
            dkim,
        )
    }

    /// An SMTP destination on the local sink, with its `smtp` workspace credential.
    pub(super) async fn smtp_destination(&self) -> String {
        let credential = self
            .api_ok(
                "POST",
                "/credentials",
                Some(json!({"kind": "smtp", "name": "Relay",
                            "fields": {"host": "127.0.0.1", "port": self.sink.port(),
                                       "tls": "none", "username": SMTP_SINK_USER},
                            "secret": {"password": SMTP_SINK_PASSWORD}})),
                StatusCode::CREATED,
            )
            .await;
        let destination = self
            .api_ok(
                "POST",
                "/destinations",
                Some(json!({"kind": "smtp", "name": "Relay", "config": {},
                            "credential_id": credential["id"], "rate": 100, "burst": 100})),
                StatusCode::CREATED,
            )
            .await;
        destination["id"]
            .as_str()
            .expect("a destination id")
            .to_owned()
    }

    /// The manifest of the provider destination, pointed at the fake provider.
    pub(super) fn provider_config(&self) -> Value {
        let text = PROVIDER_MANIFEST.replace(
            "https://api.fixture-provider.example.test",
            &self.provider.base_url(),
        );
        json!({"manifest": serde_json::from_str::<Value>(&text).expect("a manifest")})
    }

    /// A provider destination on the fake provider, with a credential holding the API token and
    /// the signing secret of the feedback webhook.
    pub(super) async fn provider_destination(&self) -> String {
        let fields: Vec<Value> = ["token", "feedback_secret"]
            .iter()
            .map(|key| {
                json!({"key": key, "label": key, "type": "string",
                       "required": true, "secret": true})
            })
            .collect();
        let credential = self
            .api_ok(
                "POST",
                "/credentials",
                Some(json!({"kind": "custom", "name": "Provider",
                            "schema": {"fields": fields},
                            "secret": {"token": PROVIDER_TOKEN,
                                       "feedback_secret": PROVIDER_SECRET}})),
                StatusCode::CREATED,
            )
            .await;
        let destination = self
            .api_ok(
                "POST",
                "/destinations",
                Some(json!({"kind": "provider", "name": "Provider",
                            "config": self.provider_config(),
                            "credential_id": credential["id"], "rate": 100, "burst": 100})),
                StatusCode::CREATED,
            )
            .await;
        destination["id"]
            .as_str()
            .expect("a destination id")
            .to_owned()
    }

    /// Creates and publishes an email template; returns its id.
    pub(super) async fn template(&self, name: &str, document: Value) -> String {
        let created = self
            .api_ok(
                "POST",
                "/templates",
                Some(json!({"name": name, "channel": "email", "document": document})),
                StatusCode::CREATED,
            )
            .await;
        let id = created["id"].as_str().expect("a template id").to_owned();
        self.api_ok(
            "POST",
            &format!("/templates/{id}/publish"),
            None,
            StatusCode::CREATED,
        )
        .await;
        id
    }

    /// Creates a segment; returns its id.
    pub(super) async fn segment(&self, name: &str, definition: Value) -> String {
        let created = self
            .api_ok(
                "POST",
                "/segments",
                Some(json!({"name": name, "definition": definition})),
                StatusCode::CREATED,
            )
            .await;
        created["id"].as_str().expect("a segment id").to_owned()
    }

    /// Creates and publishes a flow; returns its id.
    pub(super) async fn flow(&self, name: &str, graph: Value, settings: Value) -> String {
        let created = self
            .api_ok(
                "POST",
                "/flows",
                Some(json!({"name": name, "graph": graph, "settings": settings})),
                StatusCode::CREATED,
            )
            .await;
        let id = created["id"].as_str().expect("a flow id").to_owned();
        self.api_ok(
            "POST",
            &format!("/flows/{id}/publish"),
            None,
            StatusCode::CREATED,
        )
        .await;
        id
    }

    /// The only person of the workspace.
    pub(super) async fn the_person(&self) -> Uuid {
        sqlx::query_scalar("SELECT id FROM people")
            .fetch_one(&self.pool)
            .await
            .expect("exactly one person")
    }
}

impl World {
    /// Publishes an event name that carries no properties, so segments can name it.
    pub(super) async fn event_schema(&self, name: &str) {
        self.api_ok(
            "POST",
            "/event-schemas",
            Some(json!({"name": name, "mode": "allow"})),
            StatusCode::CREATED,
        )
        .await;
        self.api_ok(
            "POST",
            &format!("/event-schemas/{name}/versions"),
            None,
            StatusCode::CREATED,
        )
        .await;
    }
}
