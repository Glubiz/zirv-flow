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
        let after = f
            .lead("POST", &format!("/{id}/publish?dry_run=true"), None)
            .await;
        assert_eq!(after.body["next_version"], 2);
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
}
