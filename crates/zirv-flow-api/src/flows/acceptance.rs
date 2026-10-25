//! The flows API end to end, driven through the router: the permission matrix, the problem
//! pointers, publish and the lifecycle, the dry run and the workspace time zone.

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;
    use zirv_flow_core::people::{Precedence, TraitType};
    use zirv_flow_core::{Role, Scope, WorkspaceId};
    use zirv_flow_store::NewTraitDefinition;

    use crate::admin::{self, Context};
    use crate::{AppState, router};

    const PASSWORD: &str = "correct horse battery";

    struct Reply {
        status: StatusCode,
        body: Value,
        /// The `name=value` of the session cookie the reply set, if it set one.
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
        let cookie = reply.cookie.clone().unwrap();
        Login {
            cookie,
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

    async fn fixture(pool: &PgPool) -> Fixture {
        zirv_flow_store::migrate(pool).await.unwrap();
        let ws = workspace(pool, "ours").await;
        let other_ws = workspace(pool, "theirs").await;
        let router = router(AppState::new(
            pool.clone(),
            std::sync::Arc::new(String::new),
        ));
        let key = |workspace: WorkspaceId, scope: Scope| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), workspace, "flows", &[scope])
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
        plan_trait(pool, ws).await;
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

    async fn plan_trait(pool: &PgPool, ws: WorkspaceId) {
        zirv_flow_store::create_trait_definition(
            pool,
            ws,
            &NewTraitDefinition {
                key: "plan".into(),
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
        fn flows(&self, tail: &str) -> String {
            format!("/api/v1/workspaces/{}/flows{tail}", self.ws.as_uuid())
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
            self.call(method, &self.flows(tail), body, Auth::Session(&self.lead))
                .await
        }

        /// Creates a flow as a workspace admin and returns its id.
        async fn create(&self, name: &str, graph: Value, settings: Value) -> String {
            let reply = self
                .lead(
                    "POST",
                    "",
                    Some(json!({"name": name, "graph": graph, "settings": settings})),
                )
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().to_owned()
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        /// Everything a publish, a lifecycle change or a dry run could write.
        async fn written(&self) -> (i64, i64, i64, String) {
            let versions = self.count("SELECT count(*) FROM flow_versions").await;
            let decisions = self
                .count("SELECT count(*) FROM decision_records WHERE action LIKE 'flow.%'")
                .await;
            let audits = self
                .count("SELECT count(*) FROM audit_log WHERE action LIKE 'flow.%'")
                .await;
            let flows: String = sqlx::query_scalar(
                "SELECT COALESCE(string_agg(f::text, ',' ORDER BY id), '') FROM flows f",
            )
            .fetch_one(&self.pool)
            .await
            .unwrap();
            (versions, decisions, audits, flows)
        }
    }

    fn simple_graph() -> Value {
        json!({"v": 1, "nodes": [
            {"id": "wait", "type": "wait.duration", "seconds": 60},
            {"id": "end", "type": "exit"}
        ], "edges": [{"id": "e1", "from": "wait", "port": "next", "to": "end"}]})
    }

    fn manual() -> Value {
        json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}})
    }

    fn issue_tuples(issues: &Value) -> Vec<(String, String, String)> {
        let text = |issue: &Value, key: &str| issue[key].as_str().unwrap_or("").to_owned();
        let mut tuples: Vec<_> = issues
            .as_array()
            .unwrap()
            .iter()
            .map(|issue| (text(issue, "code"), text(issue, "node"), text(issue, "at")))
            .collect();
        tuples.sort();
        tuples
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn each_route_needs_its_permission_and_no_other(pool: PgPool) {
        let f = fixture(&pool).await;
        let flow = f.create("Welcome", simple_graph(), manual()).await;
        let id = |tail: &str| format!("/{flow}{tail}");
        let credentials: [(&str, Auth<'_>, u8); 7] = [
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
        let held = |code: u8, need: u8| code.to_string().contains(&need.to_string());
        let requests: [(&str, &str, String, Option<Value>, u8); 9] = [
            ("list", "GET", String::new(), None, 1),
            ("get", "GET", id(""), None, 1),
            ("versions", "GET", id("/versions"), None, 1),
            (
                "create",
                "POST",
                String::new(),
                Some(json!({"name": "Other"})),
                2,
            ),
            (
                "update",
                "PATCH",
                id(""),
                Some(json!({"name": "Renamed"})),
                2,
            ),
            ("validate", "POST", id("/validate"), None, 2),
            (
                "publish dry run",
                "POST",
                id("/publish?dry_run=true"),
                None,
                3,
            ),
            ("pause", "POST", id("/pause"), None, 3),
            ("archive", "POST", id("/archive"), None, 3),
        ];

        for (label, method, tail, body, need) in requests {
            for (who, auth, code) in &credentials {
                let auth = match auth {
                    Auth::Anonymous => Auth::Anonymous,
                    Auth::Key(key) => Auth::Key(key),
                    Auth::Session(login) => Auth::Session(login),
                };
                let reply = f.call(method, &f.flows(&tail), body.clone(), auth).await;
                let expected_status = match (*code, held(*code, need)) {
                    (0, _) => StatusCode::UNAUTHORIZED,
                    (9, _) => StatusCode::NOT_FOUND,
                    (_, true) => {
                        // Allowed: whatever the route answers, it is not a refusal.
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
                        continue;
                    }
                    (_, false) => StatusCode::FORBIDDEN,
                };
                assert_eq!(reply.status, expected_status, "{label} as {who}");
            }
        }
        // The only flow the admin credentials archived is the one the matrix ended on.
        let flows = f.lead("GET", "?archived=true", None).await;
        assert!(flows.body.as_array().unwrap().len() >= 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn publish_pause_resume_and_archive_need_admin_and_publish_is_not_a_write(pool: PgPool) {
        let f = fixture(&pool).await;
        let flow = f.create("Welcome", simple_graph(), manual()).await;
        let tail = |action: &str| format!("/{flow}/{action}");

        for action in ["publish", "pause", "resume", "archive"] {
            let uri = f.flows(&tail(action));
            for (who, auth) in [
                ("write key", Auth::Key(&f.write)),
                ("read key", Auth::Key(&f.read)),
                ("member", Auth::Session(&f.member)),
            ] {
                let reply = f.call("POST", &uri, None, auth).await;
                assert_eq!(reply.status, StatusCode::FORBIDDEN, "{action} as {who}");
            }
        }

        let published = f
            .call(
                "POST",
                &f.flows(&tail("publish")),
                None,
                Auth::Key(&f.admin),
            )
            .await;
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        let (versions, decisions, _, _) = f.written().await;
        assert_eq!((versions, decisions), (1, 1));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_flow_is_drafted_validated_published_paused_resumed_and_archived(pool: PgPool) {
        let f = fixture(&pool).await;

        // A draft starts empty and may be incomplete.
        let created = f
            .call(
                "POST",
                &f.flows(""),
                Some(json!({"name": "  Welcome  "})),
                Auth::Session(&f.member),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        assert_eq!(created.body["name"], "Welcome");
        assert_eq!(created.body["state"], "draft");
        assert_eq!(created.body["current_version"], Value::Null);
        assert_eq!(
            created.body["graph"],
            json!({"v": 1, "nodes": [], "edges": []})
        );
        let id = created.body["id"].as_str().unwrap().to_owned();
        let empty = f.lead("POST", &format!("/{id}/validate"), None).await;
        assert_eq!(empty.body["valid"], false);
        assert_eq!(empty.body["issues"][0]["code"], "no_entry");
        let refused = f.lead("POST", &format!("/{id}/publish"), None).await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(refused.body["code"], "flow_invalid");
        assert_eq!(refused.body["flow_issues"][0]["code"], "no_entry");

        // Editing the draft, then publishing it.
        let edited = f
            .call(
                "PATCH",
                &f.flows(&format!("/{id}")),
                Some(json!({"graph": simple_graph(), "settings": manual()})),
                Auth::Session(&f.member),
            )
            .await;
        assert_eq!(edited.status, StatusCode::OK, "{:?}", edited.body);
        let valid = f
            .call(
                "POST",
                &f.flows(&format!("/{id}/validate")),
                None,
                Auth::Key(&f.write),
            )
            .await;
        assert_eq!(valid.body, json!({"valid": true, "issues": []}));
        let one = f.lead("POST", &format!("/{id}/publish"), None).await;
        assert_eq!(one.status, StatusCode::CREATED, "{:?}", one.body);
        assert_eq!(one.body["version"]["version"], 1);
        assert_eq!(one.body["flow"]["state"], "published");
        assert_eq!(one.body["flow"]["current_version"], 1);
        assert_eq!(one.body["version"]["graph"], simple_graph());

        // A later edit leaves version 1 alone; the next publish is version 2.
        let longer = json!({"v": 1, "nodes": [
            {"id": "wait", "type": "wait.duration", "seconds": 3600},
            {"id": "end", "type": "exit"}
        ], "edges": [{"id": "e1", "from": "wait", "port": "next", "to": "end"}]});
        f.lead("PATCH", &format!("/{id}"), Some(json!({"graph": longer})))
            .await;
        let still_one = f.lead("GET", &format!("/{id}/versions/1"), None).await;
        assert_eq!(still_one.body["graph"], simple_graph());
        let two = f.lead("POST", &format!("/{id}/publish"), None).await;
        assert_eq!(two.body["version"]["version"], 2);
        assert_eq!(two.body["version"]["graph"], longer);
        let versions = f.lead("GET", &format!("/{id}/versions"), None).await;
        let numbers: Vec<i64> = versions
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["version"].as_i64().unwrap())
            .collect();
        assert_eq!(numbers, [2, 1]);
        let missing = f.lead("GET", &format!("/{id}/versions/3"), None).await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);

        // The lifecycle, with its refusals.
        let resume_live = f.lead("POST", &format!("/{id}/resume"), None).await;
        assert_eq!(resume_live.status, StatusCode::CONFLICT);
        assert_eq!(resume_live.body["code"], "flow_state");
        let paused = f.lead("POST", &format!("/{id}/pause"), None).await;
        assert_eq!(paused.body["state"], "paused");
        let pause_again = f.lead("POST", &format!("/{id}/pause"), None).await;
        assert_eq!(pause_again.status, StatusCode::CONFLICT);
        let resumed = f.lead("POST", &format!("/{id}/resume"), None).await;
        assert_eq!(resumed.body["state"], "published");
        let archived = f.lead("POST", &format!("/{id}/archive"), None).await;
        assert_eq!(archived.body["state"], "archived");
        assert!(archived.body["archived_at"].is_string());
        for (method, tail, body) in [
            ("POST", "/publish", None),
            ("POST", "/pause", None),
            ("POST", "/archive", None),
        ] {
            let reply = f.lead(method, &format!("/{id}{tail}"), body).await;
            assert_eq!(reply.status, StatusCode::CONFLICT, "{tail}");
            assert_eq!(reply.body["code"], "flow_state", "{tail}");
        }
        let edit_archived = f
            .lead("PATCH", &format!("/{id}"), Some(json!({"name": "Renamed"})))
            .await;
        assert_eq!(edit_archived.status, StatusCode::CONFLICT);
        assert_eq!(edit_archived.body["code"], "flow_archived");
        let live = f.lead("GET", "", None).await;
        assert_eq!(live.body, json!([]));
        let all = f.lead("GET", "?archived=true", None).await;
        assert_eq!(all.body.as_array().unwrap().len(), 1);

        // Each change left an audit entry and the lifecycle left its decisions.
        let audits: Vec<String> = sqlx::query_scalar(
            "SELECT action FROM audit_log WHERE action LIKE 'flow.%' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            audits,
            [
                "flow.created",
                "flow.updated",
                "flow.published",
                "flow.updated",
                "flow.published",
                "flow.paused",
                "flow.resumed",
                "flow.archived"
            ]
        );
        for (action, count) in [
            ("flow.publish", 2),
            ("flow.pause", 1),
            ("flow.resume", 1),
            ("flow.archive", 1),
        ] {
            let n = f
                .count(&format!(
                    "SELECT count(*) FROM decision_records WHERE action = '{action}'"
                ))
                .await;
            assert_eq!(n, count, "{action}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_flow_with_problems_is_a_422_that_points_at_each_node_and_writes_nothing(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let list = Uuid::now_v7();
        let graph = json!({"v": 1, "nodes": [
            {"id": "t", "type": "action.update_trait", "key": "ghost", "change": {"op": "unset"}},
            {"id": "w", "type": "action.webhook", "url": "https://10.0.0.5/hook",
                "trait_keys": ["plan", "missing"]},
            {"id": "l", "type": "action.list", "list_id": list, "change": "add"},
            {"id": "b", "type": "branch", "conditions": [
                {"trait": "plan", "op": "eq", "value": "pro"},
                {"trait": "ghost", "op": "eq", "value": "x"}]},
            {"id": "end", "type": "exit"}
        ], "edges": [
            {"id": "e1", "from": "t", "port": "next", "to": "w"},
            {"id": "e2", "from": "w", "port": "next", "to": "l"},
            {"id": "e3", "from": "l", "port": "next", "to": "b"},
            {"id": "e4", "from": "b", "port": "case_0", "to": "end"},
            {"id": "e5", "from": "b", "port": "case_1", "to": "end"}
        ]});
        let settings = json!({"trigger": {"type": "segment", "segment_id": Uuid::now_v7(),
            "on": "entered"}, "reentry": {"mode": "once"}});
        let id = f.create("Broken", graph, settings).await;
        let before = f.written().await;
        let expected = [
            // The structural check: the branch's `else` is not wired.
            ("port_not_wired", "b", "/ports/else"),
            ("unknown_trait", "t", "/key"),
            ("webhook_url_refused", "w", "/url"),
            ("unknown_trait", "w", "/trait_keys/1"),
            ("webhook_secret_missing", "w", ""),
            ("unknown_list", "l", "/list_id"),
            ("unknown_trait", "b", "/conditions/1/trait"),
            ("unknown_segment", "", "/trigger/segment_id"),
        ];
        let mut expected: Vec<(String, String, String)> = expected
            .into_iter()
            .map(|(code, node, at)| (code.to_owned(), node.to_owned(), at.to_owned()))
            .collect();
        expected.sort();

        let validated = f.lead("POST", &format!("/{id}/validate"), None).await;
        let published = f.lead("POST", &format!("/{id}/publish"), None).await;
        let by_write_key = f
            .call(
                "POST",
                &f.flows(&format!("/{id}/validate")),
                None,
                Auth::Key(&f.write),
            )
            .await;

        assert_eq!(validated.status, StatusCode::OK);
        assert_eq!(validated.body["valid"], false);
        assert_eq!(issue_tuples(&validated.body["issues"]), expected);
        assert_eq!(by_write_key.body, validated.body);
        assert_eq!(published.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(published.body["code"], "flow_invalid");
        assert_eq!(published.body["title"], "Unprocessable Entity");
        assert_eq!(issue_tuples(&published.body["flow_issues"]), expected);
        // Nothing but the validations' own (non-)writes happened.
        assert_eq!(f.written().await, before);
        let flow = f.lead("GET", &format!("/{id}"), None).await;
        assert_eq!(flow.body["state"], "draft");
        assert_eq!(flow.body["current_version"], Value::Null);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_webhook_publishes_once_the_workspace_has_a_signing_secret(pool: PgPool) {
        let f = fixture(&pool).await;
        let graph = json!({"v": 1, "nodes": [
            {"id": "w", "type": "action.webhook", "url": "https://hooks.example.com/hook"},
            {"id": "end", "type": "exit"}
        ], "edges": [{"id": "e1", "from": "w", "port": "next", "to": "end"}]});
        let id = f.create("Notify", graph, manual()).await;

        let without = f.lead("POST", &format!("/{id}/publish"), None).await;
        sqlx::query(
            "INSERT INTO encrypted_secrets \
                (id, workspace_id, purpose, key_id, wrapped_key, nonce, ciphertext) \
             VALUES ($1, $2, 'webhook_signing', 'k', '\\x00', decode(repeat('00', 24), 'hex'), \
                '\\x00')",
        )
        .bind(Uuid::now_v7())
        .bind(f.ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        let with = f.lead("POST", &format!("/{id}/publish"), None).await;

        assert_eq!(without.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            issue_tuples(&without.body["flow_issues"]),
            [(
                "webhook_secret_missing".to_owned(),
                "w".to_owned(),
                String::new()
            )]
        );
        assert_eq!(with.status, StatusCode::CREATED, "{:?}", with.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_webhook_template_must_be_a_published_webhook_template_of_the_workspace(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        sqlx::query(
            "INSERT INTO encrypted_secrets \
                (id, workspace_id, purpose, key_id, wrapped_key, nonce, ciphertext) \
             VALUES ($1, $2, 'webhook_signing', 'k', '\\x00', decode(repeat('00', 24), 'hex'), \
                '\\x00')",
        )
        .bind(Uuid::now_v7())
        .bind(f.ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        let webhook = json!({"schema": 1, "channel": "webhook", "mode": "code",
            "webhook": {"headers": {}, "body": "{}"}});
        let email = json!({"schema": 1, "channel": "email", "mode": "code",
            "email": {"subject": "s", "html": "<p>x</p>"}});
        let mut templates = Vec::new();
        for (channel, document, publish) in [
            ("webhook", webhook.clone(), true),
            ("webhook", webhook.clone(), false),
            ("email", email, true),
        ] {
            let id = Uuid::now_v7();
            let mut connection = pool.acquire().await.unwrap();
            zirv_flow_store::create_template(
                &mut connection,
                f.ws,
                &zirv_flow_store::NewTemplate {
                    id,
                    name: format!("T {id}"),
                    channel: channel.to_owned(),
                    category: "transactional".to_owned(),
                    document,
                },
                None,
            )
            .await
            .unwrap();
            if publish {
                let locked = zirv_flow_store::lock_template(&mut connection, f.ws, id)
                    .await
                    .unwrap();
                zirv_flow_store::publish_template(
                    &mut connection,
                    f.ws,
                    &locked,
                    &json!([]),
                    None,
                    &zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t")),
                )
                .await
                .unwrap();
            }
            templates.push(id);
        }
        let [published, draft, other_channel] = templates[..] else {
            unreachable!("three templates")
        };
        let node = |id: &str, template: Option<Uuid>| {
            let mut node = json!({"id": id, "type": "action.webhook",
                "url": "https://hooks.example.com/hook"});
            if let Some(template) = template {
                node["template"] = json!(template);
            }
            node
        };
        let graph = json!({"v": 1, "nodes": [
            node("plain", None),
            node("ok", Some(published)),
            node("draft", Some(draft)),
            node("email", Some(other_channel)),
            node("ghost", Some(Uuid::now_v7())),
            {"id": "end", "type": "exit"}
        ], "edges": [
            {"id": "e1", "from": "plain", "port": "next", "to": "ok"},
            {"id": "e2", "from": "ok", "port": "next", "to": "draft"},
            {"id": "e3", "from": "draft", "port": "next", "to": "email"},
            {"id": "e4", "from": "email", "port": "next", "to": "ghost"},
            {"id": "e5", "from": "ghost", "port": "next", "to": "end"}
        ]});
        let id = f.create("Templated", graph, manual()).await;

        let validated = f.lead("POST", &format!("/{id}/validate"), None).await;

        assert_eq!(validated.status, StatusCode::OK);
        assert_eq!(validated.body["valid"], false);
        let mut expected: Vec<(String, String, String)> = ["draft", "email", "ghost"]
            .into_iter()
            .map(|node| {
                (
                    "webhook_template_unusable".to_owned(),
                    node.to_owned(),
                    "/template".to_owned(),
                )
            })
            .collect();
        expected.sort();
        assert_eq!(issue_tuples(&validated.body["issues"]), expected);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_send_must_name_a_published_email_template_a_live_email_destination_and_known_scopes(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let sender =
            zirv_flow_store::sending_fixtures::plant_verified_sender(&pool, f.ws, "example.com")
                .await
                .unwrap();
        let email = json!({"schema": 1, "channel": "email", "mode": "code", "email": {
            "subject": "s", "sender": sender, "lang": "en", "category": "transactional",
            "html": "<p>x</p>"}});
        let webhook = json!({"schema": 1, "channel": "webhook", "mode": "code",
            "webhook": {"headers": {}, "body": "{}"}});
        let mut templates = Vec::new();
        for (channel, document, publish) in [
            ("email", email.clone(), true),
            ("email", email, false),
            ("webhook", webhook, true),
        ] {
            let id = Uuid::now_v7();
            let mut connection = pool.acquire().await.unwrap();
            zirv_flow_store::create_template(
                &mut connection,
                f.ws,
                &zirv_flow_store::NewTemplate {
                    id,
                    name: format!("T {id}"),
                    channel: channel.to_owned(),
                    category: "transactional".to_owned(),
                    document,
                },
                None,
            )
            .await
            .unwrap();
            if publish {
                let locked = zirv_flow_store::lock_template(&mut connection, f.ws, id)
                    .await
                    .unwrap();
                zirv_flow_store::publish_template(
                    &mut connection,
                    f.ws,
                    &locked,
                    &json!([]),
                    None,
                    &zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t")),
                )
                .await
                .unwrap();
            }
            templates.push(id);
        }
        let [published, draft, webhook_template] = templates[..] else {
            unreachable!("three templates")
        };
        let mut destinations = Vec::new();
        for (kind, config, archive) in [
            ("fake", json!({}), false),
            ("fake", json!({}), true),
            (
                "webhook",
                json!({"url": "https://hooks.example.com/h"}),
                false,
            ),
        ] {
            let id = Uuid::now_v7();
            zirv_flow_store::create_destination(
                &pool,
                f.ws,
                &zirv_flow_store::NewDestination {
                    id,
                    kind,
                    name: &format!("D {id}"),
                    config: &config,
                    credential_id: None,
                    rate: 10,
                    burst: 10,
                    created_by: None,
                },
            )
            .await
            .unwrap();
            if archive {
                zirv_flow_store::archive_destination(&pool, f.ws, id)
                    .await
                    .unwrap();
            }
            destinations.push(id);
        }
        let [live, archived, webhook_destination] = destinations[..] else {
            unreachable!("three destinations")
        };
        let topic = zirv_flow_store::create_topic(
            &pool,
            f.ws,
            &zirv_flow_store::NewTopic {
                key: "news".to_owned(),
                name: "News".to_owned(),
                description: String::new(),
                channel: zirv_flow_core::consent::Channel::Email,
                policy: zirv_flow_core::consent::TopicPolicy::OptIn,
            },
        )
        .await
        .unwrap()
        .id;
        let node = |id: &str, template: Uuid, destination: Uuid, topic: Option<Uuid>| {
            let mut node = json!({"id": id, "type": "action.send",
                "template": template, "destination": destination});
            if let Some(topic) = topic {
                node["topic"] = json!(topic);
            }
            node
        };
        let mut unknown_dimension = node("dimension", published, live, None);
        unknown_dimension["dimensions"] = json!({"vertical": "sports"});
        let graph = json!({"v": 1, "nodes": [
            node("ok", published, live, Some(topic)),
            node("draft", draft, live, None),
            node("webhook_template", webhook_template, live, None),
            node("archived", published, archived, None),
            node("webhook_destination", published, webhook_destination, None),
            node("topic", published, live, Some(Uuid::now_v7())),
            unknown_dimension,
            {"id": "end", "type": "exit"}
        ], "edges": [
            {"id": "e1", "from": "ok", "port": "next", "to": "draft"},
            {"id": "e2", "from": "draft", "port": "next", "to": "webhook_template"},
            {"id": "e3", "from": "webhook_template", "port": "next", "to": "archived"},
            {"id": "e4", "from": "archived", "port": "next", "to": "webhook_destination"},
            {"id": "e5", "from": "webhook_destination", "port": "next", "to": "topic"},
            {"id": "e6", "from": "topic", "port": "next", "to": "dimension"},
            {"id": "e7", "from": "dimension", "port": "next", "to": "end"}
        ]});
        let id = f.create("Mail", graph, manual()).await;

        let validated = f.lead("POST", &format!("/{id}/validate"), None).await;

        assert_eq!(validated.status, StatusCode::OK);
        assert_eq!(validated.body["valid"], false);
        let mut expected: Vec<(String, String, String)> = [
            ("send_template_unusable", "draft", "/template"),
            ("send_template_unusable", "webhook_template", "/template"),
            ("send_destination_unusable", "archived", "/destination"),
            (
                "send_destination_unusable",
                "webhook_destination",
                "/destination",
            ),
            ("unknown_topic", "topic", "/topic"),
            ("unknown_dimension", "dimension", "/dimensions"),
        ]
        .into_iter()
        .map(|(code, node, at)| (code.to_owned(), node.to_owned(), at.to_owned()))
        .collect();
        expected.sort();
        assert_eq!(issue_tuples(&validated.body["issues"]), expected);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dry_run_reports_the_impact_and_writes_nothing(pool: PgPool) {
        let f = fixture(&pool).await;
        // A segment with members, and a failing source that feeds the trait a branch reads.
        let segment = f
            .lead_segment(
                "Pros",
                json!({"v": 1, "trait": "plan", "op": "eq", "value": "pro"}),
            )
            .await;
        sqlx::query("UPDATE segments SET member_count = 42 WHERE id = $1")
            .bind(segment)
            .execute(&pool)
            .await
            .unwrap();
        let source = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO sources (id, workspace_id, name, kind, config, enabled) \
             VALUES ($1, $2, 'CRM', 'postgres', '{}', true)",
        )
        .bind(source)
        .bind(f.ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO streams (id, workspace_id, source_id, name, enabled, sync_mode, mapping, \
                discovered, enabled_at, last_success_at, last_run_failed) \
             VALUES ($1, $2, $3, 'contacts', true, 'full_refresh', $4, '{}', now(), now(), true)",
        )
        .bind(Uuid::now_v7())
        .bind(f.ws.as_uuid())
        .bind(source)
        .bind(json!({"columns": [
            {"column": "e", "target": {"type": "identifier", "kind": "email"}},
            {"column": "p", "target": {"type": "trait", "key": "plan"}}]}))
        .execute(&pool)
        .await
        .unwrap();
        let graph = json!({"v": 1, "nodes": [
            {"id": "b", "type": "branch", "conditions": [{"trait": "plan", "op": "eq", "value": "pro"}]},
            {"id": "end", "type": "exit"}
        ], "edges": [
            {"id": "e1", "from": "b", "port": "case_0", "to": "end"},
            {"id": "e2", "from": "b", "port": "else", "to": "end"}
        ]});
        let settings = json!({"trigger": {"type": "segment", "segment_id": segment,
            "on": "entered", "enroll_current": true}, "reentry": {"mode": "once"}});
        let id = f.create("Pros welcome", graph, settings).await;
        let broken = f
            .create(
                "Broken",
                json!({"v": 1, "nodes": [], "edges": []}),
                manual(),
            )
            .await;
        let before = f.written().await;

        let impact = f
            .lead("POST", &format!("/{id}/publish?dry_run=true"), None)
            .await;
        let bare = f
            .lead("POST", &format!("/{id}/publish?dry_run"), None)
            .await;
        let invalid = f
            .lead("POST", &format!("/{broken}/publish?dry_run=true"), None)
            .await;
        let bad_flag = f
            .lead("POST", &format!("/{id}/publish?dry_run=maybe"), None)
            .await;

        assert_eq!(impact.status, StatusCode::OK, "{:?}", impact.body);
        assert_eq!(impact.body["valid"], true);
        assert_eq!(impact.body["issues"], json!([]));
        assert_eq!(impact.body["next_version"], 1);
        assert_eq!(impact.body["audience"]["members"], 42);
        assert_eq!(impact.body["audience"]["segment_id"], segment.to_string());
        assert_eq!(impact.body["enrollments_staying_on_older_versions"], 0);
        let inputs = impact.body["unhealthy_inputs"].as_array().unwrap();
        assert_eq!(inputs.len(), 1, "{inputs:?}");
        assert_eq!(inputs[0]["name"], "CRM");
        assert_eq!(inputs[0]["state"], "failing");
        assert_eq!(bare.body, impact.body);
        assert_eq!(invalid.status, StatusCode::OK);
        assert_eq!(invalid.body["valid"], false);
        assert_eq!(invalid.body["issues"][0]["code"], "no_entry");
        assert_eq!(invalid.body["audience"], Value::Null);
        assert_eq!(bad_flag.status, StatusCode::BAD_REQUEST);
        // Not one row changed: no version, decision, audit entry, or flow row.
        assert_eq!(f.written().await, before);

        // The same request without the flag publishes, and the next dry run counts on from it.
        let published = f.lead("POST", &format!("/{id}/publish"), None).await;
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        // A person in flight on version 1 stays there when version 2 goes live.
        let person = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(person)
            .bind(f.ws.as_uuid())
            .execute(&f.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO enrollments \
                (id, workspace_id, flow_id, flow_version, person_id, state, node, occurrence_id, \
                 trace_id, cause_kind, cause_id) \
             SELECT $1, $2, id, 1, $3, 'waiting', 'n', $1, $1, 'api', 't' \
             FROM flows WHERE id::text = $4",
        )
        .bind(Uuid::now_v7())
        .bind(f.ws.as_uuid())
        .bind(person)
        .bind(&id)
        .execute(&f.pool)
        .await
        .unwrap();
        let after = f
            .lead("POST", &format!("/{id}/publish?dry_run=true"), None)
            .await;
        assert_eq!(after.body["next_version"], 2);
        assert_eq!(after.body["enrollments_staying_on_older_versions"], 1);
        assert_eq!(f.written().await.0, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_document_that_is_not_a_flow_a_taken_name_and_another_workspace_are_refused(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let id = f.create("Welcome", simple_graph(), manual()).await;

        let unknown_type = f
            .lead(
                "POST",
                "",
                Some(json!({"name": "A", "graph": {"v": 1, "edges": [],
                    "nodes": [{"id": "n", "type": "teleport"}]}})),
            )
            .await;
        let unknown_key = f
            .lead(
                "PATCH",
                &format!("/{id}"),
                Some(json!({"settings": {"trigger": {"type": "manual"},
                    "reentry": {"mode": "once"}, "extra": 1}})),
            )
            .await;
        let duplicate = f.lead("POST", "", Some(json!({"name": "welcome"}))).await;
        let nothing = f.lead("PATCH", &format!("/{id}"), Some(json!({}))).await;
        let blank = f.lead("POST", "", Some(json!({"name": "   "}))).await;
        let other = format!("/api/v1/workspaces/{}/flows/{id}", f.other_ws.as_uuid());
        let foreign_get = f.call("GET", &other, None, Auth::Session(&f.lead)).await;
        let foreign_publish = f
            .call(
                "POST",
                &format!("{other}/publish"),
                None,
                Auth::Session(&f.lead),
            )
            .await;
        let missing = f.lead("GET", &format!("/{}", Uuid::now_v7()), None).await;
        let not_an_id = f.lead("GET", "/nope", None).await;

        for refused in [&unknown_type, &unknown_key] {
            assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(refused.body["code"], "flow_malformed");
        }
        assert_eq!(duplicate.status, StatusCode::CONFLICT);
        assert_eq!(nothing.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(blank.status, StatusCode::UNPROCESSABLE_ENTITY);
        // The caller is not a member of the other workspace.
        for reply in [&foreign_get, &foreign_publish] {
            assert!(
                matches!(reply.status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN),
                "{}",
                reply.status
            );
        }
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        assert_eq!(not_an_id.status, StatusCode::NOT_FOUND);
        let list = f.lead("GET", "", None).await;
        assert_eq!(list.body.as_array().unwrap().len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_an_admin_sets_the_workspace_time_zone_and_it_must_be_an_iana_name(pool: PgPool) {
        let f = fixture(&pool).await;
        let uri = format!("/api/v1/workspaces/{}", f.ws.as_uuid());
        let set = |zone: &str| Some(json!({"timezone": zone}));

        let default = f.call("GET", &uri, None, Auth::Key(&f.read)).await;
        let by_member = f
            .call(
                "PATCH",
                &uri,
                set("Europe/Copenhagen"),
                Auth::Session(&f.member),
            )
            .await;
        let by_write_key = f
            .call("PATCH", &uri, set("Europe/Copenhagen"), Auth::Key(&f.write))
            .await;
        let by_read_key = f
            .call("PATCH", &uri, set("Europe/Copenhagen"), Auth::Key(&f.read))
            .await;
        let by_other = f
            .call(
                "PATCH",
                &uri,
                set("Europe/Copenhagen"),
                Auth::Key(&f.other_admin),
            )
            .await;
        let unchanged = f.call("GET", &uri, None, Auth::Key(&f.read)).await;
        let by_admin = f
            .call(
                "PATCH",
                &uri,
                set("Europe/Copenhagen"),
                Auth::Session(&f.lead),
            )
            .await;
        let by_admin_key = f
            .call("PATCH", &uri, set("America/New_York"), Auth::Key(&f.admin))
            .await;
        let read_back = f.call("GET", &uri, None, Auth::Key(&f.read)).await;
        let mut invalid = Vec::new();
        for zone in [
            "Mars/Olympus",
            "",
            "+02:00",
            "europe/copenhagen ",
            &"x".repeat(65),
        ] {
            invalid.push(
                f.call("PATCH", &uri, set(zone), Auth::Session(&f.lead))
                    .await
                    .status,
            );
        }
        let not_a_string = f
            .call(
                "PATCH",
                &uri,
                Some(json!({"timezone": 5})),
                Auth::Session(&f.lead),
            )
            .await;
        let missing_field = f
            .call("PATCH", &uri, Some(json!({})), Auth::Session(&f.lead))
            .await;
        let after_invalid = f.call("GET", &uri, None, Auth::Key(&f.read)).await;

        assert_eq!(default.body["timezone"], "UTC");
        assert_eq!(by_member.status, StatusCode::FORBIDDEN);
        assert_eq!(by_write_key.status, StatusCode::FORBIDDEN);
        assert_eq!(by_read_key.status, StatusCode::FORBIDDEN);
        assert_eq!(by_other.status, StatusCode::NOT_FOUND);
        assert_eq!(unchanged.body["timezone"], "UTC");
        assert_eq!(by_admin.status, StatusCode::OK, "{:?}", by_admin.body);
        assert_eq!(by_admin.body["timezone"], "Europe/Copenhagen");
        assert_eq!(by_admin_key.body["timezone"], "America/New_York");
        assert_eq!(read_back.body["timezone"], "America/New_York");
        assert_eq!(invalid, [StatusCode::UNPROCESSABLE_ENTITY; 5]);
        assert!(not_a_string.status.is_client_error());
        assert!(missing_field.status.is_client_error());
        assert_eq!(after_invalid.body["timezone"], "America/New_York");
        let other: String = sqlx::query_scalar("SELECT timezone FROM workspaces WHERE id = $1")
            .bind(f.other_ws.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(other, "UTC");
        let audits = f
            .count("SELECT count(*) FROM audit_log WHERE action = 'workspace.updated'")
            .await;
        assert_eq!(audits, 2);
    }

    impl Fixture {
        /// Creates a segment through the API as a workspace admin and returns its id.
        async fn lead_segment(&self, name: &str, definition: Value) -> Uuid {
            let uri = format!("/api/v1/workspaces/{}/segments", self.ws.as_uuid());
            let reply = self
                .call(
                    "POST",
                    &uri,
                    Some(json!({"name": name, "definition": definition})),
                    Auth::Session(&self.lead),
                )
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().parse().unwrap()
        }
    }

    fn api_trigger() -> Value {
        json!({"trigger": {"type": "api"}, "reentry": {"mode": "once"}})
    }

    impl Fixture {
        /// Creates a flow and publishes it as a workspace admin; returns its id.
        async fn live(&self, name: &str, settings: Value) -> String {
            let flow = self.create(name, simple_graph(), settings).await;
            let published = self.lead("POST", &format!("/{flow}/publish"), None).await;
            assert_eq!(
                published.status,
                StatusCode::CREATED,
                "{:?}",
                published.body
            );
            flow
        }

        /// A person with an email identifier.
        async fn person(&self, email: &str) -> Uuid {
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
                .bind(id)
                .bind(self.ws.as_uuid())
                .execute(&self.pool)
                .await
                .unwrap();
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
            id
        }

        /// The queued `flow.enroll` payloads and the cause kind of their jobs.
        async fn enroll_jobs(&self) -> Vec<(Value, String)> {
            sqlx::query_as(
                "SELECT payload, cause_kind FROM jobs WHERE kind = 'flow.enroll' \
                 ORDER BY created_at",
            )
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn enrolling_through_the_api_takes_write_and_a_manual_start_takes_admin(pool: PgPool) {
        let f = fixture(&pool).await;
        let api = f.live("By api", api_trigger()).await;
        let manual_flow = f.live("By hand", manual()).await;
        let ada = f.person("ada@example.com").await;
        let list = zirv_flow_store::create_list(&pool, f.ws, "VIPs", "", None)
            .await
            .unwrap()
            .id;
        let credentials: [(&str, Auth<'_>, u8); 7] = [
            ("anonymous", Auth::Anonymous, 0),
            ("read key", Auth::Key(&f.read), 1),
            ("write key", Auth::Key(&f.write), 2),
            ("admin key", Auth::Key(&f.admin), 3),
            ("member", Auth::Session(&f.member), 12),
            ("workspace admin", Auth::Session(&f.lead), 123),
            ("other workspace's admin key", Auth::Key(&f.other_admin), 9),
        ];
        let held = |code: u8, need: u8| code.to_string().contains(&need.to_string());
        let requests = [
            (
                "enroll",
                f.flows(&format!("/{api}/enrollments")),
                json!({"person_id": ada}),
                2_u8,
            ),
            (
                "start",
                f.flows(&format!("/{manual_flow}/start")),
                json!({"list_id": list}),
                3,
            ),
        ];

        for (label, uri, body, need) in requests {
            for (who, auth, code) in &credentials {
                let auth = match auth {
                    Auth::Anonymous => Auth::Anonymous,
                    Auth::Key(key) => Auth::Key(key),
                    Auth::Session(login) => Auth::Session(login),
                };
                let reply = f.call("POST", &uri, Some(body.clone()), auth).await;
                let expected = match (*code, held(*code, need)) {
                    (0, _) => StatusCode::UNAUTHORIZED,
                    (9, _) => StatusCode::NOT_FOUND,
                    (_, true) => StatusCode::ACCEPTED,
                    (_, false) => StatusCode::FORBIDDEN,
                };
                assert_eq!(reply.status, expected, "{label} as {who}: {:?}", reply.body);
            }
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_api_enrollment_is_queued_for_a_person_by_id_or_identifier_and_a_key_makes_a_retry_one_occurrence(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let flow = f.live("By api", api_trigger()).await;
        let ada = f.person("ada@example.com").await;
        let uri = f.flows(&format!("/{flow}/enrollments"));
        let post = |body: Value| f.call("POST", &uri, Some(body), Auth::Key(&f.write));

        let by_id = post(json!({"person_id": ada, "idempotency_key": "order-1"})).await;
        let retry = post(json!({
            "identifier": {"kind": "email", "value": "ada@example.com"},
            "idempotency_key": "order-1"
        }))
        .await;
        let other_key = post(json!({"person_id": ada, "idempotency_key": "order-2"})).await;
        let no_key = post(json!({"person_id": ada})).await;

        for reply in [&by_id, &retry, &other_key, &no_key] {
            assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.body);
        }
        assert_eq!(by_id.body["occurrence_id"], retry.body["occurrence_id"]);
        assert_ne!(by_id.body["occurrence_id"], other_key.body["occurrence_id"]);
        assert_ne!(
            other_key.body["occurrence_id"],
            no_key.body["occurrence_id"]
        );
        let jobs = f.enroll_jobs().await;
        assert_eq!(jobs.len(), 3, "the retry holds the first job's key");
        let (payload, cause) = &jobs[0];
        assert_eq!(cause, "api");
        assert_eq!(payload["trigger"], "api");
        assert_eq!(payload["flow_id"], flow);
        assert_eq!(
            payload["people"],
            json!([[ada, by_id.body["occurrence_id"]]])
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_api_enrollment_that_names_nobody_or_an_unknown_person_or_the_wrong_flow_is_refused(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let api = f.live("By api", api_trigger()).await;
        let by_hand = f.live("By hand", manual()).await;
        let draft = f.create("Draft", simple_graph(), api_trigger()).await;
        let paused = f.live("Paused", api_trigger()).await;
        let archived = f.live("Archived", api_trigger()).await;
        f.lead("POST", &format!("/{paused}/pause"), None).await;
        f.lead("POST", &format!("/{archived}/archive"), None).await;
        let ada = f.person("ada@example.com").await;
        let fixture = &f;
        let enroll = |flow: &str, body: Value| {
            let uri = fixture.flows(&format!("/{flow}/enrollments"));
            async move {
                fixture
                    .call("POST", &uri, Some(body), Auth::Key(&fixture.write))
                    .await
            }
        };

        let neither = enroll(&api, json!({})).await;
        let both = enroll(
            &api,
            json!({"person_id": ada, "identifier": {"kind": "email", "value": "ada@example.com"}}),
        )
        .await;
        let stranger = enroll(&api, json!({"person_id": Uuid::now_v7()})).await;
        let unknown = enroll(
            &api,
            json!({"identifier": {"kind": "email", "value": "nobody@example.com"}}),
        )
        .await;
        let bad_identifier =
            enroll(&api, json!({"identifier": {"kind": "fax", "value": "1"}})).await;
        let long_key = enroll(
            &api,
            json!({"person_id": ada, "idempotency_key": "k".repeat(201)}),
        )
        .await;
        let wrong_trigger = enroll(&by_hand, json!({"person_id": ada})).await;
        let not_published = enroll(&draft, json!({"person_id": ada})).await;
        let on_pause = enroll(&paused, json!({"person_id": ada})).await;
        let ended = enroll(&archived, json!({"person_id": ada})).await;
        let missing = enroll(&Uuid::now_v7().to_string(), json!({"person_id": ada})).await;

        for (label, reply, status) in [
            ("neither", &neither, StatusCode::UNPROCESSABLE_ENTITY),
            ("both", &both, StatusCode::UNPROCESSABLE_ENTITY),
            ("stranger", &stranger, StatusCode::NOT_FOUND),
            ("unknown identifier", &unknown, StatusCode::NOT_FOUND),
            (
                "bad identifier",
                &bad_identifier,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            ("long key", &long_key, StatusCode::UNPROCESSABLE_ENTITY),
            ("wrong trigger", &wrong_trigger, StatusCode::CONFLICT),
            ("draft", &not_published, StatusCode::CONFLICT),
            // A paused flow still answers: the job records the skip.
            ("paused", &on_pause, StatusCode::ACCEPTED),
            ("archived", &ended, StatusCode::CONFLICT),
            ("no such flow", &missing, StatusCode::NOT_FOUND),
        ] {
            assert_eq!(reply.status, status, "{label}: {:?}", reply.body);
        }
        assert_eq!(wrong_trigger.body["code"], "flow_state");
        assert_eq!(
            f.enroll_jobs().await.len(),
            1,
            "only the paused flow's request"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_manual_start_queues_a_segment_or_a_list_audits_and_refuses_the_rest(pool: PgPool) {
        let f = fixture(&pool).await;
        let by_hand = f.live("By hand", manual()).await;
        let api = f.live("By api", api_trigger()).await;
        let segment = f
            .lead_segment(
                "Pros",
                json!({"op": "eq", "trait": "plan", "value": "pro", "v": 1}),
            )
            .await;
        let list = zirv_flow_store::create_list(&pool, f.ws, "VIPs", "", None)
            .await
            .unwrap()
            .id;
        let uri = f.flows(&format!("/{by_hand}/start"));
        let start = |body: Value| f.call("POST", &uri, Some(body), Auth::Session(&f.lead));

        let of_segment = start(json!({"segment_id": segment})).await;
        let of_list = start(json!({"list_id": list})).await;
        let neither = start(json!({})).await;
        let both = start(json!({"segment_id": segment, "list_id": list})).await;
        let no_segment = start(json!({"segment_id": Uuid::now_v7()})).await;
        let no_list = start(json!({"list_id": Uuid::now_v7()})).await;
        let wrong_trigger = f
            .call(
                "POST",
                &f.flows(&format!("/{api}/start")),
                Some(json!({"list_id": list})),
                Auth::Session(&f.lead),
            )
            .await;
        let by_key = f
            .call(
                "POST",
                &uri,
                Some(json!({"list_id": list})),
                Auth::Key(&f.admin),
            )
            .await;

        assert_eq!(
            of_segment.status,
            StatusCode::ACCEPTED,
            "{:?}",
            of_segment.body
        );
        assert_eq!(of_list.status, StatusCode::ACCEPTED);
        assert_eq!(by_key.status, StatusCode::ACCEPTED);
        assert_eq!(neither.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(both.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(no_segment.status, StatusCode::NOT_FOUND);
        assert_eq!(no_list.status, StatusCode::NOT_FOUND);
        assert_eq!(wrong_trigger.status, StatusCode::CONFLICT);
        let jobs = f.enroll_jobs().await;
        assert_eq!(jobs.len(), 3);
        // A signed-in admin starts it under the cause `manual`; a key has no user, so `api`.
        assert_eq!(jobs[0].1, "manual");
        assert_eq!(jobs[0].0["trigger"], "manual");
        assert_eq!(
            jobs[0].0["audience"],
            json!({"type": "segment", "segment_id": segment})
        );
        assert_eq!(jobs[0].0["occurrence"], of_segment.body["occurrence_id"]);
        assert_eq!(
            jobs[1].0["audience"],
            json!({"type": "list", "list_id": list})
        );
        assert_eq!(jobs[2].1, "api");
        let audits = f
            .count("SELECT count(*) FROM audit_log WHERE action = 'flow.started'")
            .await;
        assert_eq!(audits, 3);
    }

    /// An enrollment of `flow` for a new person, in `state`. A failed one stopped at `failed_node`
    /// and kept the input `start`, as a failed first step does.
    async fn enrollment_in(
        f: &Fixture,
        flow: &str,
        state: &str,
        node: &str,
        failed_node: Option<&str>,
    ) -> String {
        let person = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(person)
            .bind(f.ws.as_uuid())
            .execute(&f.pool)
            .await
            .unwrap();
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO enrollments \
                (id, workspace_id, flow_id, flow_version, person_id, state, node, occurrence_id, \
                 trace_id, cause_kind, cause_id, error, failed_node, failure_placed, resume_input) \
             SELECT $1, $2, id, 1, $3, $5, $6, $1, $1, 'api', 't', \
                    CASE WHEN $5 = 'failed' THEN 'boom' END, $7, $7 IS NOT NULL, \
                    CASE WHEN $5 = 'failed' THEN '{\"input\": \"start\"}'::jsonb END \
             FROM flows WHERE id::text = $4",
        )
        .bind(id)
        .bind(f.ws.as_uuid())
        .bind(person)
        .bind(flow)
        .bind(state)
        .bind(node)
        .bind(failed_node)
        .execute(&f.pool)
        .await
        .unwrap();
        id.to_string()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_operator_controls_need_admin_refuse_the_wrong_state_and_are_audited(pool: PgPool) {
        let f = fixture(&pool).await;
        let flow = f.create("Welcome", simple_graph(), manual()).await;
        let exit_older = |version: i32| format!("/{flow}/versions/{version}/exit-enrollments");

        // A draft has no enrollments.
        let draft = f.lead("POST", &exit_older(1), None).await;
        assert_eq!(draft.status, StatusCode::CONFLICT);
        assert_eq!(draft.body["code"], "flow_state");

        let published = f.lead("POST", &format!("/{flow}/publish"), None).await;
        assert_eq!(published.status, StatusCode::CREATED);
        f.lead(
            "PATCH",
            &format!("/{flow}"),
            Some(json!({"graph": {"v": 1, "nodes": [
                {"id": "wait", "type": "wait.duration", "seconds": 3600},
                {"id": "end", "type": "exit"}
            ], "edges": [{"id": "e1", "from": "wait", "port": "next", "to": "end"}]}})),
        )
        .await;
        let second = f.lead("POST", &format!("/{flow}/publish"), None).await;
        assert_eq!(second.body["version"]["version"], 2);
        let failed = enrollment_in(&f, &flow, "failed", "wait", Some("wait")).await;
        let also_failed = enrollment_in(&f, &flow, "failed", "wait", Some("wait")).await;
        let late = enrollment_in(&f, &flow, "failed", "wait", Some("end")).await;
        let waiting = enrollment_in(&f, &flow, "waiting", "wait", None).await;
        let retry = |id: &str| format!("/{flow}/enrollments/{id}/retry");
        let skip = |id: &str| format!("/{flow}/enrollments/{id}/skip");

        // Only a workspace admin, by session or by key, may use them.
        let routes = [
            ("retry", retry(&failed)),
            ("skip", skip(&failed)),
            ("exit", exit_older(1)),
        ];
        for (label, tail) in &routes {
            for (who, auth, expected) in [
                ("anonymous", Auth::Anonymous, StatusCode::UNAUTHORIZED),
                ("read key", Auth::Key(&f.read), StatusCode::FORBIDDEN),
                ("write key", Auth::Key(&f.write), StatusCode::FORBIDDEN),
                ("member", Auth::Session(&f.member), StatusCode::FORBIDDEN),
                (
                    "other workspace's admin key",
                    Auth::Key(&f.other_admin),
                    StatusCode::NOT_FOUND,
                ),
            ] {
                let reply = f.call("POST", &f.flows(tail), None, auth).await;
                assert_eq!(reply.status, expected, "{label} as {who}");
            }
        }
        assert_eq!(
            f.count("SELECT count(*) FROM audit_log WHERE action LIKE 'flow.enrollment%'")
                .await,
            0,
            "a refusal writes nothing"
        );

        // The wrong state is a 409 with its own code; nothing there is not found.
        for (label, tail) in [
            ("retry a waiting one", retry(&waiting)),
            ("skip a waiting one", skip(&waiting)),
            ("skip a late failure", skip(&late)),
        ] {
            let reply = f.lead("POST", &tail, None).await;
            assert_eq!(reply.status, StatusCode::CONFLICT, "{label}");
            assert_eq!(reply.body["code"], "enrollment_state", "{label}");
        }
        let unknown = f
            .lead("POST", &retry(&Uuid::now_v7().to_string()), None)
            .await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        let current = f.lead("POST", &exit_older(2), None).await;
        assert_eq!(current.status, StatusCode::CONFLICT);
        assert_eq!(current.body["code"], "flow_state");
        let no_version = f.lead("POST", &exit_older(9), None).await;
        assert_eq!(no_version.status, StatusCode::NOT_FOUND);

        // The accepted ones queue their work, record a decision on the enrollment's own trace
        // (cause manual) and audit the action with that trace.
        let retried = f.lead("POST", &retry(&failed), None).await;
        let skipped = f.lead("POST", &skip(&also_failed), None).await;
        let exited = f.lead("POST", &exit_older(1), None).await;
        assert_eq!(retried.status, StatusCode::ACCEPTED, "{:?}", retried.body);
        assert_eq!(skipped.status, StatusCode::ACCEPTED, "{:?}", skipped.body);
        assert_eq!(exited.status, StatusCode::ACCEPTED, "{:?}", exited.body);
        assert_eq!(retried.body["action"], "retry");
        assert_eq!(skipped.body["action"], "skip");
        assert_eq!(exited.body["version"], 1);
        let trace_of = |reply: &Reply| reply.body["trace_id"].as_str().unwrap().to_owned();
        for (reply, outcome) in [(&retried, "retried"), (&skipped, "skipped")] {
            let (cause, reason, subjects): (String, Option<String>, Value) = sqlx::query_as(
                "SELECT cause_kind, reason, subject_ids FROM decision_records \
                 WHERE trace_id::text = $1 AND outcome = $2",
            )
            .bind(trace_of(reply))
            .bind(outcome)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(cause, "manual");
            assert_eq!(reason.as_deref(), Some("operator"));
            // The enrollment's own trace (the fixture's trace id is the enrollment id).
            assert_eq!(
                trace_of(reply),
                reply.body["enrollment_id"].as_str().unwrap()
            );
            assert_eq!(subjects["enrollment"], reply.body["enrollment_id"]);
        }
        let audits: Vec<(String, Value)> = sqlx::query_as(
            "SELECT action, summary FROM audit_log WHERE action LIKE 'flow.enrollment%' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            audits.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>(),
            [
                "flow.enrollment_retried",
                "flow.enrollment_skipped",
                "flow.enrollments_exited"
            ]
        );
        assert_eq!(audits[0].1["trace_id"], retried.body["trace_id"]);
        assert_eq!(audits[2].1["trace_id"], exited.body["trace_id"]);
        let queued: Vec<Value> = sqlx::query_scalar(
            "SELECT payload FROM jobs WHERE kind = 'flow.lifecycle' ORDER BY created_at",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0]["action"], "exit_version");
        assert_eq!(queued[0]["version"], 1);
        assert_eq!(
            f.count("SELECT count(*) FROM jobs WHERE kind = 'flow.step'")
                .await,
            2,
            "the retry and the skip are jobs"
        );

        // An archived flow takes no bulk exit.
        let archived = f.lead("POST", &format!("/{flow}/archive"), None).await;
        assert_eq!(archived.status, StatusCode::OK);
        let after = f.lead("POST", &exit_older(1), None).await;
        assert_eq!(after.status, StatusCode::CONFLICT);
        assert_eq!(after.body["code"], "flow_archived");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn no_operator_control_works_on_a_paused_or_an_archived_flow_and_none_writes_anything(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let flow = f.live("Welcome", manual()).await;
        f.lead(
            "PATCH",
            &format!("/{flow}"),
            Some(json!({"graph": {"v": 1, "nodes": [
                {"id": "wait", "type": "wait.duration", "seconds": 3600},
                {"id": "end", "type": "exit"}
            ], "edges": [{"id": "e1", "from": "wait", "port": "next", "to": "end"}]}})),
        )
        .await;
        let second = f.lead("POST", &format!("/{flow}/publish"), None).await;
        assert_eq!(second.status, StatusCode::CREATED);
        let failed = enrollment_in(&f, &flow, "failed", "wait", Some("wait")).await;
        let controls = [
            format!("/{flow}/enrollments/{failed}/retry"),
            format!("/{flow}/enrollments/{failed}/skip"),
            format!("/{flow}/versions/1/exit-enrollments"),
        ];

        for (action, code) in [("pause", "flow_paused"), ("archive", "flow_archived")] {
            let moved = f.lead("POST", &format!("/{flow}/{action}"), None).await;
            assert_eq!(moved.status, StatusCode::OK, "{action}: {:?}", moved.body);
            let written = "SELECT (SELECT count(*) FROM audit_log \
                                   WHERE action LIKE 'flow.enrollment%') \
                          + (SELECT count(*) FROM decision_records \
                                   WHERE action = 'flow.enrollment') \
                          + (SELECT count(*) FROM jobs)";
            let before = f.count(written).await;
            for tail in &controls {
                let reply = f.lead("POST", tail, None).await;
                assert_eq!(reply.status, StatusCode::CONFLICT, "{action} {tail}");
                assert_eq!(reply.body["code"], code, "{action} {tail}");
            }
            assert_eq!(
                f.count(written).await,
                before,
                "{action}: a refusal writes no audit entry, decision or job"
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_bulk_exit_that_waits_on_a_pausing_flow_is_refused_and_writes_nothing(pool: PgPool) {
        let f = fixture(&pool).await;
        let flow = f.live("Welcome", manual()).await;
        f.lead(
            "PATCH",
            &format!("/{flow}"),
            Some(json!({"graph": {"v": 1, "nodes": [
                {"id": "wait", "type": "wait.duration", "seconds": 3600},
                {"id": "end", "type": "exit"}
            ], "edges": [{"id": "e1", "from": "wait", "port": "next", "to": "end"}]}})),
        )
        .await;
        let second = f.lead("POST", &format!("/{flow}/publish"), None).await;
        assert_eq!(second.status, StatusCode::CREATED);
        let flow_id: Uuid = flow.parse().unwrap();

        // A pause that has the flow row locked and has not committed yet.
        let mut pausing = zirv_flow_store::begin(&pool).await.unwrap();
        zirv_flow_store::lock_flow(&mut pausing, f.ws, flow_id)
            .await
            .unwrap();
        sqlx::query("UPDATE flows SET state = 'paused' WHERE id = $1")
            .bind(flow_id)
            .execute(&mut *pausing)
            .await
            .unwrap();

        let exit = {
            let router = f.router.clone();
            let uri = f.flows(&format!("/{flow}/versions/1/exit-enrollments"));
            let login = Login {
                cookie: f.lead.cookie.clone(),
                csrf: f.lead.csrf.clone(),
            };
            tokio::spawn(
                async move { send(&router, "POST", &uri, None, Auth::Session(&login)).await },
            )
        };
        // Wait until the exit is blocked on the flow row.
        let mut blocked = false;
        for _ in 0..200 {
            if exit.is_finished() {
                break;
            }
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity                  WHERE datname = current_database() AND pid <> pg_backend_pid()                    AND wait_event_type = 'Lock' AND query ILIKE '%FROM flows%'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting > 0 {
                blocked = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(blocked, "the bulk exit should wait for the flow row");
        zirv_flow_store::commit(pausing).await.unwrap();

        let reply = exit.await.unwrap();
        assert_eq!(reply.status, StatusCode::CONFLICT, "{:?}", reply.body);
        assert_eq!(reply.body["code"], "flow_paused");
        assert_eq!(f.count("SELECT count(*) FROM jobs").await, 0);
        assert_eq!(
            f.count("SELECT count(*) FROM audit_log WHERE action = 'flow.enrollments_exited'")
                .await,
            0
        );
        assert_eq!(
            f.count("SELECT count(*) FROM decision_records WHERE action = 'flow.enrollment'")
                .await,
            0
        );
    }

    async fn end_enrollment(f: &Fixture, id: &str) {
        sqlx::query(
            "UPDATE enrollments SET state = 'completed', ended_at = now() WHERE id = $1::uuid",
        )
        .bind(id)
        .execute(&f.pool)
        .await
        .unwrap();
    }

    async fn step_row(f: &Fixture, enrollment: &str, node: &str, outcome: &str) {
        sqlx::query(
            "INSERT INTO step_executions (id, workspace_id, enrollment_id, node_id, outcome, error) \
             VALUES ($1, $2, $3::uuid, $4, $5, CASE WHEN $5 = 'failed' THEN 'boom' END)",
        )
        .bind(Uuid::now_v7())
        .bind(f.ws.as_uuid())
        .bind(enrollment)
        .bind(node)
        .bind(outcome)
        .execute(&f.pool)
        .await
        .unwrap();
    }

    /// A published two-node flow (`wait`, then `end`) and its id.
    async fn published(f: &Fixture, name: &str) -> String {
        let flow = f.create(name, simple_graph(), manual()).await;
        let reply = f.lead("POST", &format!("/{flow}/publish"), None).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
        flow
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_inspector_routes_need_read_and_stay_in_their_workspace(pool: PgPool) {
        let f = fixture(&pool).await;
        let flow = published(&f, "Welcome").await;
        let enrollment = enrollment_in(&f, &flow, "waiting", "wait", None).await;
        let routes = [
            format!("/{flow}/enrollments"),
            format!("/{flow}/enrollments/{enrollment}"),
            format!("/{flow}/versions/1/counts"),
        ];
        for tail in &routes {
            for (who, auth, expected) in [
                ("anonymous", Auth::Anonymous, StatusCode::UNAUTHORIZED),
                ("read key", Auth::Key(&f.read), StatusCode::OK),
                ("write key", Auth::Key(&f.write), StatusCode::FORBIDDEN),
                ("member", Auth::Session(&f.member), StatusCode::OK),
                ("workspace admin", Auth::Session(&f.lead), StatusCode::OK),
                (
                    "other workspace's admin key",
                    Auth::Key(&f.other_admin),
                    StatusCode::NOT_FOUND,
                ),
            ] {
                let reply = f.call("GET", &f.flows(tail), None, auth).await;
                assert_eq!(reply.status, expected, "{tail} as {who}: {:?}", reply.body);
            }
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_enrollment_list_pages_without_duplicates_or_gaps_filters_and_refuses_bad_input(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let flow = published(&f, "Welcome").await;
        let mut created = Vec::new();
        for (state, node, failed) in [
            ("failed", "wait", Some("end")),
            ("waiting", "wait", None),
            ("waiting", "wait", None),
            ("completed", "end", None),
            ("waiting", "end", None),
        ] {
            let id = enrollment_in(
                &f,
                &flow,
                if state == "completed" {
                    "active"
                } else {
                    state
                },
                node,
                failed,
            )
            .await;
            if state == "completed" {
                end_enrollment(&f, &id).await;
            }
            created.push(id);
        }
        let list = |query: &str| format!("/{flow}/enrollments{query}");

        let mut seen = Vec::new();
        let mut query = "?limit=2".to_owned();
        for page in 0..3 {
            let reply = f.lead("GET", &list(&query), None).await;
            assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
            let items = reply.body["enrollments"].as_array().unwrap();
            seen.extend(
                items
                    .iter()
                    .map(|item| item["id"].as_str().unwrap().to_owned()),
            );
            match reply.body["next_before"].as_str() {
                Some(next) => query = format!("?limit=2&before={next}"),
                None => {
                    assert_eq!(page, 2, "three pages of 2, 2 and 1");
                    break;
                }
            }
        }
        let mut newest_first = created.clone();
        newest_first.reverse();
        assert_eq!(seen, newest_first, "newest first, no duplicate, no gap");

        let ids = |reply: &Reply| -> Vec<String> {
            reply.body["enrollments"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["id"].as_str().unwrap().to_owned())
                .collect()
        };
        let waiting = f.lead("GET", &list("?state=waiting"), None).await;
        assert_eq!(
            ids(&waiting),
            [created[4].clone(), created[2].clone(), created[1].clone()]
        );
        let at_end = f.lead("GET", &list("?node=end"), None).await;
        let mut found = ids(&at_end);
        found.sort();
        let mut want = vec![created[0].clone(), created[3].clone(), created[4].clone()];
        want.sort();
        assert_eq!(
            found, want,
            "a node filter matches the node and the failed node"
        );
        let first = f.lead("GET", &list("?version=1&limit=200"), None).await;
        assert_eq!(ids(&first).len(), 5);
        let none = f.lead("GET", &list("?version=2"), None).await;
        assert!(ids(&none).is_empty());
        let item = &waiting.body["enrollments"][0];
        assert_eq!(item["state"], "waiting");
        assert_eq!(item["version"], 1);
        assert_eq!(item["trace_id"].as_str().unwrap().len(), 32);

        for bad in [
            "?limit=0",
            "?limit=201",
            "?before=nonsense",
            "?state=asleep",
            "?version=0",
            "?version=x",
            "?node=",
        ] {
            let reply = f.lead("GET", &list(bad), None).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
        }
        let unknown = f
            .lead("GET", &format!("/{}/enrollments", Uuid::now_v7()), None)
            .await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_enrollments_path_has_its_steps_waits_and_decisions_and_is_404_elsewhere(
        pool: PgPool,
    ) {
        use zirv_flow_core::{Cause, TraceContext, TraceId};
        use zirv_flow_store::{GraphPoint, NewDecision, append_decision};

        let f = fixture(&pool).await;
        let flow = published(&f, "Welcome").await;
        let other_flow = published(&f, "Other").await;
        let mine = enrollment_in(&f, &flow, "waiting", "wait", None).await;
        let theirs = enrollment_in(&f, &other_flow, "waiting", "wait", None).await;
        step_row(&f, &mine, "wait", "waiting").await;
        sqlx::query(
            "UPDATE enrollments SET wake_at = now() + interval '1 minute' WHERE id = $1::uuid",
        )
        .bind(&mine)
        .execute(&pool)
        .await
        .unwrap();
        let (flow_id, mine_id): (Uuid, Uuid) = (flow.parse().unwrap(), mine.parse().unwrap());
        let trace = TraceContext {
            trace_id: TraceId::from_uuid(mine_id),
            cause: Cause::api("t"),
        };
        for (action, outcome, reason) in [
            ("flow.step", "waiting", Some("wait.duration")),
            ("flow.wait", "waiting", Some("duration")),
        ] {
            append_decision(
                &pool,
                &NewDecision {
                    graph: Some(GraphPoint {
                        version: 1,
                        node: Some("wait".into()),
                    }),
                    workspace_id: f.ws,
                    trace: trace.clone(),
                    action,
                    outcome,
                    reason,
                    subject_ids: vec![("flow", flow_id), ("enrollment", mine_id)],
                    job: None,
                },
            )
            .await
            .unwrap();
        }

        let reply = f
            .lead("GET", &format!("/{flow}/enrollments/{mine}"), None)
            .await;

        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        assert_eq!(reply.body["enrollment"]["id"], mine.as_str());
        assert_eq!(reply.body["steps"][0]["node_id"], "wait");
        assert_eq!(reply.body["steps"][0]["node_type"], "wait.duration");
        assert_eq!(reply.body["waits"][0]["kind"], "timer");
        assert!(reply.body["waits"][0]["wake_at"].is_string());
        let decisions = reply.body["decisions"].as_array().unwrap();
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0]["action"], "flow.step");
        assert_eq!(decisions[0]["reason"], "wait.duration");
        assert_eq!(decisions[0]["cause"]["kind"], "api");
        assert_eq!(decisions[0]["subject_ids"]["node"], "wait");
        assert_eq!(decisions[0]["subject_ids"]["version"], 1);
        // The trace endpoint returns the same decisions, with the same subjects.
        let traced = f
            .call(
                "GET",
                &format!(
                    "/api/v1/workspaces/{}/traces/{}",
                    f.ws.as_uuid(),
                    reply.body["enrollment"]["trace_id"].as_str().unwrap()
                ),
                None,
                Auth::Key(&f.read),
            )
            .await;
        assert_eq!(traced.status, StatusCode::OK, "{:?}", traced.body);
        assert_eq!(traced.body["records"], reply.body["decisions"]);

        // Another flow's enrollment, a missing one and another workspace's are not found.
        let wrong_flow = f
            .lead("GET", &format!("/{flow}/enrollments/{theirs}"), None)
            .await;
        assert_eq!(wrong_flow.status, StatusCode::NOT_FOUND);
        let missing = f
            .lead(
                "GET",
                &format!("/{flow}/enrollments/{}", Uuid::now_v7()),
                None,
            )
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        let their_read = admin::create_api_key(
            &pool,
            &Context::system(),
            f.other_ws,
            "read",
            &[Scope::Read],
        )
        .await
        .unwrap()
        .plaintext;
        let elsewhere = f
            .call(
                "GET",
                &format!(
                    "/api/v1/workspaces/{}/flows/{flow}/enrollments/{mine}",
                    f.other_ws.as_uuid()
                ),
                None,
                Auth::Key(&their_read),
            )
            .await;
        assert_eq!(elsewhere.status, StatusCode::NOT_FOUND);
        let malformed = f
            .lead("GET", &format!("/{flow}/enrollments/nope"), None)
            .await;
        assert_eq!(
            malformed.status,
            StatusCode::NOT_FOUND,
            "a malformed path id is not found, as everywhere"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_counts_of_a_version_match_a_hand_count_per_node(pool: PgPool) {
        let f = fixture(&pool).await;
        let segment = f
            .lead_segment(
                "Pros",
                json!({"v": 1, "trait": "plan", "op": "eq", "value": "pro"}),
            )
            .await;
        let graph = json!({"v": 1, "nodes": [
            {"id": "wait", "type": "wait.duration", "seconds": 60},
            {"id": "win", "type": "goal", "exit": false},
            {"id": "end", "type": "exit"}
        ], "edges": [
            {"id": "e1", "from": "wait", "port": "next", "to": "win"},
            {"id": "e2", "from": "win", "port": "next", "to": "end"}
        ]});
        let settings = json!({
            "trigger": {"type": "manual"},
            "reentry": {"mode": "once"},
            "goal": {"type": "segment", "segment_id": segment}
        });
        let flow = f.create("Goals", graph, settings).await;
        let published = f.lead("POST", &format!("/{flow}/publish"), None).await;
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        let converted = enrollment_in(&f, &flow, "active", "end", None).await;
        end_enrollment(&f, &converted).await;
        let asleep = enrollment_in(&f, &flow, "waiting", "wait", None).await;
        let stuck = enrollment_in(&f, &flow, "failed", "wait", Some("wait")).await;
        for (enrollment, node, outcome) in [
            (&converted, "wait", "done"),
            (&converted, "win", "done"),
            (&converted, "end", "done"),
            (&converted, "settings.goal", "done"),
            (&asleep, "wait", "waiting"),
            (&stuck, "wait", "failed"),
        ] {
            step_row(&f, enrollment, node, outcome).await;
        }

        let reply = f
            .lead("GET", &format!("/{flow}/versions/1/counts"), None)
            .await;

        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        let row = |node: &str| -> Vec<i64> {
            let nodes = reply.body["nodes"].as_array().unwrap();
            let found = nodes.iter().find(|n| n["node_id"] == node).unwrap();
            [
                "entered",
                "completed",
                "waiting",
                "failed",
                "goal_conversions",
            ]
            .iter()
            .map(|key| found[*key].as_i64().unwrap())
            .collect()
        };
        assert_eq!(row("wait"), [3, 1, 1, 1, 0]);
        assert_eq!(row("win"), [1, 1, 0, 0, 1]);
        assert_eq!(row("end"), [1, 1, 0, 0, 0]);
        assert_eq!(row("settings.goal"), [1, 1, 0, 0, 1]);
        let ids: Vec<&str> = reply.body["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["node_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["wait", "win", "end", "settings.goal"]);
        assert_eq!(reply.body["nodes"][1]["node_type"], "goal");
        assert_eq!(reply.body["nodes"][0]["sent"], 0);
        assert_eq!(reply.body["nodes"][0]["send_failed"], 0);
        assert_eq!(reply.body["nodes"][0]["skipped"], serde_json::json!([]));

        let unknown = f
            .lead("GET", &format!("/{flow}/versions/9/counts"), None)
            .await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        let malformed = f
            .lead("GET", &format!("/{flow}/versions/x/counts"), None)
            .await;
        assert_eq!(
            malformed.status,
            StatusCode::NOT_FOUND,
            "a malformed path id is not found, as everywhere"
        );
    }
}
