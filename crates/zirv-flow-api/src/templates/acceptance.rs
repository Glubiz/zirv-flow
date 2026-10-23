//! The templates API end to end, driven through the router: the permission matrix, drafts,
//! publish and its immutability, validation against the registry and the event catalog, where-used
//! and preview through the render pool.

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
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
    use zirv_flow_templates::{Hooks, TemplateDocument, Tracking, render_email, render_webhook};

    use crate::admin::{self, Context};
    use crate::templates::{field_catalog, preview};
    use crate::{AppState, router};

    const PASSWORD: &str = "correct horse battery";
    /// The id of the verified sender identity the fixture plants in workspace `ours`; the valid
    /// documents name it.
    const SENDER: &str = "0192b1a4-7c3e-7000-8000-00000000a001";

    struct Reply {
        status: StatusCode,
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
        std::process::exit(i32::from(zirv_flow_render::host::run(None)));
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
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::to_owned);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
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
        zirv_flow_store::migrate(pool).await.unwrap();
        let ws = workspace(pool, "ours").await;
        let other_ws = workspace(pool, "theirs").await;
        let mut state = AppState::new(pool.clone(), std::sync::Arc::new(String::new));
        if let Some(render_pool) = render_pool {
            state = state.with_render_pool(render_pool);
        }
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
                    Some(json!({"name": name, "channel": "email", "document": document})),
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
        json!({"schema": 1, "channel": "email", "mode": "code", "email": {
            "subject": subject, "sender": SENDER, "lang": "en", "category": "marketing",
            "html": format!("<p>{body}</p><a href=\"{{{{ system.unsubscribe_url }}}}\">Unsubscribe</a>"),
            "text": format!("{body} {{{{ system.unsubscribe_url }}}}")
        }})
    }

    fn webhook_doc(body: &str) -> Value {
        json!({"schema": 1, "channel": "webhook", "mode": "code",
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
        let requests: [(&str, &str, String, Option<Value>, u16); 12] = [
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
                        if label != "get version" {
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
        let blocks = draft(json!({"schema": 1, "channel": "email", "mode": "blocks"})).await;
        let nonsense = draft(json!({"channel": "email"})).await;
        let incomplete = draft(json!({"schema": 1, "channel": "email", "mode": "code",
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
        let context = preview::sample_context(&catalog);

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
        json!({"schema": 1, "channel": "email", "mode": "code", "email": {
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
        let context = preview::sample_context(&catalog);
        let preview = f
            .lead("POST", &format!("/{id}/preview"), Some(json!({})))
            .await;
        let doc: TemplateDocument = serde_json::from_value(document).unwrap();
        let hooks = Hooks {
            tracking: Tracking::Inert {
                base_url: "https://preview.sample.invalid/t",
            },
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
        json!({"schema": 1, "channel": "email", "mode": "blocks", "blocks": {
        "subject": subject, "sender": "snd_1", "lang": "en", "category": "marketing",
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
        let context = preview::sample_context(&catalog);
        let preview = f
            .lead("POST", &format!("/{id}/preview"), Some(json!({})))
            .await;
        let doc: TemplateDocument = serde_json::from_value(document).unwrap();
        let hooks = Hooks {
            tracking: Tracking::Inert {
                base_url: "https://preview.sample.invalid/t",
            },
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
        newer["schema"] = json!(2);
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
}
