//! The world the end-to-end send test (#232) runs in: one PostgreSQL database, the API router in
//! process, a real worker pool on a `ManualClock`, the render host, the local SMTP sink, the fake
//! email provider and the fixture DNS resolver.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
#[cfg(test)]
use axum::http::HeaderMap;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
#[cfg(test)]
use time::OffsetDateTime;
#[cfg(test)]
use time::format_description::well_known::Rfc3339;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tower::ServiceExt;
#[cfg(test)]
use uuid::Uuid;
use zirv_flow_api::AppState;
use zirv_flow_api::admin::{self, Context};
#[cfg(test)]
use zirv_flow_connectors::destinations::{FakeProvider, FakeProviderOptions};
use zirv_flow_jobs::test_support::{
    FixtureResolver, SMTP_SINK_PASSWORD, SMTP_SINK_USER, SmtpSink, SmtpSinkOptions,
};
use zirv_flow_jobs::{Registry, Worker, WorkerConfig};
use zirv_flow_render::{PoolConfig, RenderPool};
#[cfg(test)]
use zirv_flow_store::ManualClock;
use zirv_flow_store::{
    Clock, HmacPurpose, HmacRoot, Keyring, MasterKey, Scope, Slug, WorkspaceId, WorkspaceName,
    deployment_key_aad, secrets,
};

/// Where browsers reach the server in this world: tracking links are built on it.
#[cfg(test)]
pub(super) const PUBLIC_URL: &str = "https://flow.example.test";

/// The provider's signing secret and API token. They only ever appear inside the sealed
/// credential, so the privacy scan can look for them.
#[cfg(test)]
pub(super) const PROVIDER_TOKEN: &str = "zz-e2e-provider-token-0815";
#[cfg(test)]
pub(super) const PROVIDER_SECRET: &str = "zz-e2e-feedback-secret-4711";

/// The provider manifest the fake provider stands in for.
#[cfg(test)]
pub(super) const PROVIDER_MANIFEST: &str =
    include_str!("../../../zirv-flow-connectors/tests/fixtures/providers/bearer_timestamped.json");

/// What one API call answered.
pub(super) struct Reply {
    pub(super) status: StatusCode,
    #[cfg(test)]
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
#[cfg(test)]
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
    #[cfg(test)]
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("the body is read")
        .to_bytes();
    Reply {
        status,
        #[cfg(test)]
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
    #[cfg(test)]
    pub(super) ingest: String,
    #[cfg(test)]
    pub(super) clock: ManualClock,
    pub(super) sink: SmtpSink,
    #[cfg(test)]
    pub(super) provider: FakeProvider,
    pub(super) dns: FixtureResolver,
    /// The deployment's HMAC root, for tests that call the admin operations the CLI does.
    #[cfg(test)]
    pub(super) root: HmacRoot,
    render: RenderPool,
    stop: watch::Sender<bool>,
    worker: Option<JoinHandle<()>>,
}

impl World {
    /// Builds the database content every scenario starts from, and starts the worker. Links are
    /// built on `public_url`. Tests run on a clock they move; the browser test server on the
    /// database clock, which is the real one.
    pub(super) async fn start(pool: PgPool, public_url: &str) -> Self {
        Self::build(pool, public_url, None, false).await
    }

    /// As [`Self::start`], with the worker copying each completed erasure to `ledger_dir`.
    #[cfg(test)]
    pub(super) async fn start_with_ledger(
        pool: PgPool,
        public_url: &str,
        ledger_dir: &std::path::Path,
    ) -> Self {
        Self::build(pool, public_url, Some(ledger_dir), false).await
    }

    /// Starts a world on a database an earlier world filled: the workspace is found again, not
    /// created.
    #[cfg(test)]
    pub(super) async fn resume(
        pool: PgPool,
        public_url: &str,
        ledger_dir: &std::path::Path,
    ) -> Self {
        Self::build(pool, public_url, Some(ledger_dir), true).await
    }

