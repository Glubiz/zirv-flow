//! The templates API end to end, driven through the router: the permission matrix, drafts,
//! publish and its immutability, validation against the registry and the event catalog, where-used
//! and preview through the render pool.

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;
    use zirv_flow_core::events::{EventMode, PropertyType, SchemaProperty};
    use zirv_flow_core::people::{Precedence, TraitType};
    use zirv_flow_core::{Role, Scope, WorkspaceId};
    use zirv_flow_render::{ChildCommand, PoolConfig, RenderPool};
    use zirv_flow_store::{NewEventSchema, NewTraitDefinition};
    use zirv_flow_templates::{
        Hooks, RenderContext, TemplateDocument, Tracking, render_email, render_webhook,
    };

    use crate::admin::{self, Context};
    use crate::templates::field_catalog;
    use crate::{AppState, router};

    const PASSWORD: &str = "correct horse battery";
    /// The id of the verified sender identity the fixture plants in workspace `ours`; the valid
    /// documents name it.
    const SENDER: &str = "0192b1a4-7c3e-7000-8000-00000000a001";

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Value,
        cookie: Option<String>,
    }

    struct Login {
        cookie: String,
        csrf: String,
    }

    enum Auth<'a> {
        Anonymous,
        Key(&'a str),
        Session(&'a Login),
    }

    struct Fixture {
        pool: PgPool,
        router: Router,
        ws: WorkspaceId,
        other_ws: WorkspaceId,
        read: String,
        write: String,
        admin: String,
        other_admin: String,
        /// A workspace member: may read and write, not administer.
        member: Login,
        /// A workspace admin.
        lead: Login,
    }

    /// The render host of the pool under test: a no-op unless the pool started it.
    ///
    /// In `hang_first` and `slow_first` mode the first child announces itself, reads its first
    /// request, creates the signal file (so a test knows the request reached a child) and then
    /// either never answers (`hang_first`, so the pool must kill it) or answers after a pause
    /// (`slow_first`).
    #[test]
    fn render_host_child() {
        use std::io::Read;

        let Ok(mode) = std::env::var("ZIRV_FLOW_TEST_TEMPLATE_HOST") else {
            return;
        };
        if mode == "hang_first" || mode == "slow_first" {
            let marker = std::env::var("ZIRV_FLOW_TEST_TEMPLATE_MARKER").unwrap_or_default();
            if !std::path::Path::new(&marker).exists() {
                std::fs::write(&marker, b"x").expect("the marker can be written");
                let frame = |out: &mut std::io::StdoutLock<'_>, bytes: &[u8]| {
                    out.write_all(&u32::try_from(bytes.len()).unwrap().to_be_bytes())
                        .unwrap();
                    out.write_all(bytes).unwrap();
                    out.flush().unwrap();
                };
                frame(&mut std::io::stdout().lock(), br#"{"kind":"ready"}"#);
                let mut input = std::io::stdin().lock();
                let mut prefix = [0u8; 4];
                input.read_exact(&mut prefix).unwrap();
                let mut request = vec![0u8; u32::from_be_bytes(prefix) as usize];
                input.read_exact(&mut request).unwrap();
                let signal = std::env::var("ZIRV_FLOW_TEST_TEMPLATE_SIGNAL").unwrap_or_default();
                std::fs::write(signal, b"x").expect("the signal can be written");
                if mode == "slow_first" {
                    // Answers, but only after a pause: a slow render, not a stuck one.
                    std::thread::sleep(Duration::from_secs(3));
                    let request: zirv_flow_templates::RenderRequest<'_> =
                        serde_json::from_slice(&request).unwrap();
                    let response = serde_json::to_vec(&request.run()).unwrap();
                    frame(&mut std::io::stdout().lock(), &response);
                    drop(input);
                    let served = zirv_flow_render::host::serve(
                        std::io::stdin().lock(),
                        std::io::stdout().lock(),
                    );
                    std::process::exit(i32::from(served.is_err()));
                }
                loop {
                    std::thread::sleep(Duration::from_secs(3600));
                }
            }
        }
        let limit = (mode == "memory_limit").then_some(536_870_912);
        std::process::exit(i32::from(zirv_flow_render::host::run(limit)));
    }

    async fn start_pool(mode: &str, deadline: Duration) -> RenderPool {
        start_signalling_pool(mode, deadline).await.0
    }

    /// A pool whose first child creates the returned file once it has received a request.
    async fn start_signalling_pool(
        mode: &str,
        deadline: Duration,
    ) -> (RenderPool, std::path::PathBuf) {
        let marker =
            std::env::temp_dir().join(format!("zirv-flow-template-host-{}", Uuid::now_v7()));
        let signal =
            std::env::temp_dir().join(format!("zirv-flow-template-signal-{}", Uuid::now_v7()));
        let command = ChildCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "templates::acceptance::tests::render_host_child",
                "--nocapture",
            ])
            .args(["--test-threads", "1", "-q"])
            .env("ZIRV_FLOW_TEST_TEMPLATE_HOST", mode)
            .env("ZIRV_FLOW_TEST_TEMPLATE_MARKER", marker)
            .env("ZIRV_FLOW_TEST_TEMPLATE_SIGNAL", &signal)
            .skip_stdout_until("running 1 test\n");
        let pool = RenderPool::start(command, PoolConfig { size: 1, deadline })
            .await
            .unwrap();
        (pool, signal)
    }

    /// Waits until the child has received its request. The bound is only a guard against a hang.
    async fn request_reached_a_child(signal: &std::path::Path) {
        for _ in 0..3000 {
            if signal.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no child received a request within a minute");
    }

    async fn send(
        router: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
        auth: Auth<'_>,
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(uri);
        request = match auth {
            Auth::Anonymous => request,
            Auth::Key(key) => request.header(header::AUTHORIZATION, format!("Bearer {key}")),
            Auth::Session(login) => request
                .header(header::COOKIE, &login.cookie)
                .header("x-csrf-token", &login.csrf),
        };
        let body = match body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::to_owned);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            cookie,
        }
    }

    async fn login_as(router: &Router, email: &str) -> Login {
        let reply = send(
            router,
            "POST",
            "/api/v1/session",
            Some(json!({"email": email, "password": PASSWORD})),
            Auth::Anonymous,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        Login {
            cookie: reply.cookie.clone().unwrap(),
            csrf: reply.body["csrf_token"].as_str().unwrap().to_owned(),
        }
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = zirv_flow_core::WorkspaceName::parse(slug).unwrap();
        let slug = zirv_flow_core::Slug::parse(slug).unwrap();
        zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id
    }

    async fn fixture_with(pool: &PgPool, render_pool: Option<RenderPool>) -> Fixture {
        fixture_at(pool, render_pool, None).await
    }

    async fn fixture_at(
        pool: &PgPool,
        render_pool: Option<RenderPool>,
        public_url: Option<&str>,
    ) -> Fixture {
        zirv_flow_store::migrate(pool).await.unwrap();
        let ws = workspace(pool, "ours").await;
        let other_ws = workspace(pool, "theirs").await;
        let mut state = AppState::new(pool.clone(), std::sync::Arc::new(String::new));
        if let Some(render_pool) = render_pool {
            state = state.with_render_pool(render_pool);
        }
        state = state.with_public_url(public_url.map(str::to_owned));
        let router = router(state);
        let key = |workspace: WorkspaceId, scope: Scope| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), workspace, "templates", &[scope])
                    .await
                    .unwrap()
                    .plaintext
            }
        };
        let read = key(ws, Scope::Read).await;
        let write = key(ws, Scope::Write).await;
        let admin_key = key(ws, Scope::Admin).await;
        let other_admin = key(other_ws, Scope::Admin).await;
        for (email, role) in [
            ("member@example.com", Role::Member),
            ("lead@example.com", Role::Admin),
        ] {
            let user = admin::create_user(pool, &Context::system(), email, PASSWORD)
                .await
                .unwrap();
            admin::set_member(pool, &Context::system(), ws, user.id, role)
                .await
                .unwrap();
        }
        let member = login_as(&router, "member@example.com").await;
        let lead = login_as(&router, "lead@example.com").await;
        add_trait(pool, ws, "plan").await;
        zirv_flow_store::sending_fixtures::plant_verified_sender_as(
            pool,
            ws,
            "example.com",
            SENDER.parse().unwrap(),
        )
        .await
        .unwrap();
        Fixture {
            pool: pool.clone(),
            router,
            ws,
            other_ws,
            read,
            write,
            admin: admin_key,
            other_admin,
            member,
            lead,
        }
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        let render_pool = start_pool("serve", Duration::from_secs(10)).await;
        fixture_with(pool, Some(render_pool)).await
    }

    async fn add_trait(pool: &PgPool, ws: WorkspaceId, key: &str) {
        zirv_flow_store::create_trait_definition(
            pool,
            ws,
            &NewTraitDefinition {
                key: key.into(),
                trait_type: TraitType::String,
                precedence: Precedence::Latest,
                source_order: Vec::new(),
                pii: false,
                channel_address: false,
                inferred: false,
                description: String::new(),
            },
        )
        .await
        .unwrap();
    }

    impl Fixture {
        fn templates(&self, tail: &str) -> String {
            format!("/api/v1/workspaces/{}/templates{tail}", self.ws.as_uuid())
        }

        async fn call(
            &self,
            method: &str,
            uri: &str,
            body: Option<Value>,
            auth: Auth<'_>,
        ) -> Reply {
            send(&self.router, method, uri, body, auth).await
        }

        /// As a workspace admin.
        async fn lead(&self, method: &str, tail: &str, body: Option<Value>) -> Reply {
            self.call(
                method,
                &self.templates(tail),
                body,
                Auth::Session(&self.lead),
            )
            .await
        }

        /// Creates a template as a workspace admin and returns its id.
        async fn create(&self, name: &str, document: Value) -> String {
            let reply = self
                .lead(
                    "POST",
                    "",
                    Some(json!({"name": name, "channel": document["channel"].clone(), "document": document})),
                )
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().to_owned()
        }

        async fn put_draft(&self, id: &str, document: Value) -> Reply {
            self.lead(
                "PUT",
                &format!("/{id}/draft"),
                Some(json!({ "document": document })),
            )
            .await
        }
        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        async fn preview_audits(&self) -> i64 {
            self.count("SELECT count(*) FROM audit_log WHERE action = 'template.previewed'")
                .await
        }

        async fn versions_written(&self) -> i64 {
            self.count("SELECT count(*) FROM template_versions").await
        }

        async fn person(&self, traits: Value, email: Option<&str>) -> Uuid {
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, $3)")
                .bind(id)
                .bind(self.ws.as_uuid())
                .bind(traits)
                .execute(&self.pool)
                .await
                .unwrap();
            if let Some(email) = email {
                sqlx::query(
                    "INSERT INTO identities (id, workspace_id, person_id, kind, value) \
                     VALUES ($1, $2, $3, 'email', $4)",
                )
                .bind(Uuid::now_v7())
                .bind(self.ws.as_uuid())
                .bind(id)
                .bind(email)
                .execute(&self.pool)
                .await
                .unwrap();
            }
            id
        }
    }

    /// A valid marketing email: a sender, a language and the unsubscribe link.
    fn email_doc(subject: &str, body: &str) -> Value {
        json!({"schema": 2, "channel": "email", "mode": "code", "email": {
            "subject": subject, "sender": SENDER, "lang": "en", "category": "marketing",
            "html": format!("<p>{body}</p><a href=\"{{{{ system.unsubscribe_url }}}}\">Unsubscribe</a>"),
            "text": format!("{body} {{{{ system.unsubscribe_url }}}}")
        }})
    }

    fn webhook_doc(body: &str) -> Value {
        json!({"schema": 2, "channel": "webhook", "mode": "code",
               "webhook": {"headers": {"X-Plan": "{{ person.plan }}"}, "body": body}})
    }

    /// A valid email document that names `sender`.
    fn email_doc_from(sender: &str) -> Value {
        let mut doc = email_doc("Hi", "x");
        doc["email"]["sender"] = json!(sender);
        doc
    }

    async fn publish_names(f: &Fixture, sender: &str) -> Reply {
        let id = f
            .create(&format!("Sender {sender}"), email_doc_from(sender))
            .await;
        f.lead("POST", &format!("/{id}/publish"), None).await
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn publish_is_refused_with_sender_unverified_until_the_senders_domain_is_verified(
        pool: PgPool,
    ) {
        use zirv_flow_store::sending_fixtures::{plant_domain, plant_identity, verify_domain};

        let f = fixture(&pool).await;
        let domain = plant_domain(&pool, f.ws, "pending.example.org")
            .await
            .unwrap();
        let sender = plant_identity(&pool, f.ws, domain, "news@pending.example.org")
            .await
            .unwrap()
            .to_string();

        let refused = publish_names(&f, &sender).await;

        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            refused.body
        );
        assert_eq!(refused.body["code"], "sender_unverified");
        assert!(
            refused.body["detail"]
                .as_str()
                .unwrap()
                .contains("pending.example.org")
        );
        assert_eq!(
            f.count("SELECT count(*) FROM template_versions").await,
            0,
            "a refused publish writes no version"
        );
        assert_eq!(
            f.count("SELECT count(*) FROM audit_log WHERE action = 'template.published'")
                .await,
            0
        );

        // The same template publishes once the domain verifies.
        verify_domain(&pool, f.ws, domain).await.unwrap();
        let id: String = sqlx::query_scalar("SELECT id::text FROM templates")
            .fetch_one(&pool)
            .await
            .unwrap();
        let published = f.lead("POST", &format!("/{id}/publish"), None).await;
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn publish_is_refused_for_an_unknown_archived_or_foreign_sender(pool: PgPool) {
        use zirv_flow_store::sending_fixtures::plant_verified_sender;

        let f = fixture(&pool).await;
        let archived = plant_verified_sender(&pool, f.ws, "gone.example.org")
            .await
            .unwrap();
        zirv_flow_store::archive_sender_identity(&pool, f.ws, archived)
            .await
            .unwrap();
        let foreign = plant_verified_sender(&pool, f.other_ws, "theirs.example.org")
            .await
            .unwrap();

        for sender in [
            archived.to_string(),
            foreign.to_string(),
            uuid::Uuid::now_v7().to_string(),
        ] {
            let refused = publish_names(&f, &sender).await;
            assert_eq!(
                refused.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{sender}: {:?}",
                refused.body
            );
            assert_eq!(refused.body["code"], "sender_unverified", "{sender}");
        }
        assert_eq!(f.count("SELECT count(*) FROM template_versions").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_webhook_template_needs_no_sender_and_the_senders_column_follows_the_draft(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let webhook = f
            .lead(
                "POST",
                "",
                Some(json!({"name": "Hook", "channel": "webhook",
                            "document": webhook_doc("{\"plan\": \"{{ person.plan }}\"}")})),
            )
            .await;
        assert_eq!(webhook.status, StatusCode::CREATED, "{:?}", webhook.body);
        let published = f
            .lead(
                "POST",
                &format!("/{}/publish", webhook.body["id"].as_str().unwrap()),
                None,
            )
            .await;
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );

        // The template's sender is read-only: it follows the draft's document, and an id in an
        // update is not a way to set it.
        let id = f.create("Mail", email_doc("Hi", "x")).await;
        let read = f.lead("GET", &format!("/{id}"), None).await;
        assert_eq!(read.body["sender_identity_id"], SENDER);
        let ignored = f
            .lead(
                "PUT",
                &format!("/{id}"),
                Some(json!({"name": "Mail 2", "sender_identity_id": uuid::Uuid::now_v7()})),
            )
            .await;
        assert_eq!(ignored.status, StatusCode::OK, "{:?}", ignored.body);
        assert_eq!(ignored.body["sender_identity_id"], SENDER);
        let unknown = f
            .put_draft(&id, email_doc_from(&uuid::Uuid::now_v7().to_string()))
            .await;
        assert_eq!(unknown.status, StatusCode::OK, "{:?}", unknown.body);
        let read = f.lead("GET", &format!("/{id}"), None).await;
        assert!(read.body["sender_identity_id"].is_null());
        f.put_draft(&id, email_doc("Hi", "x")).await;
        let read = f.lead("GET", &format!("/{id}"), None).await;
        assert_eq!(read.body["sender_identity_id"], SENDER);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sender_that_is_not_an_id_is_a_missing_sender_not_an_unverified_one(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Mail", email_doc_from("snd_1")).await;

        let refused = f.lead("POST", &format!("/{id}/publish"), None).await;

        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            refused.body
        );
        assert_eq!(refused.body["code"], "template_invalid");
        let issues = refused.body["template_issues"].as_array().unwrap();
        assert!(
            issues
                .iter()
                .any(|issue| issue["code"] == "missing_sender" && issue["at"] == "/email/sender"),
            "{issues:?}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_domain_cannot_be_deleted_under_a_published_version_that_names_its_sender(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let id = f.create("Mail", email_doc("Hi", "x")).await;
        let published = f.lead("POST", &format!("/{id}/publish"), None).await;
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        // The draft moves on and the identity is archived; the published version still names it.
        f.put_draft(&id, email_doc_from("")).await;
        let sender: uuid::Uuid = SENDER.parse().unwrap();
        zirv_flow_store::archive_sender_identity(&pool, f.ws, sender)
            .await
            .unwrap();
        let domain: uuid::Uuid = sqlx::query_scalar("SELECT domain_id FROM sender_identities")
            .fetch_one(&pool)
            .await
            .unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();

        let outcome = zirv_flow_store::delete_sending_domain(&mut tx, f.ws, domain)
            .await
            .unwrap();

        assert_eq!(outcome, zirv_flow_store::DomainDelete::InUse);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn each_route_needs_its_permission_and_no_other(pool: PgPool) {
        let f = fixture(&pool).await;
        let template = f.create("Welcome", email_doc("Hi", "x")).await;
        let person = f.person(json!({"plan": "pro"}), None).await;
        let id = |tail: &str| format!("/{template}{tail}");
        let credentials: [(&str, Auth<'_>, u16); 7] = [
            ("anonymous", Auth::Anonymous, 0),
            ("read key", Auth::Key(&f.read), 1),
            ("write key", Auth::Key(&f.write), 2),
            ("admin key", Auth::Key(&f.admin), 3),
            ("member", Auth::Session(&f.member), 12),
            ("workspace admin", Auth::Session(&f.lead), 123),
            ("other workspace's admin key", Auth::Key(&f.other_admin), 9),
        ];
        // The levels each credential holds: 1 read, 2 write, 3 admin. A key holds one scope; a
        // member reads and writes; a workspace admin does all three.
        let held = |code: u16, need: u16| code.to_string().contains(&need.to_string());
        let requests: [(&str, &str, String, Option<Value>, u16); 14] = [
            ("list", "GET", String::new(), None, 1),
            ("get", "GET", id(""), None, 1),
            ("versions", "GET", id("/versions"), None, 1),
            ("where used", "GET", id("/where-used"), None, 1),
            (
                "create",
                "POST",
                String::new(),
                Some(json!({"name": "Other", "channel": "email"})),
                2,
            ),
            ("update", "PUT", id(""), Some(json!({"name": "Renamed"})), 2),
            (
                "save draft",
                "PUT",
                id("/draft"),
                Some(json!({"document": email_doc("Hi", "y")})),
                2,
            ),
            (
                "convert to mjml",
                "POST",
                id("/draft/convert-to-mjml"),
                None,
                2,
            ),
            ("sample preview", "POST", id("/preview"), Some(json!({})), 2),
            (
                "real person preview",
                "POST",
                id("/preview"),
                Some(json!({"person_id": person})),
                3,
            ),
            ("publish", "POST", id("/publish"), None, 3),
            ("archive", "DELETE", id(""), None, 3),
            ("get version", "GET", id("/versions/1"), None, 1),
            ("version stats", "GET", id("/versions/1/stats"), None, 1),
        ];

        for (label, method, tail, body, need) in requests {
            for (who, auth, code) in &credentials {
                let auth = match auth {
                    Auth::Anonymous => Auth::Anonymous,
                    Auth::Key(key) => Auth::Key(key),
                    Auth::Session(login) => Auth::Session(login),
                };
                let reply = f
                    .call(method, &f.templates(&tail), body.clone(), auth)
                    .await;
                let expected_status = match (*code, held(*code, need)) {
                    (0, _) => StatusCode::UNAUTHORIZED,
                    (9, _) => StatusCode::NOT_FOUND,
                    (_, true) => {
                        // Allowed: whatever the route answers, it is not a refusal. (The version
                        // read answers 404 until the publish above it has run, so it is last.)
                        if !matches!(label, "get version" | "version stats") {
                            assert!(
                                !matches!(
                                    reply.status,
                                    StatusCode::UNAUTHORIZED
                                        | StatusCode::FORBIDDEN
                                        | StatusCode::NOT_FOUND
                                ),
                                "{label} as {who}: {} {:?}",
                                reply.status,
                                reply.body
                            );
                        }
                        continue;
                    }
                    (_, false) => StatusCode::FORBIDDEN,
                };
                assert_eq!(reply.status, expected_status, "{label} as {who}");
            }
        }
        // A caller with no access to the workspace at all learns nothing about it: 404, not 403.
        let stranger = f
            .call(
                "GET",
                &format!(
                    "/api/v1/workspaces/{}/templates/{template}",
                    f.other_ws.as_uuid()
                ),
                None,
                Auth::Key(&f.admin),
            )
            .await;
        assert_eq!(stranger.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn publish_creates_versions_one_and_two_and_a_version_never_changes(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", email_doc("First", "one")).await;
        let tail = |rest: &str| format!("/{id}{rest}");

        let one = f.lead("POST", &tail("/publish"), None).await;
        assert_eq!(one.status, StatusCode::CREATED, "{:?}", one.body);
        assert_eq!(one.body["version"]["version"], 1);
        assert_eq!(one.body["template"]["current_version"], 1);
        let saved = f
            .lead(
                "PUT",
                &tail("/draft"),
                Some(json!({"document": email_doc("Second", "two")})),
            )
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert_eq!(saved.body["draft"]["revision"], 2);
        let two = f.lead("POST", &tail("/publish"), None).await;
        assert_eq!(two.body["version"]["version"], 2);

        // Editing the draft after a publish changes no version.
        f.lead(
            "PUT",
            &tail("/draft"),
            Some(json!({"document": email_doc("Third", "three")})),
        )
        .await;
        let versions = f.lead("GET", &tail("/versions"), None).await;
        let numbers: Vec<i64> = versions
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["version"].as_i64().unwrap())
            .collect();
        assert_eq!(numbers, [2, 1]);
        assert!(
            versions.body[0].get("document").is_none(),
            "the history carries no documents"
        );
        assert_eq!(
            (&versions.body[0]["blocking"], &versions.body[0]["warnings"]),
            (&json!(0), &json!(0))
        );
        let first = f.lead("GET", &tail("/versions/1"), None).await;
        assert_eq!(first.body["document"], email_doc("First", "one"));
        let current = f.lead("GET", &tail("/versions/2"), None).await;
        assert_eq!(current.body["document"], email_doc("Second", "two"));
        let missing = f.lead("GET", &tail("/versions/3"), None).await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);

        // The row itself refuses an update, whoever asks.
        let update = sqlx::query("UPDATE template_versions SET document = '{}'::jsonb")
            .execute(&f.pool)
            .await;
        assert!(update.is_err());

        // A publish is audited and appends a decision caused by a definition change.
        assert_eq!(
            f.count("SELECT count(*) FROM audit_log WHERE action = 'template.published'")
                .await,
            2
        );
        let decisions: Vec<(String, String)> = sqlx::query_as(
            "SELECT action, cause_kind FROM decision_records WHERE action LIKE 'template.%'",
        )
        .fetch_all(&f.pool)
        .await
        .unwrap();
        assert_eq!(
            decisions,
            vec![
                (
                    "template.publish".to_owned(),
                    "definition_change".to_owned()
                );
                2
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_blocked_publish_is_a_422_with_every_issue_and_writes_no_version(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f
            .create("Welcome", email_doc("Hi {{ person.nope }}", "x"))
            .await;
        let before = (
            f.versions_written().await,
            f.count("SELECT count(*) FROM decision_records WHERE action = 'template.publish'")
                .await,
            f.count("SELECT count(*) FROM audit_log WHERE action = 'template.published'")
                .await,
        );

        let refused = f.lead("POST", &format!("/{id}/publish"), None).await;

        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(refused.body["code"], "template_invalid");
        let issues = refused.body["template_issues"].as_array().unwrap();
        let unknown = issues
            .iter()
            .find(|issue| issue["code"] == "unknown_merge_field")
            .expect("the unknown field is listed");
        assert_eq!(unknown["field"], "person.nope");
        assert_eq!(unknown["severity"], "blocking");
        assert_eq!(unknown["at"], "/email/subject");
        let after = (
            f.versions_written().await,
            f.count("SELECT count(*) FROM decision_records WHERE action = 'template.publish'")
                .await,
            f.count("SELECT count(*) FROM audit_log WHERE action = 'template.published'")
                .await,
        );
        assert_eq!(before, after);
        let template = f.lead("GET", &format!("/{id}"), None).await;
        assert_eq!(template.body["current_version"], Value::Null);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn merge_fields_come_from_the_registry_the_event_catalog_and_the_reserved_email(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let new = NewEventSchema {
            name: "order.placed".into(),
            mode: EventMode::Warn,
            description: String::new(),
            properties: vec![SchemaProperty {
                name: "total".into(),
                property_type: PropertyType::Number,
                required: false,
                enum_values: None,
                description: String::new(),
                pii: false,
            }],
        };
        zirv_flow_store::create_event_schema(&f.pool, f.ws, &new)
            .await
            .unwrap();
        let mut connection = f.pool.acquire().await.unwrap();
        zirv_flow_store::publish_schema_draft(&mut connection, f.ws, "order.placed", None)
            .await
            .unwrap();
        drop(connection);
        let good = f
            .create(
                "Good",
                email_doc(
                    "{{ person.plan }} {{ person.email }}",
                    "{{ event.total }} {{ person.plan }}",
                ),
            )
            .await;
        let bad = f
            .create("Bad", email_doc("{{ event.nope }}", "{{ person.nick }}"))
            .await;

        let published = f.lead("POST", &format!("/{good}/publish"), None).await;
        let refused = f.lead("POST", &format!("/{bad}/publish"), None).await;

        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        let fields: Vec<&str> = refused.body["template_issues"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|issue| issue["code"] == "unknown_merge_field")
            .map(|issue| issue["field"].as_str().unwrap())
            .collect();
        assert!(fields.contains(&"event.nope"), "{fields:?}");
        assert!(fields.contains(&"person.nick"), "{fields:?}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_draft_keeps_its_templates_channel_category_and_a_registered_content_type(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;
        let draft = |document: Value| f.put_draft(&id, document);
        let webhook = draft(webhook_doc("{}")).await;
        let transactional = {
            let mut doc = email_doc("Hi", "x");
            doc["email"]["category"] = json!("transactional");
            draft(doc).await
        };
        let blocks = draft(json!({"schema": 2, "channel": "email", "mode": "blocks"})).await;
        let nonsense = draft(json!({"channel": "email"})).await;
        let incomplete = draft(json!({"schema": 2, "channel": "email", "mode": "code",
                                      "email": {"subject": "", "html": ""}}))
        .await;

        assert_eq!(webhook.body["code"], "template_mismatch");
        assert_eq!(transactional.body["code"], "template_mismatch");
        assert_eq!(blocks.body["code"], "template_content_unsupported");
        assert_eq!(nonsense.body["code"], "template_malformed");
        // An incomplete draft is fine: only a publish validates it.
        assert_eq!(incomplete.status, StatusCode::OK, "{:?}", incomplete.body);
        assert_eq!(incomplete.body["draft"]["content_type"], "html");
        let refused = f.lead("POST", &format!("/{id}/publish"), None).await;
        assert_eq!(refused.body["code"], "template_invalid");

        // And a template made for a webhook takes a webhook document.
        let hook = f
            .lead(
                "POST",
                "",
                Some(json!({"name": "Hook", "channel": "webhook"})),
            )
            .await;
        assert_eq!(hook.status, StatusCode::CREATED, "{:?}", hook.body);
        assert_eq!(hook.body["category"], "transactional");
        assert_eq!(hook.body["draft"]["content_type"], "webhook");
        let wrong = f
            .lead(
                "POST",
                "",
                Some(json!({"name": "Mixed", "channel": "webhook",
                            "document": email_doc("Hi", "x")})),
            )
            .await;
        assert_eq!(wrong.body["code"], "template_mismatch");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_archived_template_is_read_only_and_its_name_is_free(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;
        f.lead("POST", &format!("/{id}/publish"), None).await;

        let archived = f.lead("DELETE", &format!("/{id}"), None).await;
        assert_eq!(archived.status, StatusCode::OK, "{:?}", archived.body);
        assert!(archived.body["archived_at"].is_string());

        for (method, tail, body) in [
            ("DELETE", String::new(), None),
            ("POST", "/publish".to_owned(), None),
            (
                "PUT",
                "/draft".to_owned(),
                Some(json!({"document": email_doc("Hi", "y")})),
            ),
            ("PUT", String::new(), Some(json!({"name": "New"}))),
        ] {
            let reply = f.lead(method, &format!("/{id}{tail}"), body).await;
            assert_eq!(reply.status, StatusCode::CONFLICT, "{method} {tail}");
            assert_eq!(reply.body["code"], "template_archived");
        }
        // Versions stay readable, the list hides it unless asked, and the name is free again.
        let versions = f.lead("GET", &format!("/{id}/versions"), None).await;
        assert_eq!(versions.body.as_array().unwrap().len(), 1);
        let live = f.lead("GET", "", None).await;
        assert!(live.body.as_array().unwrap().is_empty());
        let all = f.lead("GET", "?archived=true", None).await;
        assert_eq!(all.body.as_array().unwrap().len(), 1);
        f.create("Welcome", email_doc("Hi", "x")).await;
        let duplicate = f
            .lead(
                "POST",
                "",
                Some(json!({"name": "WELCOME", "channel": "email"})),
            )
            .await;
        assert_eq!(duplicate.status, StatusCode::CONFLICT);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sample_preview_returns_the_bytes_of_the_renderer_for_the_same_input(pool: PgPool) {
        let f = fixture(&pool).await;
        let document = email_doc("Hello {{ person.plan }}", "Plan {{ person.plan }}");
        let id = f.create("Welcome", document.clone()).await;
        let hook_document =
            webhook_doc(r#"{"plan":"{{ person.plan }}","who":"{{ person.email }}"}"#);
        let hook = f
            .lead(
                "POST",
                "",
                Some(json!({"name": "Hook", "channel": "webhook", "document": hook_document})),
            )
            .await;
        let hook = hook.body["id"].as_str().unwrap().to_owned();
        let mut connection = f.pool.acquire().await.unwrap();
        let catalog = field_catalog(
            zirv_flow_store::read_template_field_catalog(&mut connection, f.ws)
                .await
                .unwrap(),
        );
        drop(connection);
        let context = RenderContext::sample_person(&catalog);

        let email = f
            .lead("POST", &format!("/{id}/preview"), Some(json!({})))
            .await;
        let webhook = f
            .lead("POST", &format!("/{hook}/preview"), Some(json!({})))
            .await;

        let doc: TemplateDocument = serde_json::from_value(document).unwrap();
        let hooks = Hooks {
            tracking: Tracking::Inert {
                base_url: "https://preview.sample.invalid/t",
            },
            utm: None,
        };
        let expected = render_email(&doc, &context, &hooks).unwrap();
        assert_eq!(email.status, StatusCode::OK, "{:?}", email.body);
        assert_eq!(email.body["rendered"], true);
        assert_eq!(email.body["content_type"], "html");
        assert_eq!(email.body["source"], "draft");
        assert_eq!(email.body["subject"], expected.subject.as_str());
        assert_eq!(email.body["html"], expected.html.as_str());
        assert_eq!(email.body["text"], expected.text.as_str());
        let headers: Vec<(String, String)> = email.body["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                (
                    h["name"].as_str().unwrap().to_owned(),
                    h["value"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(headers, expected.headers);
        assert!(
            !headers.is_empty(),
            "a marketing email carries List-Unsubscribe"
        );
        // Inert tracking: a web link in the content would carry the inert token.
        assert!(
            email.body["html"]
                .as_str()
                .unwrap()
                .contains("sample.invalid")
        );

        let doc: TemplateDocument = serde_json::from_value(hook_document).unwrap();
        let expected = render_webhook(&doc, &context).unwrap();
        assert_eq!(webhook.status, StatusCode::OK, "{:?}", webhook.body);
        assert_eq!(webhook.body["content_type"], "webhook");
        assert_eq!(webhook.body["body"], expected.body.as_str());
        assert_eq!(
            webhook.body["headers"][0]["name"],
            expected.headers[0].0.as_str()
        );
        assert_eq!(
            webhook.body["headers"][0]["value"],
            expected.headers[0].1.as_str()
        );
    }

    fn mjml_doc(body: &str, footer: &str) -> Value {
        json!({"schema": 2, "channel": "email", "mode": "code", "email": {
            "subject": "Hello {{ person.plan }}", "sender": SENDER, "category": "marketing",
            "syntax": "mjml",
            "html": format!(
                "<mjml lang=\"en\"><mj-body><mj-section><mj-column>\
                 <mj-text>{body} {{{{ person.plan }}}}</mj-text>\
                 <mj-text>{footer}</mj-text>\
                 </mj-column></mj-section></mj-body></mjml>"
            ),
            "text": "Plan {{ person.plan }} {{ system.unsubscribe_url }}"
        }})
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_mjml_template_drafts_publishes_and_previews_like_an_html_one(pool: PgPool) {
        let f = fixture(&pool).await;
        let footer = r#"<a href="{{ system.unsubscribe_url }}">Unsubscribe</a>"#;
        let document = mjml_doc("Welcome", footer);
        let id = f.create("Newsletter", document.clone()).await;

        let draft = f.lead("GET", &format!("/{id}"), None).await;
        assert_eq!(draft.status, StatusCode::OK, "{:?}", draft.body);
        assert_eq!(draft.body["draft"]["content_type"], "mjml");
        assert_eq!(draft.body["draft"]["document"], document);

        let published = f.lead("POST", &format!("/{id}/publish"), None).await;
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        assert_eq!(published.body["version"]["version"], 1);
        assert_eq!(published.body["template"]["current_version"], 1);

        let mut connection = f.pool.acquire().await.unwrap();
        let catalog = field_catalog(
            zirv_flow_store::read_template_field_catalog(&mut connection, f.ws)
                .await
                .unwrap(),
        );
        drop(connection);
        let context = RenderContext::sample_person(&catalog);
        let preview = f
            .lead("POST", &format!("/{id}/preview"), Some(json!({})))
            .await;
        let doc: TemplateDocument = serde_json::from_value(document).unwrap();
        let hooks = Hooks {
            tracking: Tracking::Inert {
                base_url: "https://preview.sample.invalid/t",
            },
            utm: None,
        };
        let expected = render_email(&doc, &context, &hooks).unwrap();
        assert_eq!(preview.status, StatusCode::OK, "{:?}", preview.body);
        assert_eq!(preview.body["rendered"], true);
        assert_eq!(preview.body["content_type"], "mjml");
        assert_eq!(preview.body["html"], expected.html.as_str());
        assert_eq!(preview.body["text"], expected.text.as_str());
        assert!(expected.html.contains("<!--[if mso"), "{}", expected.html);

        // Without the unsubscribe link the publish is refused with the same code as for HTML.
        let bad = f.create("No footer", mjml_doc("Welcome", "no link")).await;
        let refused = f.lead("POST", &format!("/{bad}/publish"), None).await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            refused.body
        );
        assert_eq!(refused.body["code"], "template_invalid");
        assert!(
            refused.body["template_issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["code"] == "missing_unsubscribe"),
            "{:?}",
            refused.body
        );
    }

    /// A valid marketing block email: a sender, a language and the unsubscribe link.
    fn blocks_doc(subject: &str, footer: &str) -> Value {
        json!({"schema": 2, "channel": "email", "mode": "blocks", "blocks": {
        "subject": subject, "sender": SENDER, "lang": "en", "category": "marketing",
        "sections": [
            {"id": "intro", "columns": [[
                {"type": "heading", "id": "title", "level": 1, "text": "Hello {{ person.plan }}"},
                {"type": "button", "id": "cta", "label": "Open", "url": "https://shop.example/a?b=1&c=2"}
            ]]},
            {"id": "foot", "columns": [[
                {"type": "text", "id": "legal", "content": footer}
            ]]}
        ]}})
    }

    const LINK: &str = r#"<p><a href="{{ system.unsubscribe_url }}">Unsubscribe</a></p>"#;

    impl Fixture {
        /// A call as a workspace admin to a path under the workspace.
        async fn ws_call(&self, method: &str, tail: &str, body: Option<Value>) -> Reply {
            let uri = format!("/api/v1/workspaces/{}{tail}", self.ws.as_uuid());
            self.call(method, &uri, body, Auth::Session(&self.lead))
                .await
        }

        /// A published snippet; returns its id.
        async fn snippet(&self, name: &str, blocks: Value) -> String {
            let made = self
                .ws_call(
                    "POST",
                    "/snippets",
                    Some(json!({"name": name, "content": {"blocks": blocks}})),
                )
                .await;
            assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
            let id = made.body["id"].as_str().unwrap().to_owned();
            let published = self
                .ws_call("POST", &format!("/snippets/{id}/publish"), None)
                .await;
            assert_eq!(
                published.status,
                StatusCode::CREATED,
                "{:?}",
                published.body
            );
            id
        }

        /// A published theme; returns its id.
        async fn theme(&self, name: &str, tokens: Value) -> String {
            let made = self
                .ws_call(
                    "POST",
                    "/themes",
                    Some(json!({"name": name, "tokens": tokens})),
                )
                .await;
            assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
            let id = made.body["id"].as_str().unwrap().to_owned();
            let published = self
                .ws_call("POST", &format!("/themes/{id}/publish"), None)
                .await;
            assert_eq!(
                published.status,
                StatusCode::CREATED,
                "{:?}",
                published.body
            );
            id
        }

        async fn publish_doc(&self, name: &str, document: Value) -> Reply {
            let id = self.create(name, document).await;
            self.lead("POST", &format!("/{id}/publish"), None).await
        }

        async fn preview_of(&self, id: &str, body: Value) -> Reply {
            let reply = self
                .lead("POST", &format!("/{id}/preview"), Some(body))
                .await;
            assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
            reply
        }
    }

    fn issue_pairs(reply: &Reply) -> Vec<(String, String)> {
        reply.body["template_issues"]
            .as_array()
            .map(|issues| {
                issues
                    .iter()
                    .map(|issue| {
                        (
                            issue["code"].as_str().unwrap().to_owned(),
                            issue["at"].as_str().unwrap().to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_valid_condition_publishes_and_a_bad_one_is_refused_at_the_ruled_pointer(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let mut ok = blocks_doc("Welcome", LINK);
        ok["blocks"]["sections"][0]["columns"][0][0]["visible_if"] =
            json!({"trait": "plan", "op": "eq", "value": "pro"});
        let published = f.publish_doc("Valid", ok).await;
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        assert_eq!(published.body["used_by"], json!([]));

        let mut bad = blocks_doc("Welcome", LINK);
        bad["blocks"]["sections"][0]["columns"][0][0]["visible_if"] =
            json!({"trait": "nope", "op": "eq", "value": "pro"});
        bad["blocks"]["sections"][0]["visible_if"] = json!({"segment": Uuid::now_v7()});
        let refused = f.publish_doc("Invalid", bad).await;

        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        let pairs = issue_pairs(&refused);
        assert!(
            pairs.contains(&(
                "invalid_condition".into(),
                "/blocks/block/title/visible_if".into()
            )),
            "{pairs:?}"
        );
        assert!(
            pairs.contains(&(
                "broken_segment_reference".into(),
                "/blocks/sections/intro/visible_if".into()
            )),
            "{pairs:?}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_block_document_needs_the_unsubscribe_link_in_an_unconditioned_place(pool: PgPool) {
        let f = fixture(&pool).await;
        let none = f.publish_doc("Bare", blocks_doc("Hi", "<p>bye</p>")).await;
        assert_eq!(none.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            issue_pairs(&none)
                .iter()
                .any(|(code, _)| code == "missing_unsubscribe"),
            "{:?}",
            none.body
        );

        let linked = f
            .snippet(
                "Legal",
                json!([{"type": "text", "id": "t", "content": LINK}]),
            )
            .await;
        let mut with_snippet = blocks_doc("Hi", "<p>bye</p>");
        with_snippet["blocks"]["sections"][1]["columns"][0]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "snippet", "id": "legal2", "snippet_id": linked}));
        let ok = f.publish_doc("Snippet", with_snippet).await;
        assert_eq!(ok.status, StatusCode::CREATED, "{:?}", ok.body);

        let theme = f
            .theme(
                "Brand",
                json!({"primary": "#112233", "footer_snippet": linked}),
            )
            .await;
        let mut with_theme = blocks_doc("Hi", "<p>bye</p>");
        with_theme["blocks"]["theme"] = json!({"id": theme});
        let ok = f.publish_doc("Footer", with_theme).await;
        assert_eq!(ok.status, StatusCode::CREATED, "{:?}", ok.body);

        let mut hidden = blocks_doc("Hi", LINK);
        hidden["blocks"]["sections"][1]["visible_if"] =
            json!({"trait": "plan", "op": "eq", "value": "pro"});
        let conditioned = f.publish_doc("Conditioned link", hidden).await;
        assert_eq!(conditioned.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            issue_pairs(&conditioned)
                .iter()
                .any(|(code, _)| code == "missing_unsubscribe"),
            "{:?}",
            conditioned.body
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_mjml_publish_with_an_unknown_merge_field_is_a_422_and_writes_no_version(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let footer = r#"<a href="{{ system.unsubscribe_url }}">Unsubscribe</a>"#;
        let id = f
            .create("Newsletter", mjml_doc("Hi {{ person.nope }}", footer))
            .await;

        let refused = f.lead("POST", &format!("/{id}/publish"), None).await;

        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            refused.body
        );
        assert_eq!(refused.body["code"], "template_invalid");
        let issues = refused.body["template_issues"].as_array().unwrap();
        let unknown = issues
            .iter()
            .find(|issue| issue["code"] == "unknown_merge_field")
            .expect("the unknown field is listed");
        assert_eq!(unknown["field"], "person.nope");
        assert_eq!(unknown["severity"], "blocking");
        assert_eq!(unknown["at"], "/email/html");
        assert_eq!(f.versions_written().await, 0);
        let template = f.lead("GET", &format!("/{id}"), None).await;
        assert_eq!(template.body["current_version"], Value::Null);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_block_publish_with_an_unknown_merge_field_is_a_422_and_writes_no_version(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let mut document = blocks_doc("Hi", LINK);
        document["blocks"]["sections"][0]["columns"][0]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "text", "id": "note", "content": "<p>{{ person.nope }}</p>"}));
        let id = f.create("Blocks", document).await;

        let refused = f.lead("POST", &format!("/{id}/publish"), None).await;

        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            refused.body
        );
        assert_eq!(refused.body["code"], "template_invalid");
        let issues = refused.body["template_issues"].as_array().unwrap();
        let unknown = issues
            .iter()
            .find(|issue| issue["code"] == "unknown_merge_field")
            .expect("the unknown field is listed");
        assert_eq!(unknown["field"], "person.nope");
        assert_eq!(unknown["severity"], "blocking");
        assert_eq!(unknown["at"], "/blocks/block/note/content");
        assert_eq!(f.versions_written().await, 0);
        let template = f.lead("GET", &format!("/{id}"), None).await;
        assert_eq!(template.body["current_version"], Value::Null);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn publish_records_the_assets_the_template_names_and_refuses_a_missing_one(pool: PgPool) {
        use zirv_flow_store::NewAsset;
        let f = fixture(&pool).await;
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let stored = zirv_flow_store::store_asset(
            &mut tx,
            f.ws,
            &NewAsset {
                name: "hero",
                ext: "png",
                content_type: "image/png",
                bytes: b"hero-bytes",
                width: 2,
                height: 3,
                created_by: None,
            },
            1 << 30,
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let image = |asset: String| {
            let mut doc = blocks_doc("Hi", LINK);
            doc["blocks"]["sections"][0]["columns"][0]
                .as_array_mut()
                .unwrap()
                .push(json!({"type": "image", "id": "pic", "alt": "x", "src": {"asset": asset}}));
            doc
        };

        let ok = f
            .publish_doc("With image", image(stored.asset.id.to_string()))
            .await;
        let refused = f
            .publish_doc("Missing image", image(Uuid::now_v7().to_string()))
            .await;

        assert_eq!(ok.status, StatusCode::CREATED, "{:?}", ok.body);
        assert_eq!(
            f.count(
                "SELECT count(*) FROM asset_references WHERE owner_kind = 'template' AND version = 1"
            )
            .await,
            1
        );
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            issue_pairs(&refused)
                .iter()
                .any(|(code, _)| code == "broken_asset_reference"),
            "{:?}",
            refused.body
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn preview_builds_asset_urls_on_the_public_url_like_a_send_and_an_http_one_is_refused(
        pool: PgPool,
    ) {
        use zirv_flow_store::NewAsset;
        let render_pool = start_pool("serve", Duration::from_secs(10)).await;
        let f = fixture_at(&pool, Some(render_pool), Some("http://localhost:8080")).await;
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let stored = zirv_flow_store::store_asset(
            &mut tx,
            f.ws,
            &NewAsset {
                name: "hero",
                ext: "png",
                content_type: "image/png",
                bytes: b"hero-bytes",
                width: 2,
                height: 3,
                created_by: None,
            },
            1 << 30,
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let mut doc = blocks_doc("Hi", LINK);
        doc["blocks"]["sections"][0]["columns"][0]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "image", "id": "pic", "alt": "x",
                         "src": {"asset": stored.asset.id.to_string()}}));
        let id = f.create("Http", doc).await;

        let preview = f.preview_of(&id, json!({})).await;

        assert_eq!(preview.body["rendered"], false, "{:?}", preview.body);
        assert!(
            preview.body["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["code"] == "broken_asset_reference"),
            "{:?}",
            preview.body
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn preview_hides_a_block_for_a_person_who_fails_its_condition_and_says_why(pool: PgPool) {
        let f = fixture(&pool).await;
        let mut doc = blocks_doc("Hi", LINK);
        doc["blocks"]["sections"][0]["columns"][0][0]["visible_if"] =
            json!({"trait": "plan", "op": "eq", "value": "pro"});
        let id = f.create("Promo", doc).await;
        let pro = f
            .person(json!({"plan": "pro"}), Some("pro@example.com"))
            .await;
        let free = f
            .person(json!({"plan": "free"}), Some("free@example.com"))
            .await;

        let sees = f.preview_of(&id, json!({"person_id": pro})).await;
        let hidden = f.preview_of(&id, json!({"person_id": free})).await;
        let sample = f.preview_of(&id, json!({})).await;

        assert_eq!(sees.body["hidden_blocks"], json!([]));
        assert!(sees.body["html"].as_str().unwrap().contains("Hello pro"));
        assert_eq!(
            hidden.body["hidden_blocks"],
            json!([{"id": "title", "reason": "trait `plan` was not met"}])
        );
        assert!(!hidden.body["html"].as_str().unwrap().contains("Hello"));
        assert_eq!(sample.body["hidden_blocks"][0]["id"], "title");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_named_themes_differ_only_in_tokens_and_the_default_moves_only_following_templates(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let red = f.theme("Red", json!({"primary": "#aa0000"})).await;
        let blue = f.theme("Blue", json!({"primary": "#0000bb"})).await;
        let doc_with = |theme: Value| {
            let mut doc = blocks_doc("Hi", LINK);
            doc["blocks"]["theme"] = theme;
            doc
        };
        let in_red = f.create("In red", doc_with(json!({"id": red}))).await;
        let in_blue = f.create("In blue", doc_with(json!({"id": blue}))).await;
        let follows = f.create("Follows", doc_with(json!("default"))).await;
        let html = |reply: Reply| reply.body["html"].as_str().unwrap().to_lowercase();

        let red_html = html(f.preview_of(&in_red, json!({})).await);
        let blue_html = html(f.preview_of(&in_blue, json!({})).await);
        let before = html(f.preview_of(&follows, json!({})).await);
        let set = f
            .ws_call("PUT", "/themes/default", Some(json!({"theme_id": red})))
            .await;
        assert_eq!(set.status, StatusCode::OK, "{:?}", set.body);
        let after_red = html(f.preview_of(&follows, json!({})).await);
        let set = f
            .ws_call("PUT", "/themes/default", Some(json!({"theme_id": blue})))
            .await;
        assert_eq!(set.status, StatusCode::OK, "{:?}", set.body);
        let after_blue = html(f.preview_of(&follows, json!({})).await);

        assert_eq!(red_html.replace("#aa0000", "#0000bb"), blue_html);
        assert_ne!(before, after_red);
        assert_eq!(after_red.replace("#aa0000", "#0000bb"), after_blue);
        assert_eq!(
            red_html,
            html(f.preview_of(&in_red, json!({})).await),
            "a named theme does not follow the default"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_stored_schema_one_version_is_served_at_schema_two_and_stays_schema_one(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let id = f.create("Old", email_doc("First", "one")).await;
        sqlx::query("UPDATE template_drafts SET document = jsonb_set(document, '{schema}', '1') WHERE template_id = $1::uuid")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let published = f.lead("POST", &format!("/{id}/publish"), None).await;
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );

        let served = f.lead("GET", &format!("/{id}/versions/1"), None).await;

        assert_eq!(served.body["document"], email_doc("First", "one"));
        let stored: i32 = sqlx::query_scalar(
            "SELECT (document->>'schema')::int FROM template_versions WHERE template_id = $1::uuid AND version = 1",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stored, 1, "the stored row is not rewritten");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_condition_nested_200_levels_deep_is_a_client_error_not_a_crash(pool: PgPool) {
        let f = fixture(&pool).await;
        let footer = r#"<p><a href="{{ system.unsubscribe_url }}">Unsubscribe</a></p>"#;
        let id = f.create("Deep", blocks_doc("Welcome", footer)).await;
        let mut deep = json!(1);
        for _ in 0..200 {
            deep = json!([deep]);
        }
        let mut document = blocks_doc("Welcome", footer);
        document["blocks"]["sections"][0]["columns"][0][0]["visible_if"] = deep;

        let reply = f.put_draft(&id, document).await;

        assert!(reply.status.is_client_error(), "{:?}", reply.status);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_schema_one_block_draft_is_saved_and_served_at_schema_two(pool: PgPool) {
        let f = fixture(&pool).await;
        let footer = r#"<p><a href="{{ system.unsubscribe_url }}">Unsubscribe</a></p>"#;
        let id = f.create("Old", blocks_doc("Welcome", footer)).await;
        let mut old = blocks_doc("Welcome", footer);
        old["schema"] = json!(1);
        old["blocks"]["sections"][0]["columns"][0]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "image", "id": "pic", "src": "https://img.example/a.png", "alt": "A"}));

        let saved = f.put_draft(&id, old).await;

        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        let document = &saved.body["draft"]["document"];
        assert_eq!(document["schema"], 2);
        assert_eq!(document["blocks"]["theme"], "none");
        assert_eq!(document["blocks"]["footer"], "none");
        assert_eq!(
            document["blocks"]["sections"][0]["columns"][0][2]["src"],
            json!({"url": "https://img.example/a.png"})
        );
        let read = f.lead("GET", &format!("/{id}"), None).await;
        assert_eq!(read.body["draft"]["document"], *document);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_block_document_is_refused_when_its_sender_is_missing_or_unverified(pool: PgPool) {
        use zirv_flow_store::sending_fixtures::{
            plant_domain, plant_identity, plant_verified_sender,
        };

        let f = fixture(&pool).await;
        let footer = r#"<p><a href="{{ system.unsubscribe_url }}">Unsubscribe</a></p>"#;
        let domain = plant_domain(&pool, f.ws, "pending.example.org")
            .await
            .unwrap();
        let pending = plant_identity(&pool, f.ws, domain, "news@pending.example.org")
            .await
            .unwrap()
            .to_string();
        let theirs = plant_verified_sender(&pool, f.other_ws, "theirs.example.org")
            .await
            .unwrap()
            .to_string();
        let with_sender = |sender: &str| {
            let mut document = blocks_doc("Welcome", footer);
            document["blocks"]["sender"] = json!(sender);
            document
        };

        // A sender that names no identity is the same blocking issue as in a code document.
        let mut missing = blocks_doc("Welcome", footer);
        missing["blocks"]
            .as_object_mut()
            .expect("a blocks object")
            .remove("sender");
        let mut code_missing = email_doc("Welcome", "x");
        code_missing["email"]
            .as_object_mut()
            .expect("an email object")
            .remove("sender");
        let mut codes = Vec::new();
        for (name, document) in [
            ("Code missing", code_missing),
            ("Block missing", missing),
            ("Block not an id", with_sender("snd_1")),
        ] {
            let id = f.create(name, document).await;
            let refused = f.lead("POST", &format!("/{id}/publish"), None).await;
            assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{name}");
            codes.push(refused.body["code"].clone());
        }
        assert_eq!(codes, ["template_invalid"; 3]);

        // An identity that exists but cannot send is `sender_unverified`.
        for (name, sender) in [("Unverified", pending), ("Foreign", theirs)] {
            let id = f.create(name, with_sender(&sender)).await;
            let refused = f.lead("POST", &format!("/{id}/publish"), None).await;

            assert_eq!(
                refused.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{name}: {:?}",
                refused.body
            );
            assert_eq!(refused.body["code"], "sender_unverified", "{name}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_block_draft_converts_to_mjml_code_and_published_versions_stay(pool: PgPool) {
        let f = fixture(&pool).await;
        let footer = r#"<p><a href="{{ system.unsubscribe_url }}">Unsubscribe</a></p>"#;
        let document = blocks_doc("Welcome", footer);
        let id = f.create("Newsletter", document.clone()).await;
        let published = f.lead("POST", &format!("/{id}/publish"), None).await;
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        let before = f.lead("GET", &format!("/{id}/versions/1"), None).await;

        let converted = f
            .lead("POST", &format!("/{id}/draft/convert-to-mjml"), None)
            .await;

        assert_eq!(converted.status, StatusCode::OK, "{:?}", converted.body);
        let draft = &converted.body["draft"];
        assert_eq!(draft["content_type"], "mjml");
        assert_eq!(draft["revision"], 2);
        assert_eq!(draft["document"]["mode"], "code");
        assert!(draft["document"].get("blocks").is_none());
        let email = &draft["document"]["email"];
        assert_eq!(email["syntax"], "mjml");
        assert_eq!(email["subject"], "Welcome");
        assert_eq!(email["sender"], SENDER);
        assert_eq!(email["category"], "marketing");
        let mjml = email["html"].as_str().unwrap();
        assert!(mjml.starts_with("<mjml"), "{mjml}");
        // The merge tags stay as the marketer wrote them.
        assert!(mjml.contains("Hello {{ person.plan }}"), "{mjml}");
        assert!(mjml.contains("{{ system.unsubscribe_url }}"), "{mjml}");
        // The published version is untouched; the audit records the conversion.
        let after = f.lead("GET", &format!("/{id}/versions/1"), None).await;
        assert_eq!(after.body, before.body);
        assert_eq!(after.body["document"], document);
        assert_eq!(f.versions_written().await, 1);
        assert_eq!(
            f.count("SELECT count(*) FROM audit_log WHERE action = 'template.draft_converted'")
                .await,
            1
        );
        // The converted draft previews through the MJML path.
        let preview = f
            .lead("POST", &format!("/{id}/preview"), Some(json!({})))
            .await;
        assert_eq!(preview.status, StatusCode::OK, "{:?}", preview.body);
        assert_eq!(preview.body["rendered"], true);

        // It is one way: the draft is no longer in blocks mode.
        let again = f
            .lead("POST", &format!("/{id}/draft/convert-to-mjml"), None)
            .await;
        assert_eq!(again.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(again.body["code"], "template_malformed");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_draft_that_is_not_blocks_or_has_blocking_issues_is_not_converted(pool: PgPool) {
        let f = fixture(&pool).await;
        let code = f.create("Code", email_doc("Hi", "x")).await;
        let refused = f
            .lead("POST", &format!("/{code}/draft/convert-to-mjml"), None)
            .await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(refused.body["code"], "template_malformed");

        let footer = r#"<p><a href="{{ system.unsubscribe_url }}">Unsubscribe</a></p>"#;
        let mut broken = blocks_doc("Welcome", footer);
        broken["blocks"]["settings"] = json!({"content_width": 10});
        let id = f.create("Broken", broken.clone()).await;
        let refused = f
            .lead("POST", &format!("/{id}/draft/convert-to-mjml"), None)
            .await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(refused.body["code"], "template_invalid");
        assert!(
            refused.body["template_issues"]
                .as_array()
                .is_some_and(|issues| issues
                    .iter()
                    .any(|issue| issue["at"] == "/blocks/settings/content_width")),
            "{:?}",
            refused.body
        );
        // The draft is unchanged and nothing was audited.
        let draft = f.lead("GET", &format!("/{id}"), None).await;
        assert_eq!(draft.body["draft"]["document"], broken);
        assert_eq!(draft.body["draft"]["revision"], 1);
        assert_eq!(
            f.count("SELECT count(*) FROM audit_log WHERE action = 'template.draft_converted'")
                .await,
            0
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn convert_is_a_404_in_another_workspace_and_a_409_for_an_archived_template(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let footer = r#"<p><a href="{{ system.unsubscribe_url }}">Unsubscribe</a></p>"#;
        let id = f.create("Newsletter", blocks_doc("Welcome", footer)).await;
        let uri = f.templates(&format!("/{id}/draft/convert-to-mjml"));

        let stranger = f.call("POST", &uri, None, Auth::Key(&f.other_admin)).await;
        assert_eq!(
            stranger.status,
            StatusCode::NOT_FOUND,
            "{:?}",
            stranger.body
        );

        let archived = f.lead("DELETE", &format!("/{id}"), None).await;
        assert_eq!(archived.status, StatusCode::OK, "{:?}", archived.body);
        let refused = f
            .lead("POST", &format!("/{id}/draft/convert-to-mjml"), None)
            .await;
        assert_eq!(refused.status, StatusCode::CONFLICT, "{:?}", refused.body);
        assert_eq!(refused.body["code"], "template_archived");
        let draft = f.lead("GET", &format!("/{id}"), None).await;
        assert_eq!(draft.body["draft"]["document"]["mode"], "blocks");
        assert_eq!(draft.body["draft"]["revision"], 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_read_key_cannot_convert_a_draft(pool: PgPool) {
        let f = fixture(&pool).await;
        let footer = r#"<p><a href="{{ system.unsubscribe_url }}">Unsubscribe</a></p>"#;
        let id = f.create("Newsletter", blocks_doc("Welcome", footer)).await;
        let uri = f.templates(&format!("/{id}/draft/convert-to-mjml"));
        let refused = f.call("POST", &uri, None, Auth::Key(&f.read)).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        let allowed = f.call("POST", &uri, None, Auth::Key(&f.write)).await;
        assert_eq!(allowed.status, StatusCode::OK, "{:?}", allowed.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_blocks_template_drafts_publishes_and_previews_like_an_html_one(pool: PgPool) {
        let f = fixture(&pool).await;
        let footer = r#"<p><a href="{{ system.unsubscribe_url }}">Unsubscribe</a></p>"#;
        let document = blocks_doc("Welcome", footer);
        let id = f.create("Newsletter", document.clone()).await;

        let draft = f.lead("GET", &format!("/{id}"), None).await;
        assert_eq!(draft.status, StatusCode::OK, "{:?}", draft.body);
        assert_eq!(draft.body["draft"]["content_type"], "blocks");
        assert_eq!(draft.body["draft"]["document"], document);

        let published = f.lead("POST", &format!("/{id}/publish"), None).await;
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        assert_eq!(published.body["version"]["version"], 1);
        assert_eq!(published.body["template"]["current_version"], 1);

        let mut connection = f.pool.acquire().await.unwrap();
        let catalog = field_catalog(
            zirv_flow_store::read_template_field_catalog(&mut connection, f.ws)
                .await
                .unwrap(),
        );
        drop(connection);
        let context = RenderContext::sample_person(&catalog);
        let preview = f
            .lead("POST", &format!("/{id}/preview"), Some(json!({})))
            .await;
        let doc: TemplateDocument = serde_json::from_value(document).unwrap();
        let hooks = Hooks {
            tracking: Tracking::Inert {
                base_url: "https://preview.sample.invalid/t",
            },
            utm: None,
        };
        let expected = render_email(&doc, &context, &hooks).unwrap();
        assert_eq!(preview.status, StatusCode::OK, "{:?}", preview.body);
        assert_eq!(preview.body["rendered"], true);
        assert_eq!(preview.body["content_type"], "blocks");
        assert_eq!(preview.body["html"], expected.html.as_str());
        assert_eq!(preview.body["text"], expected.text.as_str());
        assert!(expected.html.contains("<!--[if mso"), "{}", expected.html);
        assert!(
            expected
                .text
                .contains("Open: https://shop.example/a?b=1&c=2")
        );

        // Without the unsubscribe link the publish is refused with the same code as for HTML.
        let bad = f
            .create("No footer", blocks_doc("Welcome", "<p>no link</p>"))
            .await;
        let refused = f.lead("POST", &format!("/{bad}/publish"), None).await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            refused.body
        );
        assert_eq!(refused.body["code"], "template_invalid");
        assert!(
            refused.body["template_issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["code"] == "missing_unsubscribe" && issue["at"] == "/blocks"),
            "{:?}",
            refused.body
        );

        // A block the editor does not know is a malformed draft, and the category is the
        // template's own.
        let mut unknown = blocks_doc("Welcome", footer);
        unknown["blocks"]["sections"][0]["columns"][0][0]["type"] = json!("carousel");
        let refused = f.put_draft(&id, unknown).await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(refused.body["code"], "template_unknown_block");
        let mut newer = blocks_doc("Welcome", footer);
        newer["schema"] = json!(3);
        assert_eq!(
            f.put_draft(&id, newer).await.body["code"],
            "template_unsupported_schema"
        );
        let mut extra = blocks_doc("Welcome", footer);
        extra["theme"] = json!("dark");
        assert_eq!(
            f.put_draft(&id, extra).await.body["code"],
            "template_unknown_field"
        );
        // The dummy marketing `email` beside transactional blocks must not slip past the
        // category check.
        let mut both = blocks_doc("Welcome", footer);
        both["blocks"]["category"] = json!("transactional");
        both["email"] = json!({"subject": "x", "html": "", "category": "marketing"});
        assert_eq!(
            f.put_draft(&id, both).await.body["code"],
            "template_malformed"
        );
        let mut transactional = blocks_doc("Welcome", footer);
        transactional["blocks"]["category"] = json!("transactional");
        assert_eq!(
            f.put_draft(&id, transactional).await.body["code"],
            "template_mismatch"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_stored_draft_the_stricter_envelope_refuses_is_a_422_not_a_500(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Newsletter", email_doc("Welcome", "x")).await;
        let mut stale = email_doc("Welcome", "x");
        stale["theme"] = json!("dark");
        sqlx::query("UPDATE template_drafts SET document = $1 WHERE template_id = $2")
            .bind(&stale)
            .bind(Uuid::parse_str(&id).unwrap())
            .execute(&f.pool)
            .await
            .unwrap();

        let preview = f
            .lead("POST", &format!("/{id}/preview"), Some(json!({})))
            .await;
        let publish = f.lead("POST", &format!("/{id}/publish"), None).await;
        let listed = f.lead("GET", "", None).await;

        assert_eq!(preview.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(preview.body["code"], "template_unknown_field");
        assert_eq!(publish.body["code"], "template_unknown_field");
        assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_draft_with_an_unknown_syntax_is_a_422_template_malformed(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Newsletter", email_doc("Hi", "x")).await;
        let mut document = mjml_doc("Welcome", "footer");
        document["email"]["syntax"] = json!("markdown");

        let saved = f.put_draft(&id, document.clone()).await;
        assert_eq!(
            saved.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            saved.body
        );
        assert_eq!(saved.body["code"], "template_malformed");

        let created = f
            .lead(
                "POST",
                "",
                Some(json!({"name": "Other", "channel": "email", "document": document})),
            )
            .await;
        assert_eq!(
            created.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            created.body
        );
        assert_eq!(created.body["code"], "template_malformed");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_rewrites_web_links_to_inert_tokens(pool: PgPool) {
        let f = fixture(&pool).await;
        let mut document = email_doc("Hi", "x");
        document["email"]["html"] = json!(
            "<a href=\"https://shop.example.com/a?x=1\">Shop</a>\
             <a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a>"
        );
        let id = f.create("Welcome", document).await;

        let reply = f
            .lead("POST", &format!("/{id}/preview"), Some(json!({})))
            .await;

        let html = reply.body["html"].as_str().unwrap();
        assert!(
            html.contains("https://preview.sample.invalid/t/inert"),
            "{html}"
        );
        assert!(!html.contains("shop.example.com"), "{html}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_real_person_preview_is_refused_for_write_and_allowed_for_an_admin(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f
            .create(
                "Welcome",
                email_doc(
                    "Hi {{ person.plan }}",
                    "{{ person.email }} on {{ person.plan }}",
                ),
            )
            .await;
        let person = f
            .person(json!({"plan": "enterprise"}), Some("ann@example.com"))
            .await;
        let uri = f.templates(&format!("/{id}/preview"));
        let body = Some(json!({"person_id": person}));

        let member = f
            .call("POST", &uri, body.clone(), Auth::Session(&f.member))
            .await;
        let write_key = f
            .call("POST", &uri, body.clone(), Auth::Key(&f.write))
            .await;
        let admin_key = f
            .call("POST", &uri, body.clone(), Auth::Key(&f.admin))
            .await;
        let lead = f.call("POST", &uri, body, Auth::Session(&f.lead)).await;
        let unknown = f
            .call(
                "POST",
                &uri,
                Some(json!({"person_id": Uuid::now_v7()})),
                Auth::Key(&f.admin),
            )
            .await;
        let sample = f
            .call("POST", &uri, Some(json!({})), Auth::Session(&f.member))
            .await;

        assert_eq!(member.status, StatusCode::FORBIDDEN);
        assert_eq!(member.body["code"], "template_preview_values_forbidden");
        assert_eq!(write_key.status, StatusCode::FORBIDDEN);
        for allowed in [&admin_key, &lead] {
            assert_eq!(allowed.status, StatusCode::OK, "{:?}", allowed.body);
            assert_eq!(allowed.body["person_id"], json!(person));
            assert_eq!(allowed.body["subject"], "Hi enterprise");
            assert!(
                allowed.body["html"]
                    .as_str()
                    .unwrap()
                    .contains("ann@example.com on enterprise")
            );
        }
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        // A Write member may still preview the sample person.
        assert_eq!(sample.status, StatusCode::OK, "{:?}", sample.body);
        assert_eq!(sample.body["person_id"], Value::Null);
        assert_eq!(sample.body["subject"], "Hi sample");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_real_person_preview_is_audited_with_the_ids_and_no_values(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f
            .create(
                "Welcome",
                email_doc("Hi {{ person.plan }}", "{{ person.email }}"),
            )
            .await;
        let person = f
            .person(json!({"plan": "enterprise"}), Some("ann@example.com"))
            .await;
        let sample = f
            .lead("POST", &format!("/{id}/preview"), Some(json!({})))
            .await;
        assert_eq!(sample.status, StatusCode::OK);
        assert_eq!(
            f.preview_audits().await,
            0,
            "a sample preview shows no one's values"
        );

        let real = f
            .lead(
                "POST",
                &format!("/{id}/preview"),
                Some(json!({"person_id": person})),
            )
            .await;

        assert_eq!(real.status, StatusCode::OK, "{:?}", real.body);
        assert_eq!(f.preview_audits().await, 1);
        let row: (String, Option<Uuid>, String) = sqlx::query_as(
            "SELECT object_kind, object_id, summary::text FROM audit_log \
             WHERE action = 'template.previewed'",
        )
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(
            (row.0.as_str(), row.1),
            ("template", Some(id.parse().unwrap()))
        );
        assert!(row.2.contains(&person.to_string()));
        for value in ["enterprise", "ann@example.com"] {
            assert!(!row.2.contains(value), "{}", row.2);
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_preview_people_routes_are_for_admins_only(pool: PgPool) {
        let f = fixture(&pool).await;
        let template = f.create("Welcome", email_doc("Hi", "x")).await;
        let person = f.person(json!({}), Some("ann@example.com")).await;
        let tail = format!("/{template}/preview-people");
        let routes: [(&str, String, Option<Value>); 3] = [
            ("GET", tail.clone(), None),
            ("POST", tail.clone(), Some(json!({"person_id": person}))),
            ("DELETE", format!("{tail}/{person}"), None),
        ];
        let refused: [(&str, Auth<'_>, StatusCode); 5] = [
            ("anonymous", Auth::Anonymous, StatusCode::UNAUTHORIZED),
            ("read key", Auth::Key(&f.read), StatusCode::FORBIDDEN),
            ("write key", Auth::Key(&f.write), StatusCode::FORBIDDEN),
            ("member", Auth::Session(&f.member), StatusCode::FORBIDDEN),
            (
                "other workspace's admin key",
                Auth::Key(&f.other_admin),
                StatusCode::NOT_FOUND,
            ),
        ];

        for (method, uri, body) in &routes {
            for (who, auth, status) in &refused {
                let auth = match auth {
                    Auth::Anonymous => Auth::Anonymous,
                    Auth::Key(key) => Auth::Key(key),
                    Auth::Session(login) => Auth::Session(login),
                };
                let reply = f.call(method, &f.templates(uri), body.clone(), auth).await;
                assert_eq!(reply.status, *status, "{method} {uri} as {who}");
            }
        }
        assert_eq!(
            f.count("SELECT count(*) FROM template_preview_people")
                .await,
            0
        );

        let key = f
            .call(
                "POST",
                &f.templates(&tail),
                Some(json!({"person_id": person})),
                Auth::Key(&f.admin),
            )
            .await;
        assert_eq!(key.status, StatusCode::CREATED, "{:?}", key.body);
        let listed = f.lead("GET", &tail, None).await;
        assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.body);
        assert_eq!(listed.body[0]["person_id"], json!(person));
        assert_eq!(listed.body[0]["display"]["value"], "ann@example.com");
        assert_eq!(listed.body[0]["display"]["masked"], false);
        let removed = f.lead("DELETE", &format!("{tail}/{person}"), None).await;
        assert_eq!(removed.status, StatusCode::NO_CONTENT);
        let again = f.lead("DELETE", &format!("{tail}/{person}"), None).await;
        assert_eq!(again.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unknown_template_or_person_is_a_404_and_the_same_person_twice_is_one_row(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let template = f.create("Welcome", email_doc("Hi", "x")).await;
        let person = f.person(json!({}), None).await;
        let tail = format!("/{template}/preview-people");
        let body = Some(json!({"person_id": person}));

        let unknown_person = f
            .lead("POST", &tail, Some(json!({"person_id": Uuid::now_v7()})))
            .await;
        let unknown_template = f
            .lead(
                "POST",
                &format!("/{}/preview-people", Uuid::now_v7()),
                body.clone(),
            )
            .await;
        let list_unknown = f
            .lead("GET", &format!("/{}/preview-people", Uuid::now_v7()), None)
            .await;
        let first = f.lead("POST", &tail, body.clone()).await;
        let second = f.lead("POST", &tail, body).await;

        assert_eq!(unknown_person.status, StatusCode::NOT_FOUND);
        assert_eq!(unknown_template.status, StatusCode::NOT_FOUND);
        assert_eq!(list_unknown.status, StatusCode::NOT_FOUND);
        assert_eq!(first.status, StatusCode::CREATED, "{:?}", first.body);
        assert_eq!(second.status, StatusCode::OK, "{:?}", second.body);
        assert_eq!(first.body, second.body);
        assert_eq!(
            f.count("SELECT count(*) FROM template_preview_people")
                .await,
            1
        );
        assert_eq!(
            f.count(
                "SELECT count(*) FROM audit_log WHERE action = 'template.preview_person_added'"
            )
            .await,
            1,
            "only the new row is audited"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_twenty_first_preview_person_is_refused_and_the_audit_holds_ids_only(pool: PgPool) {
        let f = fixture(&pool).await;
        let template = f.create("Welcome", email_doc("Hi", "x")).await;
        let tail = format!("/{template}/preview-people");
        let mut last = Uuid::nil();
        for index in 0..20 {
            last = f
                .person(json!({}), Some(&format!("p{index}@example.com")))
                .await;
            let added = f
                .lead("POST", &tail, Some(json!({"person_id": last})))
                .await;
            assert_eq!(added.status, StatusCode::CREATED, "{:?}", added.body);
        }
        let extra = f.person(json!({}), Some("extra@example.com")).await;

        let over = f
            .lead("POST", &tail, Some(json!({"person_id": extra})))
            .await;
        let removed = f.lead("DELETE", &format!("{tail}/{last}"), None).await;

        assert_eq!(over.status, StatusCode::CONFLICT, "{:?}", over.body);
        assert_eq!(over.body["code"], "preview_people_limit");
        assert_eq!(removed.status, StatusCode::NO_CONTENT);
        let audits: Vec<(String, Option<Uuid>, String)> = sqlx::query_as(
            "SELECT action, object_id, summary::text FROM audit_log \
             WHERE action LIKE 'template.preview_person_%' ORDER BY id",
        )
        .fetch_all(&f.pool)
        .await
        .unwrap();
        assert_eq!(audits.len(), 21);
        assert_eq!(audits[20].0, "template.preview_person_removed");
        for (_, object, summary) in &audits {
            assert_eq!(*object, Some(template.parse().unwrap()));
            assert!(!summary.contains("example.com"), "{summary}");
            assert!(summary.contains("person_id"), "{summary}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_reports_blocking_issues_and_renders_nothing(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f
            .create("Welcome", email_doc("Hi {{ person.nope }}", "x"))
            .await;

        let reply = f
            .lead("POST", &format!("/{id}/preview"), Some(json!({})))
            .await;

        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        assert_eq!(reply.body["rendered"], false);
        assert_eq!(reply.body["html"], Value::Null);
        assert!(
            reply.body["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["code"] == "unknown_merge_field")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_can_read_a_published_version_instead_of_the_draft(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", email_doc("Published", "x")).await;
        f.lead("POST", &format!("/{id}/publish"), None).await;
        f.lead(
            "PUT",
            &format!("/{id}/draft"),
            Some(json!({"document": email_doc("Draft", "y")})),
        )
        .await;

        let draft = f
            .lead("POST", &format!("/{id}/preview"), Some(json!({})))
            .await;
        let version = f
            .lead(
                "POST",
                &format!("/{id}/preview"),
                Some(json!({"version": 1})),
            )
            .await;
        let missing = f
            .lead(
                "POST",
                &format!("/{id}/preview"),
                Some(json!({"version": 9})),
            )
            .await;

        assert_eq!(
            (&draft.body["source"], &draft.body["subject"]),
            (&json!("draft"), &json!("Draft"))
        );
        assert_eq!(version.body["source"], "version");
        assert_eq!(version.body["version"], 1);
        assert_eq!(version.body["subject"], "Published");
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
    }

    fn assert_preview_headers(reply: &Reply) {
        assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(reply.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert!(!reply.headers.contains_key(header::SET_COOKIE));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn previews_are_not_cached_or_sniffed_and_set_no_cookie(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Headers", email_doc("Hi", "x")).await;
        let uri = f.templates(&format!("/{id}/preview"));
        for auth in [Auth::Session(&f.lead), Auth::Key(&f.write)] {
            let reply = f.call("POST", &uri, Some(json!({})), auth).await;
            assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
            assert_eq!(reply.body["rendered"], true);
            assert_preview_headers(&reply);
        }
        let refused = f.call("POST", &uri, Some(json!({})), Auth::Anonymous).await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
        assert_preview_headers(&refused);

        f.put_draft(&id, email_doc("Hi", "{{ person.nope }}")).await;
        let blocked = f
            .lead("POST", &format!("/{id}/preview"), Some(json!({})))
            .await;
        assert_eq!(blocked.status, StatusCode::OK);
        assert_eq!(blocked.body["rendered"], false);
        assert_preview_headers(&blocked);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_that_doubles_a_string_hits_the_os_limit_and_the_next_preview_succeeds(
        pool: PgPool,
    ) {
        if !cfg!(target_os = "linux") {
            eprintln!("skipped: the OS memory limit is applied on Linux only");
            return;
        }
        let render_pool = start_pool("memory_limit", Duration::from_secs(60)).await;
        let f = fixture_with(&pool, Some(render_pool)).await;
        let doubling = "{% set ns = namespace(s='xxxxxxxx') %}\
            {% for i in range(60) %}{% set ns.s = ns.s ~ ns.s %}{% endfor %}{{ ns.s | length }}";
        let hostile = f.create("Memory limit", email_doc("Hi", doubling)).await;
        let ordinary = f.create("Recovery", email_doc("Hi", "next preview")).await;
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let failed = f
            .lead("POST", &format!("/{hostile}/preview"), Some(json!({})))
            .await;
        assert_eq!(
            failed.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            failed.body
        );
        assert_eq!(
            failed.headers[header::CONTENT_TYPE],
            "application/problem+json"
        );
        assert_eq!(failed.body["code"], "render_memory_limit");
        assert_preview_headers(&failed);

        let next = f
            .lead("POST", &format!("/{ordinary}/preview"), Some(json!({})))
            .await;
        assert_eq!(next.status, StatusCode::OK, "{:?}", next.body);
        assert_eq!(next.body["rendered"], true);
        assert_eq!(next.body["subject"], "Hi");
        assert!(next.body["html"].as_str().unwrap().contains("next preview"));
        assert_preview_headers(&next);
        let metrics = handle.render();
        assert_eq!(
            metrics
                .lines()
                .find(|line| line.starts_with("zirv_flow_render_restarts_total ")),
            Some("zirv_flow_render_restarts_total 1"),
            "{metrics}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_render_that_misses_its_deadline_is_a_problem_and_the_next_preview_works(
        pool: PgPool,
    ) {
        let render_pool = start_pool("hang_first", Duration::from_millis(1500)).await;
        let f = fixture_with(&pool, Some(render_pool)).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;
        let uri = format!("/{id}/preview");

        let stalled = f.lead("POST", &uri, Some(json!({}))).await;
        let next = f.lead("POST", &uri, Some(json!({}))).await;

        assert_eq!(
            stalled.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            stalled.body
        );
        assert_eq!(stalled.body["code"], "render_deadline");
        assert_eq!(next.status, StatusCode::OK, "{:?}", next.body);
        assert_eq!(next.body["subject"], "Hi");
        // The API stayed up: it still answers other routes.
        assert_eq!(f.lead("GET", "", None).await.status, StatusCode::OK);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_validation_that_misses_its_deadline_fails_the_publish_and_the_next_publish_works(
        pool: PgPool,
    ) {
        let render_pool = start_pool("hang_first", Duration::from_millis(1500)).await;
        let f = fixture_with(&pool, Some(render_pool)).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;
        let uri = format!("/{id}/publish");

        let stalled = f.lead("POST", &uri, None).await;
        let versions_after_stall = f.versions_written().await;
        let next = f.lead("POST", &uri, None).await;

        assert_eq!(
            stalled.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            stalled.body
        );
        assert_eq!(stalled.body["code"], "render_deadline");
        assert_eq!(versions_after_stall, 0);
        assert_eq!(next.status, StatusCode::CREATED, "{:?}", next.body);
        assert_eq!(f.versions_written().await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_draft_saved_while_a_publish_validates_is_a_409_and_writes_no_version(pool: PgPool) {
        let (render_pool, signal) =
            start_signalling_pool("slow_first", Duration::from_secs(30)).await;
        let f = fixture_with(&pool, Some(render_pool)).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;

        let publish_tail = format!("/{id}/publish");
        let publish = f.lead("POST", &publish_tail, None);
        // The save goes in only once the validate request is at the child, so the publish has
        // already read the draft it is validating.
        let edit = async {
            request_reached_a_child(&signal).await;
            f.put_draft(&id, email_doc("Changed", "y")).await
        };
        let (published, edited) = tokio::join!(publish, edit);

        assert_eq!(edited.status, StatusCode::OK, "{:?}", edited.body);
        assert_eq!(
            published.status,
            StatusCode::CONFLICT,
            "{:?}",
            published.body
        );
        assert_eq!(published.body["code"], "template_draft_changed");
        assert_eq!(f.versions_written().await, 0);
        // Publishing again validates the new draft and succeeds.
        let again = f.lead("POST", &format!("/{id}/publish"), None).await;
        assert_eq!(again.status, StatusCode::CREATED, "{:?}", again.body);
        assert_eq!(again.body["version"]["document"], email_doc("Changed", "y"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_draft_save_is_not_blocked_while_a_publish_waits_on_a_slow_validate(pool: PgPool) {
        let (render_pool, signal) =
            start_signalling_pool("hang_first", Duration::from_secs(15)).await;
        let f = fixture_with(&pool, Some(render_pool)).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;

        let publish_tail = format!("/{id}/publish");
        let mut publish = std::pin::pin!(f.lead("POST", &publish_tail, None));
        let mut edit = std::pin::pin!(async {
            request_reached_a_child(&signal).await;
            f.put_draft(&id, email_doc("Edited", "y")).await
        });

        // The save must finish while the publish is still waiting on the stuck child: if it
        // were queued behind a lock the publish holds, the publish would end first.
        let edited = tokio::select! {
            edited = &mut edit => edited,
            published = &mut publish => {
                panic!("the publish ended before the save did: {:?}", published.body)
            }
        };
        let published = publish.await;

        assert_eq!(edited.status, StatusCode::OK, "{:?}", edited.body);
        assert_eq!(
            published.body["code"], "render_deadline",
            "{:?}",
            published.body
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_a_render_pool_a_publish_is_unavailable_and_validates_nothing_here(
        pool: PgPool,
    ) {
        let f = fixture_with(&pool, None).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;

        let reply = f.lead("POST", &format!("/{id}/publish"), None).await;

        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.body["code"], "render_unavailable");
        assert_eq!(f.versions_written().await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_template_the_renderer_refuses_is_a_422_with_the_renderers_code(pool: PgPool) {
        let f = fixture(&pool).await;
        // `person.nick` is not in the registry, so validation reports it; a real person without
        // the trait makes a known field undefined at render time instead.
        let id = f
            .create("Welcome", email_doc("Hi {{ person.plan }}", "x"))
            .await;
        let person = f.person(json!({}), Some("ann@example.com")).await;

        let reply = f
            .lead(
                "POST",
                &format!("/{id}/preview"),
                Some(json!({"person_id": person})),
            )
            .await;

        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            reply.body
        );
        assert_eq!(reply.body["code"], "template_render_failed");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_a_render_pool_a_preview_is_unavailable_and_nothing_renders_here(pool: PgPool) {
        let f = fixture_with(&pool, None).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;

        let reply = f
            .lead("POST", &format!("/{id}/preview"), Some(json!({})))
            .await;

        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.body["code"], "render_unavailable");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn where_used_lists_a_flow_version_that_names_the_template(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;
        let template: Uuid = id.parse().unwrap();
        // No node type names a template yet, so the fixture is a stored graph that does.
        let flow = Uuid::now_v7();
        let graph = json!({"v": 1, "nodes": [
            {"id": "send", "type": "action.send", "template": id},
            {"id": "end", "type": "exit"}], "edges": []});
        let settings = json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}});
        let mut connection = f.pool.acquire().await.unwrap();
        zirv_flow_store::create_flow(
            &mut connection,
            f.ws,
            &zirv_flow_store::NewFlow {
                id: flow,
                name: "Welcome flow".into(),
                graph: graph.clone(),
                settings,
            },
            None,
        )
        .await
        .unwrap();
        drop(connection);
        let mut tx = zirv_flow_store::begin(&f.pool).await.unwrap();
        let locked = zirv_flow_store::lock_flow(&mut tx, f.ws, flow)
            .await
            .unwrap();
        zirv_flow_store::publish_flow(
            &mut tx,
            &zirv_flow_store::Clock::default(),
            f.ws,
            &locked,
            None,
            &zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t")),
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();

        let used = f.lead("GET", &format!("/{id}/where-used"), None).await;
        let other = f.create("Unused", email_doc("Hi", "y")).await;
        let unused = f.lead("GET", &format!("/{other}/where-used"), None).await;

        assert_eq!(used.status, StatusCode::OK, "{:?}", used.body);
        let rows = used.body.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["flow_id"], json!(flow));
        assert_eq!(rows[0]["flow_name"], "Welcome flow");
        assert_eq!(rows[0]["versions"], json!([1]));
        assert_eq!(rows[0]["active"], true);
        assert!(unused.body.as_array().unwrap().is_empty());
        let _ = template;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_utm_override_is_set_by_the_template_update_returned_and_refused_when_unknown(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;
        let uri = f.templates(&format!("/{id}"));

        let created = f.lead("GET", &format!("/{id}"), None).await;
        let by_read = f
            .call("PUT", &uri, Some(json!({"utm": "off"})), Auth::Key(&f.read))
            .await;
        let off = f
            .lead("PUT", &format!("/{id}"), Some(json!({"utm": "off"})))
            .await;
        let read_back = f.lead("GET", &format!("/{id}"), None).await;
        let bad = f
            .lead("PUT", &format!("/{id}"), Some(json!({"utm": "on"})))
            .await;
        let inherit = f
            .lead("PUT", &format!("/{id}"), Some(json!({"utm": "inherit"})))
            .await;

        assert_eq!(created.body["utm"], "inherit");
        assert_eq!(by_read.status, StatusCode::FORBIDDEN);
        assert_eq!(off.status, StatusCode::OK, "{:?}", off.body);
        assert_eq!(off.body["utm"], "off");
        assert_eq!(read_back.body["utm"], "off");
        assert!(bad.status.is_client_error());
        assert_eq!(inherit.body["utm"], "inherit");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_shows_the_tagged_links_unless_the_setting_or_the_template_is_off(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let mut document = email_doc("Hi", "x");
        document["email"]["html"] = json!(
            "<a href=\"https://shop.example.com/a?x=1\">Shop</a>\
             <a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a>"
        );
        // Clicks are inert in a preview, so the tagged URL shows in the text part.
        document["email"]["text"] = json!("Shop https://shop.example.com/a?x=1 end");
        let id = f.create("Welcome", document).await;
        let ws_uri = format!("/api/v1/workspaces/{}", f.ws.as_uuid());
        let preview = || async {
            f.lead("POST", &format!("/{id}/preview"), Some(json!({})))
                .await
        };

        let setting_off = preview().await;
        let enabled = f
            .call(
                "PATCH",
                &ws_uri,
                Some(json!({"utm": {
                    "enabled": true, "source": "{workspace}", "medium": "email",
                    "campaign": "{flow}", "content": "{template}",
                    "domains": ["example.com"],
                }})),
                Auth::Session(&f.lead),
            )
            .await;
        assert_eq!(enabled.status, StatusCode::OK, "{:?}", enabled.body);
        let on = preview().await;
        let put = f
            .lead("PUT", &format!("/{id}"), Some(json!({"utm": "off"})))
            .await;
        assert_eq!(put.status, StatusCode::OK);
        let template_off = preview().await;

        let text = |reply: &Reply| reply.body["text"].as_str().unwrap().to_owned();
        assert_eq!(
            text(&setting_off),
            "Shop https://shop.example.com/a?x=1 end"
        );
        let tagged = text(&on);
        // {flow} has no value in a preview, so utm_campaign is left out.
        assert!(
            tagged.starts_with("Shop https://shop.example.com/a?x=1&utm_source="),
            "{tagged}"
        );
        assert!(
            tagged.contains("&utm_medium=email&utm_content=welcome end"),
            "{tagged}"
        );
        assert!(!tagged.contains("utm_campaign"), "{tagged}");
        assert_eq!(setting_off.body, template_off.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn mutations_are_audited_without_the_document(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f
            .create("Welcome", email_doc("Secret subject", "Secret body"))
            .await;
        f.lead("PUT", &format!("/{id}"), Some(json!({"name": "Renamed"})))
            .await;
        f.lead(
            "PUT",
            &format!("/{id}/draft"),
            Some(json!({"document": email_doc("Secret subject", "Secret body 2")})),
        )
        .await;
        f.lead("POST", &format!("/{id}/publish"), None).await;
        f.lead("DELETE", &format!("/{id}"), None).await;

        let entries: Vec<(String, String)> = sqlx::query_as(
            "SELECT action, summary::text FROM audit_log WHERE action LIKE 'template.%' ORDER BY id",
        )
        .fetch_all(&f.pool)
        .await
        .unwrap();

        let actions: Vec<&str> = entries.iter().map(|(action, _)| action.as_str()).collect();
        assert_eq!(
            actions,
            [
                "template.created",
                "template.updated",
                "template.draft_saved",
                "template.published",
                "template.archived"
            ]
        );
        assert!(
            entries
                .iter()
                .all(|(_, summary)| !summary.contains("Secret"))
        );
    }

    /// Publishes a flow whose graph sends `template` and returns its id.
    async fn publish_flow_sending(f: &Fixture, template: &str, name: &str) -> Uuid {
        let flow = Uuid::now_v7();
        let graph = json!({"v": 1, "nodes": [
            {"id": "send", "type": "action.send", "template": template},
            {"id": "end", "type": "exit"}], "edges": []});
        let settings = json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}});
        let mut connection = f.pool.acquire().await.unwrap();
        zirv_flow_store::create_flow(
            &mut connection,
            f.ws,
            &zirv_flow_store::NewFlow {
                id: flow,
                name: name.into(),
                graph,
                settings,
            },
            None,
        )
        .await
        .unwrap();
        drop(connection);
        let mut tx = zirv_flow_store::begin(&f.pool).await.unwrap();
        let locked = zirv_flow_store::lock_flow(&mut tx, f.ws, flow)
            .await
            .unwrap();
        zirv_flow_store::publish_flow(
            &mut tx,
            &zirv_flow_store::Clock::default(),
            f.ws,
            &locked,
            None,
            &zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t")),
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        flow
    }

    fn names_of(reply: &Reply) -> Vec<String> {
        reply
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap().to_owned())
            .collect()
    }

    fn no_sender_doc() -> Value {
        let mut doc = email_doc("Hi", "x");
        doc["email"]["sender"] = json!("");
        doc
    }

    async fn stored_checks(pool: &PgPool, id: &str) -> Option<Value> {
        sqlx::query_scalar("SELECT checks FROM template_drafts WHERE template_id = $1")
            .bind(id.parse::<Uuid>().unwrap())
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_list_filters_sorts_and_hides_archived_templates_by_default(pool: PgPool) {
        let f = fixture(&pool).await;
        let live = f.create("Live one", email_doc("Hi", "x")).await;
        let changed = f.create("Changed", email_doc("Hi", "x")).await;
        f.create("Fresh", email_doc("Spring", "x")).await;
        f.create("Hook", webhook_doc("{}")).await;
        let gone = f.create("Gone", email_doc("Hi", "x")).await;
        for id in [&live, &changed] {
            let published = f.lead("POST", &format!("/{id}/publish"), None).await;
            assert_eq!(
                published.status,
                StatusCode::CREATED,
                "{:?}",
                published.body
            );
        }
        f.put_draft(&changed, email_doc("Hi", "edited")).await;
        f.lead("DELETE", &format!("/{gone}"), None).await;
        let names = |tail: &'static str| {
            let f = &f;
            async move { names_of(&f.lead("GET", tail, None).await) }
        };

        assert_eq!(
            names("?sort=name").await,
            ["Changed", "Fresh", "Hook", "Live one"]
        );
        assert_eq!(names("?status=live").await, ["Live one"]);
        assert_eq!(names("?status=changes").await, ["Changed"]);
        assert_eq!(names("?status=draft&sort=name").await, ["Fresh", "Hook"]);
        assert_eq!(names("?status=archived").await, ["Gone"]);
        assert_eq!(names("?archived=true").await.len(), 5);
        assert_eq!(names("?channel=webhook").await, ["Hook"]);
        assert_eq!(names("?q=spring").await, ["Fresh"]);
        assert_eq!(names("?q=LIVE").await, ["Live one"]);
        assert_eq!(names("?theme=none&channel=webhook").await, ["Hook"]);
        assert!(names("?theme=default").await.is_empty());
        assert_eq!(names("").await[0], "Changed");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unknown_list_parameter_value_is_a_400_naming_the_parameter(pool: PgPool) {
        let f = fixture(&pool).await;

        for (tail, name) in [
            ("?status=old", "status"),
            ("?channel=sms", "channel"),
            ("?sort=popular", "sort"),
            ("?theme=blue", "theme"),
            ("?archived=maybe", "archived"),
        ] {
            let reply = f.lead("GET", tail, None).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{tail}");
            assert!(
                reply.body["detail"].as_str().unwrap().contains(name),
                "{tail}: {:?}",
                reply.body
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_list_items_carry_the_draft_state_checks_flows_and_editor(pool: PgPool) {
        let f = fixture(&pool).await;
        let live = f.create("Live", email_doc("Hi", "x")).await;
        let changed = f.create("Changed", email_doc("Hi", "x")).await;
        let draft_only = f.create("Draft only", email_doc("Hi", "x")).await;
        let by_key = f
            .call(
                "POST",
                &f.templates(""),
                Some(json!({"name": "Keyed", "channel": "email", "document": no_sender_doc()})),
                Auth::Key(&f.write),
            )
            .await;
        assert_eq!(by_key.status, StatusCode::CREATED, "{:?}", by_key.body);
        for id in [&live, &changed] {
            f.lead("POST", &format!("/{id}/publish"), None).await;
        }
        f.put_draft(&changed, email_doc("Hi", "edited")).await;
        f.put_draft(&live, email_doc("Hi", "x")).await;
        publish_flow_sending(&f, &live, "Welcome flow").await;
        let item = |items: &Reply, name: &str| -> Value {
            items
                .body
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["name"] == name)
                .unwrap()
                .clone()
        };

        let items = f.lead("GET", "", None).await;

        let live_item = item(&items, "Live");
        assert_eq!(live_item["draft_changed"], false);
        assert_eq!(live_item["active_flows"], 1);
        assert_eq!(live_item["theme"], "none");
        assert_eq!(live_item["edited_by_email"], "lead@example.com");
        assert!(live_item["edited_at"].is_string());
        assert_eq!(live_item["checks"]["blocking"], 0);
        assert_eq!(item(&items, "Changed")["draft_changed"], true);
        assert_eq!(item(&items, "Changed")["active_flows"], 0);
        assert_eq!(item(&items, "Draft only")["draft_changed"], false);
        assert_eq!(item(&items, "Draft only")["current_version"], Value::Null);
        assert_eq!(item(&items, "Draft only")["checks"], Value::Null);
        assert_eq!(item(&items, "Keyed")["edited_by_email"], Value::Null);
        let saved = f.put_draft(&draft_only, no_sender_doc()).await;
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        let after = f.lead("GET", "?q=draft", None).await;
        assert!(
            item(&after, "Draft only")["checks"]["blocking"]
                .as_i64()
                .unwrap()
                >= 1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_block_template_lists_the_theme_it_follows(pool: PgPool) {
        let f = fixture(&pool).await;
        let theme = zirv_flow_store::create_theme(
            &mut f.pool.acquire().await.unwrap(),
            f.ws,
            &zirv_flow_store::NewTheme {
                id: Uuid::now_v7(),
                name: "Brand".into(),
                tokens: json!({"primary": "#112233", "text": "#222222", "background": "#ffffff"}),
            },
            None,
        )
        .await
        .unwrap()
        .0;
        let doc = |theme: Value| {
            json!({"schema": 2, "channel": "email", "mode": "blocks",
                   "blocks": {"subject": "Hi", "sender": SENDER, "lang": "en",
                              "category": "marketing", "theme": theme}})
        };
        f.create("Named", doc(json!({"id": theme.id}))).await;
        f.create("Following", doc(json!("default"))).await;

        let by_theme = f.lead("GET", &format!("?theme={}", theme.id), None).await;
        let following = f.lead("GET", "?theme=default", None).await;

        assert_eq!(names_of(&by_theme), ["Named"]);
        assert_eq!(
            by_theme.body[0]["theme"],
            json!({"id": theme.id, "name": "Brand"})
        );
        assert_eq!(names_of(&following), ["Following"]);
        assert_eq!(following.body[0]["theme"], "default");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_draft_save_stores_the_checks_and_a_failed_validation_still_saves(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;

        let saved = f.put_draft(&id, no_sender_doc()).await;

        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert_eq!(saved.body["draft"]["revision"], 2);
        let stored = stored_checks(&pool, &id).await.unwrap();
        assert!(stored["blocking"].as_i64().unwrap() >= 1);
        assert!(stored["warnings"].is_number());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_save_whose_validation_cannot_run_still_saves_and_leaves_the_checks_empty(
        pool: PgPool,
    ) {
        let f = fixture_with(&pool, None).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;

        let saved = f.put_draft(&id, email_doc("Hi", "y")).await;

        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert_eq!(saved.body["draft"]["revision"], 2);
        assert_eq!(stored_checks(&pool, &id).await, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_template_an_active_flow_sends_cannot_be_archived_until_the_flow_is_paused(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;
        let running = publish_flow_sending(&f, &id, "Running flow").await;
        let paused = publish_flow_sending(&f, &id, "Paused flow").await;
        sqlx::query("UPDATE flows SET state = 'paused' WHERE id = $1")
            .bind(paused)
            .execute(&f.pool)
            .await
            .unwrap();

        let refused = f.lead("DELETE", &format!("/{id}"), None).await;

        assert_eq!(refused.status, StatusCode::CONFLICT, "{:?}", refused.body);
        assert_eq!(refused.body["code"], "template_in_use");
        assert_eq!(
            refused.body["flows"],
            json!([{"flow_id": running, "flow_name": "Running flow"}])
        );
        let archived: bool =
            sqlx::query_scalar("SELECT archived_at IS NOT NULL FROM templates WHERE id = $1")
                .bind(id.parse::<Uuid>().unwrap())
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert!(!archived);
        sqlx::query("UPDATE flows SET state = 'paused' WHERE id = $1")
            .bind(running)
            .execute(&f.pool)
            .await
            .unwrap();
        let allowed = f.lead("DELETE", &format!("/{id}"), None).await;
        assert_eq!(allowed.status, StatusCode::OK, "{:?}", allowed.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_template_only_a_draft_flow_names_can_be_archived(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;
        let mut connection = f.pool.acquire().await.unwrap();
        zirv_flow_store::create_flow(
            &mut connection,
            f.ws,
            &zirv_flow_store::NewFlow {
                id: Uuid::now_v7(),
                name: "Draft flow".into(),
                graph: json!({"v": 1, "nodes": [
                    {"id": "send", "type": "action.send", "template": id},
                    {"id": "end", "type": "exit"}], "edges": []}),
                settings: json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}}),
            },
            None,
        )
        .await
        .unwrap();
        drop(connection);

        let archived = f.lead("DELETE", &format!("/{id}"), None).await;

        assert_eq!(archived.status, StatusCode::OK, "{:?}", archived.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_stored_checks_resolve_snippets_and_the_theme_footer_as_a_publish_does(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let linked = f
            .snippet(
                "Legal",
                json!([{"type": "text", "id": "t", "content": LINK}]),
            )
            .await;
        let theme = f
            .theme(
                "Brand",
                json!({"primary": "#112233", "footer_snippet": linked}),
            )
            .await;
        let mut doc = blocks_doc("Hi", "<p>bye</p>");
        doc["blocks"]["sections"][1]["columns"][0]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "snippet", "id": "legal2", "snippet_id": linked}));
        doc["blocks"]["theme"] = json!({"id": theme});
        let id = f.create("Resolved", blocks_doc("Hi", LINK)).await;

        let saved = f.put_draft(&id, doc).await;
        let stored = stored_checks(&pool, &id).await.unwrap();
        let published = f.lead("POST", &format!("/{id}/publish"), None).await;

        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        let warnings: i64 = f
            .count("SELECT jsonb_array_length(validation)::bigint FROM template_versions")
            .await;
        assert_eq!(stored["blocking"], 0, "{stored}");
        assert_eq!(stored["warnings"], warnings);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_broken_snippet_reference_stores_a_blocking_count_and_publish_refuses_it(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let mut doc = blocks_doc("Hi", LINK);
        doc["blocks"]["sections"][1]["columns"][0]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "snippet", "id": "gone", "snippet_id": Uuid::now_v7()}));
        let id = f.create("Broken", blocks_doc("Hi", LINK)).await;

        let saved = f.put_draft(&id, doc).await;
        let stored = stored_checks(&pool, &id).await.unwrap();
        let published = f.lead("POST", &format!("/{id}/publish"), None).await;

        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert!(stored["blocking"].as_i64().unwrap() >= 1, "{stored}");
        assert_eq!(published.status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_publish_note_is_stored_and_listed_and_501_characters_are_a_400_naming_note(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;

        let too_long = f
            .lead(
                "POST",
                &format!("/{id}/publish"),
                Some(json!({"note": "x".repeat(501)})),
            )
            .await;
        let published = f
            .lead(
                "POST",
                &format!("/{id}/publish"),
                Some(json!({"note": "  Fixed the footer  "})),
            )
            .await;
        let list = f.lead("GET", &format!("/{id}/versions"), None).await;
        let one = f.lead("GET", &format!("/{id}/versions/1"), None).await;

        assert_eq!(
            too_long.status,
            StatusCode::BAD_REQUEST,
            "{:?}",
            too_long.body
        );
        assert_eq!(too_long.body["errors"][0]["field"], "note");
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        assert_eq!(published.body["version"]["version"], 1);
        assert_eq!(published.body["version"]["note"], "Fixed the footer");
        assert_eq!(list.body[0]["note"], "Fixed the footer");
        assert_eq!(one.body["note"], "Fixed the footer");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_marks_blocks_only_when_asked(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Promo", blocks_doc("Hi", LINK)).await;

        let plain = f.preview_of(&id, json!({})).await;
        let marked = f.preview_of(&id, json!({"annotate": true})).await;

        let html = |reply: &Reply| reply.body["html"].as_str().unwrap().to_owned();
        assert!(!html(&plain).contains("zf-block-"));
        for class in [
            "zf-block-intro",
            "zf-block-title",
            "zf-block-cta",
            "zf-block-legal",
        ] {
            assert!(html(&marked).contains(class), "{class}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn send_volume_lists_an_active_flow_with_its_daily_average_for_a_reader(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;
        let flow = Uuid::now_v7();
        let graph = json!({"v": 1, "nodes": [
            {"id": "send", "type": "action.send", "template": id},
            {"id": "end", "type": "exit"}], "edges": []});
        let settings = json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}});
        let mut connection = f.pool.acquire().await.unwrap();
        zirv_flow_store::create_flow(
            &mut connection,
            f.ws,
            &zirv_flow_store::NewFlow {
                id: flow,
                name: "Welcome flow".into(),
                graph,
                settings,
            },
            None,
        )
        .await
        .unwrap();
        drop(connection);
        let mut tx = zirv_flow_store::begin(&f.pool).await.unwrap();
        let locked = zirv_flow_store::lock_flow(&mut tx, f.ws, flow)
            .await
            .unwrap();
        zirv_flow_store::publish_flow(
            &mut tx,
            &zirv_flow_store::Clock::default(),
            f.ws,
            &locked,
            None,
            &zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t")),
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let person = f.person(json!({}), None).await;
        for test in [false, false, false, false, false, false, false, true] {
            sqlx::query(
                "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, flow_id, \
                    node_id, template_id, destination_id, status, test, completed_at) \
                 VALUES (gen_random_uuid(), $1, $2, gen_random_uuid(), $3, 'send', $4, \
                    gen_random_uuid(), 'sent', $5, now())",
            )
            .bind(f.ws.as_uuid())
            .bind(person)
            .bind(flow)
            .bind(id.parse::<Uuid>().unwrap())
            .bind(test)
            .execute(&f.pool)
            .await
            .unwrap();
        }

        let by_reader = f
            .call(
                "GET",
                &f.templates(&format!("/{id}/send-volume")),
                None,
                Auth::Key(&f.read),
            )
            .await;
        let missing = f
            .lead("GET", &format!("/{}/send-volume", Uuid::now_v7()), None)
            .await;

        assert_eq!(by_reader.status, StatusCode::OK, "{:?}", by_reader.body);
        assert_eq!(
            by_reader.body,
            json!([{"flow_id": flow, "flow_name": "Welcome flow", "per_day": 1.0}])
        );
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn version_stats_count_a_versions_sends_and_are_a_404_for_the_unknown(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;
        f.lead("POST", &format!("/{id}/publish"), None).await;
        let person = f.person(json!({}), None).await;
        let theme = Uuid::now_v7();
        let message = |test: bool, snippets: Value| {
            let (ws, pool) = (f.ws, f.pool.clone());
            let template: Uuid = id.parse().unwrap();
            async move {
                sqlx::query(
                    "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, flow_id, \
                        node_id, template_id, template_version, destination_id, status, test, \
                        theme_id, theme_version, snippet_versions, created_at, completed_at) \
                     VALUES (gen_random_uuid(), $1, $2, gen_random_uuid(), gen_random_uuid(), 'n', \
                        $3, 1, gen_random_uuid(), 'sent', $4, $5, 2, $6, now(), now())",
                )
                .bind(ws.as_uuid())
                .bind(person)
                .bind(template)
                .bind(test)
                .bind(theme)
                .bind(snippets)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        message(false, json!({})).await;
        message(false, json!({})).await;
        message(true, json!({})).await;

        let stats = f
            .lead("GET", &format!("/{id}/versions/1/stats"), None)
            .await;
        let unknown = f
            .lead("GET", &format!("/{id}/versions/2/stats"), None)
            .await;
        let stranger = f
            .call(
                "GET",
                &format!(
                    "/api/v1/workspaces/{}/templates/{id}/versions/1/stats",
                    f.other_ws.as_uuid()
                ),
                None,
                Auth::Key(&f.read),
            )
            .await;

        assert_eq!(stats.status, StatusCode::OK, "{:?}", stats.body);
        assert_eq!(stats.body["messages"], 2);
        assert_eq!(stats.body["more"], false);
        assert_eq!(stats.body["renders"][0]["count"], 2);
        assert_eq!(stats.body["renders"][0]["theme_id"], json!(theme));
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        assert_eq!(stranger.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_stale_reviewed_revision_is_a_409_and_writes_no_version(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;
        let reviewed =
            f.lead("GET", &format!("/{id}"), None).await.body["draft"]["revision"].clone();
        let saved = f
            .lead(
                "PUT",
                &format!("/{id}/draft"),
                Some(json!({"document": email_doc("Hi", "changed")})),
            )
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);

        let stale = f
            .lead(
                "POST",
                &format!("/{id}/publish"),
                Some(json!({"revision": reviewed})),
            )
            .await;
        let versions = f.lead("GET", &format!("/{id}/versions"), None).await;

        assert_eq!(stale.status, StatusCode::CONFLICT, "{:?}", stale.body);
        assert_eq!(stale.body["code"], "template_draft_changed");
        assert_eq!(versions.body, json!([]));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_draft_save_that_names_a_stale_revision_is_a_409_and_writes_nothing(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", email_doc("First", "one")).await;
        let uri = format!("/{id}/draft");

        let current = f
            .lead(
                "PUT",
                &uri,
                Some(json!({"document": email_doc("Second", "two"), "revision": 1})),
            )
            .await;
        let stale = f
            .lead(
                "PUT",
                &uri,
                Some(json!({"document": email_doc("Third", "three"), "revision": 1})),
            )
            .await;
        let read = f.lead("GET", &format!("/{id}"), None).await;

        assert_eq!(current.status, StatusCode::OK, "{:?}", current.body);
        assert_eq!(current.body["draft"]["revision"], 2);
        assert_eq!(stale.status, StatusCode::CONFLICT);
        assert_eq!(stale.body["code"], "template_draft_revision_mismatch");
        assert_eq!(stale.body["current_revision"], 2);
        assert_eq!(read.body["draft"]["revision"], 2);
        assert_eq!(read.body["draft"]["document"], email_doc("Second", "two"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_convert_that_names_a_stale_revision_is_a_409_and_writes_nothing(pool: PgPool) {
        let f = fixture(&pool).await;
        let document = blocks_doc("Welcome", "<p>Footer</p>");
        let id = f.create("Newsletter", document.clone()).await;
        let uri = format!("/{id}/draft");
        let saved = f
            .lead(
                "PUT",
                &uri,
                Some(json!({"document": document, "revision": 1})),
            )
            .await;

        let stale = f
            .lead(
                "POST",
                &format!("{uri}/convert-to-mjml"),
                Some(json!({"revision": 1})),
            )
            .await;
        let read = f.lead("GET", &format!("/{id}"), None).await;
        let current = f
            .lead(
                "POST",
                &format!("{uri}/convert-to-mjml"),
                Some(json!({"revision": 2})),
            )
            .await;

        assert_eq!(saved.body["draft"]["revision"], 2, "{:?}", saved.body);
        assert_eq!(stale.status, StatusCode::CONFLICT);
        assert_eq!(stale.body["code"], "template_draft_revision_mismatch");
        assert_eq!(stale.body["current_revision"], 2);
        assert_eq!(read.body["draft"]["revision"], 2);
        assert_eq!(read.body["draft"]["document"]["mode"], "blocks");
        assert_eq!(current.status, StatusCode::OK, "{:?}", current.body);
        assert_eq!(current.body["draft"]["revision"], 3);
        assert_eq!(current.body["draft"]["document"]["mode"], "code");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_matching_reviewed_revision_publishes(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", email_doc("Hi", "x")).await;
        let reviewed =
            f.lead("GET", &format!("/{id}"), None).await.body["draft"]["revision"].clone();

        let published = f
            .lead(
                "POST",
                &format!("/{id}/publish"),
                Some(json!({"revision": reviewed})),
            )
            .await;

        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        assert_eq!(published.body["version"]["version"], 1);
    }
}