    async fn build(
        pool: PgPool,
        public_url: &str,
        ledger_dir: Option<&std::path::Path>,
        existing: bool,
    ) -> Self {
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
        let ws = if existing {
            zirv_flow_store::find_workspace_by_slug(&pool, &Slug::parse("acme").expect("a slug"))
                .await
                .expect("a lookup")
                .expect("the workspace of the earlier world")
                .id
        } else {
            zirv_flow_store::create_workspace(
                &pool,
                &WorkspaceName::parse("Acme").expect("a name"),
                &Slug::parse("acme").expect("a slug"),
            )
            .await
            .expect("a workspace")
            .id
        };

        #[cfg(test)]
        let manual = ManualClock::new(OffsetDateTime::now_utc());
        #[cfg(test)]
        let clock = Clock::Manual(manual.clone());
        #[cfg(not(test))]
        let clock = Clock::Database;
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
            .with_public_url(Some(public_url.to_owned()))
            .with_cookie_secure(public_url.starts_with("https:"))
            .with_clock(clock.clone())
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
        let admin_key = key(
            "e2e",
            &[Scope::Admin, Scope::Publish, Scope::Write, Scope::Read],
        )
        .await;
        #[cfg(test)]
        let ingest = key("e2e ingest", &[Scope::Ingest]).await;

        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        #[cfg(test)]
        let provider = FakeProvider::start(FakeProviderOptions::default()).await;
        let dns = FixtureResolver::new();

        let mut config = WorkerConfig::new(1);
        config.poll_interval = Duration::from_millis(50);
        config.clock = clock;
        config.render_pool = Some(render.clone());
        config.dns = Some(Arc::new(dns.clone()));
        config.public_url = Some(public_url.to_owned());
        config.keyring = Some(keyring);
        config.egress = loopback;
        config.ledger_dir = ledger_dir.map(|dir| Arc::new(dir.to_path_buf()));
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
            #[cfg(test)]
            ingest,
            #[cfg(test)]
            clock: manual,
            sink,
            #[cfg(test)]
            provider,
            dns,
            #[cfg(test)]
            root,
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
    #[cfg(test)]
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
    #[cfg(test)]
    pub(super) fn provider_config(&self) -> Value {
        let text = PROVIDER_MANIFEST.replace(
            "https://api.fixture-provider.example.test",
            &self.provider.base_url(),
        );
        json!({"manifest": serde_json::from_str::<Value>(&text).expect("a manifest")})
    }

    /// A provider destination on the fake provider, with a credential holding the API token and
    /// the signing secret of the feedback webhook.
    #[cfg(test)]
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

    /// A provider destination called `name` on its own fake `provider`. With `erase` the manifest
    /// declares an erase call keyed on the email identity.
    #[cfg(test)]
    pub(super) async fn provider_destination_on(
        &self,
        name: &str,
        provider: &FakeProvider,
        erase: bool,
    ) -> String {
        let text = PROVIDER_MANIFEST.replace(
            "https://api.fixture-provider.example.test",
            &provider.base_url(),
        );
        let mut manifest: Value = serde_json::from_str(&text).expect("a manifest");
        if erase {
            manifest["destination"]["erase_profile"] = json!({
                "method": "POST",
                "path": "/erasures",
                "idempotency_header": "Idempotency-Key",
                "key_from": {"identity": "email"},
                "body": {"key": "/subject"}
            });
        }
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
                Some(json!({"kind": "custom", "name": name,
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
                Some(json!({"kind": "provider", "name": name,
                            "config": {"manifest": manifest},
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
    #[cfg(test)]
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
    #[cfg(test)]
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
    #[cfg(test)]
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
    #[cfg(test)]
    pub(super) async fn the_person(&self) -> Uuid {
        sqlx::query_scalar("SELECT id FROM people")
            .fetch_one(&self.pool)
            .await
            .expect("exactly one person")
    }

    /// Identifies the person with `address` through the ingest endpoint and subscribes them to
    /// email. Returns their id.
    #[cfg(test)]
    pub(super) async fn subscribed_person(&self, address: &str) -> Uuid {
        let identify = call(
            &self.router,
            "POST",
            "/api/v1/ingest/identify",
            Some(json!({"identifiers": {"email": address,
                "phone": zirv_flow_store::decision_testing::PERSONAL_DATA.phone,
                "external:trace_canary": zirv_flow_store::decision_testing::PERSONAL_DATA.external_id},
                "traits": zirv_flow_store::decision_testing::PERSONAL_DATA.traits()})),
            Some(&self.ingest),
        )
        .await;
        assert_eq!(identify.status, StatusCode::ACCEPTED, "{}", identify.text);
        self.settle().await;
        let person = self.the_person().await;
        self.api_ok(
            "PUT",
            &format!("/people/{person}/consent"),
            Some(json!({"channel": "email", "status": "subscribed"})),
            StatusCode::OK,
        )
        .await;
        person
    }

    /// A minute on, a track call of the `order_placed` event for `address`. Returns the trace id
    /// the call answered with.
    #[cfg(test)]
    pub(super) async fn track_order(&self, address: &str) -> String {
        self.clock.advance(time::Duration::minutes(1));
        let at = self.clock.now().format(&Rfc3339).expect("a time");
        let track = call(
            &self.router,
            "POST",
            "/api/v1/ingest/track",
            Some(
                json!({"identifiers": {"email": address}, "event": "order_placed",
                        "messageId": "order-1", "timestamp": at,
                        "properties": zirv_flow_store::decision_testing::PERSONAL_DATA.properties()}),
            ),
            Some(&self.ingest),
        )
        .await;
        assert_eq!(track.status, StatusCode::ACCEPTED, "{}", track.text);
        track.headers["x-trace-id"]
            .to_str()
            .expect("a trace id")
            .to_owned()
    }

    /// Publishes an event name that carries no properties, so segments can name it.
    #[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_world_starts_and_a_sending_domain_verifies_through_the_fixture_resolver(
        pool: PgPool,
    ) {
        let world = World::start(pool, PUBLIC_URL).await;

        let (identity, dkim) = world
            .verified_sender("example.com", "news@example.com")
            .await;

        assert!(!identity.is_empty());
        assert!(dkim.starts_with("v=DKIM1"), "{dkim}");
        world.finish().await;
    }
}
