//! Test mode end to end, driven through the router: permissions, a walk over a draft, a version
//! and a real person, the replay, the stop on a capped condition, the bounds, and the guarantee
//! that nothing is written.

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use zirv_flow_core::{Role, Scope, WorkspaceId};

    use crate::admin::{self, Context};
    use crate::{AppState, router};

    const PASSWORD: &str = "correct horse battery";

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
        read: String,
        write: String,
        admin: String,
        /// One key holding both write and admin.
        full: String,
        other_admin: String,
        member: Login,
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

    async fn fixture_with(pool: &PgPool, state: AppState) -> Fixture {
        let ws = workspace(pool, "ours").await;
        let other_ws = workspace(pool, "theirs").await;
        let router = router(state);
        let key = |workspace: WorkspaceId, scope: Scope| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), workspace, "test", &[scope])
                    .await
                    .unwrap()
                    .plaintext
            }
        };
        let read = key(ws, Scope::Read).await;
        let write = key(ws, Scope::Write).await;
        let admin_key = key(ws, Scope::Admin).await;
        let full = admin::create_api_key(
            pool,
            &Context::system(),
            ws,
            "full",
            &[Scope::Write, Scope::Admin],
        )
        .await
        .unwrap()
        .plaintext;
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
        let fixture = Fixture {
            pool: pool.clone(),
            router,
            ws,
            read,
            write,
            admin: admin_key,
            full,
            other_admin,
            member,
            lead,
        };
        fixture.define_catalog().await;
        fixture
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        zirv_flow_store::migrate(pool).await.unwrap();
        let state = AppState::new(pool.clone(), std::sync::Arc::new(String::new));
        fixture_with(pool, state).await
    }

    impl Fixture {
        fn uri(&self, flow: &str) -> String {
            format!("/api/v1/workspaces/{}/flows/{flow}/test", self.ws.as_uuid())
        }

        async fn lead(&self, method: &str, uri: &str, body: Option<Value>) -> Reply {
            send(&self.router, method, uri, body, Auth::Session(&self.lead)).await
        }

        /// The trait `plan` and the event `order` (with `total`), as a workspace defines them.
        async fn define_catalog(&self) {
            let ws = self.ws.as_uuid();
            zirv_flow_store::create_trait_definition(
                &self.pool,
                self.ws,
                &zirv_flow_store::NewTraitDefinition {
                    key: "plan".into(),
                    trait_type: zirv_flow_core::people::TraitType::String,
                    precedence: zirv_flow_core::people::Precedence::Latest,
                    source_order: Vec::new(),
                    pii: false,
                    channel_address: false,
                    inferred: false,
                    description: String::new(),
                },
            )
            .await
            .unwrap();
            let schemas = format!("/api/v1/workspaces/{ws}/event-schemas");
            let made = self
                .lead(
                    "POST",
                    &schemas,
                    Some(json!({"name": "order", "mode": "block", "properties": [
                        {"name": "total", "type": "number", "required": true}]})),
                )
                .await;
            assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
            let published = self
                .lead("POST", &format!("{schemas}/order/versions"), None)
                .await;
            assert_eq!(
                published.status,
                StatusCode::CREATED,
                "{:?}",
                published.body
            );
        }

        async fn create_flow(&self, name: &str, graph: Value) -> String {
            let reply = self
                .lead(
                    "POST",
                    &format!("/api/v1/workspaces/{}/flows", self.ws.as_uuid()),
                    Some(json!({"name": name, "graph": graph})),
                )
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().to_owned()
        }

        /// Every table's row count, so a test can show that nothing was written.
        async fn counts(&self) -> Vec<(String, i64)> {
            sqlx::query_as(
                "SELECT table_name::text, (xpath('/row/c/text()', query_to_xml(\
                    format('select count(*) as c from %I.%I', table_schema, table_name), \
                    false, true, '')))[1]::text::bigint \
                 FROM information_schema.tables \
                 WHERE table_schema = 'public' AND table_type = 'BASE TABLE' ORDER BY 1",
            )
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }
    }

    /// Wait, set a trait, branch on it, wait for an order or its timeout, then a goal and the
    /// end; a timeout calls a webhook.
    fn journey(webhook: bool) -> Value {
        let mut nodes = json!([
            {"id": "w1", "type": "wait.duration", "seconds": 60},
            {"id": "set", "type": "action.update_trait", "key": "plan",
             "change": {"op": "set", "value": "pro"}},
            {"id": "b", "type": "branch",
             "conditions": [{"trait": "plan", "op": "eq", "value": "pro"}]},
            {"id": "sp", "type": "split.random", "arms": [{"weight": 50}, {"weight": 50}]},
            {"id": "ev", "type": "wait.until_event", "event": "order",
             "filters": [{"prop": "total", "op": "gt", "value": 100}],
             "timeout_seconds": 600},
            {"id": "won", "type": "goal"},
            {"id": "end", "type": "exit"},
            {"id": "other", "type": "exit"},
            {"id": "lost", "type": "goal", "exit": true}
        ]);
        let timeout_to = if webhook { "hook" } else { "lost" };
        if webhook {
            nodes.as_array_mut().unwrap().push(json!({
                "id": "hook", "type": "action.webhook", "url": "https://example.com/hook",
                "trait_keys": ["plan"]
            }));
        }
        let mut edges = vec![
            ("w1", "next", "set"),
            ("set", "next", "b"),
            ("b", "case_0", "sp"),
            ("b", "else", "other"),
            ("sp", "arm_0", "ev"),
            ("sp", "arm_1", "ev"),
            ("ev", "event", "won"),
            ("ev", "timeout", timeout_to),
            ("won", "next", "end"),
        ];
        if webhook {
            edges.push(("hook", "next", "lost"));
        }
        let edges: Vec<Value> = edges
            .iter()
            .enumerate()
            .map(|(i, (from, port, to))| json!({"id": format!("e{i}"), "from": from, "port": port, "to": to}))
            .collect();
        json!({"v": 1, "nodes": nodes, "edges": edges})
    }

    fn path_nodes(body: &Value) -> Vec<(String, String)> {
        body["path"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| {
                (
                    step["node"].as_str().unwrap().to_owned(),
                    step["outcome"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(node, outcome)| ((*node).to_owned(), (*outcome).to_owned()))
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn test_mode_needs_write_and_a_real_person_needs_the_right_to_see_values(pool: PgPool) {
        let f = fixture(&pool).await;
        let flow = f.create_flow("Journey", journey(false)).await;
        let body = json!({"snapshot": {}});
        let credentials: [(&str, Auth<'_>, StatusCode); 8] = [
            ("anonymous", Auth::Anonymous, StatusCode::UNAUTHORIZED),
            ("read key", Auth::Key(&f.read), StatusCode::FORBIDDEN),
            ("write key", Auth::Key(&f.write), StatusCode::OK),
            (
                "admin key alone",
                Auth::Key(&f.admin),
                StatusCode::FORBIDDEN,
            ),
            ("write and admin key", Auth::Key(&f.full), StatusCode::OK),
            ("member", Auth::Session(&f.member), StatusCode::OK),
            ("workspace admin", Auth::Session(&f.lead), StatusCode::OK),
            (
                "other workspace's admin key",
                Auth::Key(&f.other_admin),
                StatusCode::NOT_FOUND,
            ),
        ];
        for (who, auth, expected) in credentials {
            let reply = send(&f.router, "POST", &f.uri(&flow), Some(body.clone()), auth).await;
            assert_eq!(reply.status, expected, "{who}: {:?}", reply.body);
        }

        // A real person's values are returned in full, so Write alone is not enough.
        let person = json!({"person_id": uuid::Uuid::now_v7()});
        for (who, auth) in [
            ("write key", Auth::Key(&f.write)),
            ("member", Auth::Session(&f.member)),
        ] {
            let reply = send(&f.router, "POST", &f.uri(&flow), Some(person.clone()), auth).await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{who}");
            assert_eq!(reply.body["code"], "flow_test_values_forbidden", "{who}");
        }
        // An admin gets as far as the person, who does not exist.
        let reply = send(
            &f.router,
            "POST",
            &f.uri(&flow),
            Some(person),
            Auth::Key(&f.full),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{:?}", reply.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_draft_is_walked_through_waits_a_branch_a_split_an_event_and_a_timeout(pool: PgPool) {
        let f = fixture(&pool).await;
        let flow = f.create_flow("Journey", journey(true)).await;

        // The first order is too small for the filter; the second wakes the wait.
        let arrives = f
            .lead(
                "POST",
                &f.uri(&flow),
                Some(
                    json!({"snapshot": {}, "start_at": "2030-01-01T00:00:00Z", "script": [
                        {"inject": {"name": "order", "properties": {"total": 5},
                                    "at": "2030-01-01T00:02:00Z"}},
                        {"inject": {"name": "order", "properties": {"total": 500},
                                    "at": "2030-01-01T00:03:00Z"}}
                    ]}),
                ),
            )
            .await;
        assert_eq!(arrives.status, StatusCode::OK, "{:?}", arrives.body);
        let body = &arrives.body;
        assert_eq!(body["source"], "draft");
        assert_eq!(body["version"], Value::Null);
        assert_eq!(
            path_nodes(body),
            pairs(&[
                ("w1", "passed"),
                ("set", "passed"),
                ("b", "branched"),
                ("sp", "passed"),
                ("ev", "passed"),
                ("won", "passed"),
                ("end", "completed"),
            ])
        );
        assert_eq!(body["path"][0]["woke_by"], "timer");
        assert_eq!(body["path"][0]["woke_at"], "2030-01-01T00:01:00Z");
        assert_eq!(body["path"][2]["branch"][0]["outcome"], "pass");
        assert_eq!(
            body["path"][2]["branch"][0]["observed"][0]["observed"],
            json!({"value": "pro"})
        );
        assert_eq!(body["path"][4]["woke_by"], "event");
        assert_eq!(body["script"][0]["outcome"], "event_ignored");
        assert_eq!(body["script"][1]["outcome"], "event_delivered");
        assert_eq!(body["final_state"]["status"], "completed");
        assert_eq!(body["halted"], Value::Null);
        assert_eq!(body["waits"].as_array().unwrap().len(), 2);
        assert_eq!(body["wakes"][0]["fired"], true);
        assert_eq!(body["internal_effects"][0]["kind"], "update_trait");
        assert_eq!(body["internal_effects"][1]["kind"], "goal");
        assert_eq!(body["person_after"]["traits"]["plan"], "pro");
        // The snapshot returned is the one the walk started from; injected events are not in it.
        assert_eq!(body["snapshot"]["events"], json!([]));

        // Nothing arrives: the timeout edge calls the webhook, and the goal exits.
        let times_out = f
            .lead(
                "POST",
                &f.uri(&flow),
                Some(json!({"snapshot": {}, "start_at": "2030-01-01T00:00:00Z",
                    "script": [{"advance_to": "2030-01-02T00:00:00Z"}]})),
            )
            .await;
        assert_eq!(times_out.status, StatusCode::OK, "{:?}", times_out.body);
        let body = &times_out.body;
        assert_eq!(body["path"][4]["woke_by"], "timeout");
        assert_eq!(body["final_state"]["status"], "exited");
        let [hook] = body["external_effects"].as_array().unwrap().as_slice() else {
            panic!("one webhook");
        };
        let enrollment = body["enrollment_id"].as_str().unwrap();
        assert_eq!(
            hook["effect_key"],
            json!(format!("{enrollment}:hook:webhook"))
        );
        assert_eq!(hook["detail"]["url"], "https://example.com/hook");
        assert_eq!(hook["detail"]["envelope"]["flow_id"], flow);
        assert_eq!(hook["detail"]["envelope"]["traits"], json!({"plan": "pro"}));
        let text = body.to_string();
        assert!(!text.contains("signature") && !text.contains("secret"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_published_version_and_an_inline_graph_are_walked_and_a_replay_repeats(pool: PgPool) {
        let f = fixture(&pool).await;
        let flow = f.create_flow("Journey", journey(false)).await;
        let publish = f
            .lead(
                "POST",
                &format!("/api/v1/workspaces/{}/flows/{flow}/publish", f.ws.as_uuid()),
                None,
            )
            .await;
        assert_eq!(publish.status, StatusCode::CREATED, "{:?}", publish.body);
        // The draft moves on; the version keeps the graph it was published with.
        let patched = f
            .lead(
                "PATCH",
                &format!("/api/v1/workspaces/{}/flows/{flow}", f.ws.as_uuid()),
                Some(
                    json!({"graph": {"v": 1, "nodes": [{"id": "x", "type": "exit"}], "edges": []}}),
                ),
            )
            .await;
        assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);

        let request = json!({"version": 1, "snapshot": {"traits": {"plan": "free"}},
            "script": [{"advance_to": "2999-01-01T00:00:00Z"}]});
        let first = f.lead("POST", &f.uri(&flow), Some(request)).await;
        assert_eq!(first.status, StatusCode::OK, "{:?}", first.body);
        assert_eq!(first.body["source"], "version");
        assert_eq!(first.body["version"], 1);
        assert_eq!(first.body["path"][0]["node"], "w1");
        // The wait set the plan to pro whatever the snapshot said.
        assert_eq!(first.body["person_after"]["traits"]["plan"], "pro");
        assert_eq!(first.body["snapshot"]["traits"]["plan"], "free");

        // Re-posting what the response returned gives the same response.
        let replay = f
            .lead(
                "POST",
                &f.uri(&flow),
                Some(json!({
                    "version": 1,
                    "snapshot": first.body["snapshot"],
                    "start_at": first.body["start_at"],
                    "enrollment_id": first.body["enrollment_id"],
                    "script": [{"advance_to": "2999-01-01T00:00:00Z"}]
                })),
            )
            .await;
        assert_eq!(replay.status, StatusCode::OK, "{:?}", replay.body);
        assert_eq!(replay.body, first.body);

        // An inline graph, and the saved draft (now one exit node).
        let inline = f
            .lead(
                "POST",
                &f.uri(&flow),
                Some(json!({"graph": journey(false), "snapshot": {}})),
            )
            .await;
        assert_eq!(inline.body["source"], "inline", "{:?}", inline.body);
        assert_eq!(inline.body["path"][0]["node"], "w1");
        let draft = f
            .lead("POST", &f.uri(&flow), Some(json!({"snapshot": {}})))
            .await;
        assert_eq!(draft.body["source"], "draft");
        assert_eq!(path_nodes(&draft.body), pairs(&[("x", "completed")]));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_real_person_is_read_and_returned_as_the_snapshot_that_replays(pool: PgPool) {
        let f = fixture(&pool).await;
        let ws = f.ws;
        let mut record = zirv_flow_store::ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: "1".into(),
            identifiers: vec![("email".into(), "a@example.com".into())],
            traits: serde_json::Map::new(),
            observed_at: time::macros::datetime!(2026-01-01 00:00 UTC),
            trace: zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("seed")),
        };
        record.traits.insert("plan".into(), json!("pro"));
        zirv_flow_store::apply_profile_batch(&pool, ws, &[record])
            .await
            .unwrap();
        let person: uuid::Uuid = sqlx::query_scalar(
            "SELECT person_id FROM identities WHERE value = 'a@example.com' AND workspace_id = $1",
        )
        .bind(ws.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
                source, trace_id, validation_state, properties) \
             VALUES (gen_random_uuid(), $1, $2, 'order', now() - interval '1 hour', now(), \
                'test', gen_random_uuid(), 'valid', '{\"total\": 42}')",
        )
        .bind(ws.as_uuid())
        .bind(person)
        .execute(&pool)
        .await
        .unwrap();
        let graph = json!({"v": 1, "nodes": [
            {"id": "b", "type": "branch", "conditions": [
                {"trait": "plan", "op": "eq", "value": "pro"},
                {"event": "order"}
            ]},
            {"id": "hook", "type": "action.webhook", "url": "https://example.com/hook"},
            {"id": "pro", "type": "exit"},
            {"id": "buyer", "type": "exit"},
            {"id": "none", "type": "exit"}
        ], "edges": [
            {"id": "e0", "from": "hook", "port": "next", "to": "pro"},
            {"id": "e1", "from": "b", "port": "case_0", "to": "hook"},
            {"id": "e2", "from": "b", "port": "case_1", "to": "buyer"},
            {"id": "e3", "from": "b", "port": "else", "to": "none"}
        ]});
        let flow = f.create_flow("Branches", graph).await;
        let before = f.counts().await;

        let first = send(
            &f.router,
            "POST",
            &f.uri(&flow),
            Some(json!({"person_id": person})),
            Auth::Key(&f.full),
        )
        .await;

        assert_eq!(first.status, StatusCode::OK, "{:?}", first.body);
        assert_eq!(first.body["person_id"], person.to_string());
        assert_eq!(first.body["snapshot"]["traits"]["plan"], "pro");
        assert_eq!(first.body["snapshot"]["identifier_kinds"], json!(["email"]));
        assert_eq!(first.body["snapshot"]["events"][0]["name"], "order");
        assert_eq!(
            first.body["snapshot"]["events"][0]["properties"]["total"],
            42
        );
        assert_eq!(
            path_nodes(&first.body),
            pairs(&[("b", "branched"), ("hook", "passed"), ("pro", "completed")])
        );

        let replay = send(
            &f.router,
            "POST",
            &f.uri(&flow),
            Some(json!({
                "snapshot": first.body["snapshot"],
                "start_at": first.body["start_at"],
                "enrollment_id": first.body["enrollment_id"],
            })),
            Auth::Key(&f.write),
        )
        .await;
        assert_eq!(replay.status, StatusCode::OK, "{:?}", replay.body);
        // The snapshot names the person, so the webhook envelope and the whole response repeat.
        assert_eq!(replay.body, first.body);
        let envelope = &first.body["external_effects"][0]["detail"]["envelope"];
        assert_eq!(envelope["person_id"], person.to_string());
        assert_eq!(first.body["snapshot"]["person_id"], person.to_string());
        assert_eq!(f.counts().await, before, "the reads wrote nothing");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_flow_settings_goal_and_exit_criteria_apply_to_the_walk(pool: PgPool) {
        let f = fixture(&pool).await;
        let flow = f.create_flow("Journey", journey(false)).await;
        let patched = f
            .lead(
                "PATCH",
                &format!("/api/v1/workspaces/{}/flows/{flow}", f.ws.as_uuid()),
                Some(json!({"settings": {
                    "trigger": {"type": "manual"}, "reentry": {"mode": "once"},
                    "goal": {"type": "event", "event": "order",
                             "filters": [{"prop": "total", "op": "gt", "value": 100}]},
                    "exit_on_goal": true,
                    "exit_criteria": [{"type": "leaves_segment"}]
                }})),
            )
            .await;
        assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);

        let reply = f
            .lead(
                "POST",
                &f.uri(&flow),
                Some(
                    json!({"snapshot": {}, "start_at": "2030-01-01T00:00:00Z", "script": [
                        {"inject": {"name": "order", "properties": {"total": 500},
                                    "at": "2030-01-01T00:03:00Z"}}
                    ]}),
                ),
            )
            .await;

        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        assert_eq!(reply.body["script"][0]["outcome"], "goal_exit");
        assert_eq!(reply.body["final_state"]["status"], "exited");
        let last = reply.body["path"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(last["type"], "settings.exit");
        assert_eq!(reply.body["not_simulated"], json!(["exit_leaves_segment"]));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_capped_condition_stops_with_unable_to_evaluate_and_the_partial_path(pool: PgPool) {
        let f = fixture(&pool).await;
        let graph = json!({"v": 1, "nodes": [
            {"id": "w", "type": "wait.duration", "seconds": 5},
            {"id": "b", "type": "branch", "conditions": [{"event": "order"}]},
            {"id": "yes", "type": "exit"},
            {"id": "no", "type": "exit"}
        ], "edges": [
            {"id": "e1", "from": "w", "port": "next", "to": "b"},
            {"id": "e2", "from": "b", "port": "case_0", "to": "yes"},
            {"id": "e3", "from": "b", "port": "else", "to": "no"}
        ]});
        let flow = f.create_flow("Capped", graph).await;

        let reply = f
            .lead(
                "POST",
                &f.uri(&flow),
                Some(json!({"snapshot": {"capped_events": ["order"]},
                    "script": [{"advance_to": "2999-01-01T00:00:00Z"}]})),
            )
            .await;

        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        assert_eq!(
            path_nodes(&reply.body),
            pairs(&[("w", "passed"), ("b", "unable_to_evaluate")])
        );
        let halted = &reply.body["halted"];
        assert_eq!(halted["reason"], "unable_to_evaluate");
        assert_eq!(halted["node"], "b");
        assert_eq!(halted["condition"], 0);
        assert_eq!(halted["capped_events"], json!(["order"]));
        assert_eq!(halted["cap"], zirv_flow_store::EXPLAIN_EVENT_CAP);
        assert_eq!(reply.body["final_state"]["status"], "awaiting_conditions");
        assert_eq!(
            reply.body["path"][1]["branch"][0]["outcome"], "unknown",
            "{:?}",
            reply.body["path"]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn no_row_is_written_by_any_walk_or_refusal(pool: PgPool) {
        let f = fixture(&pool).await;
        let flow = f.create_flow("Journey", journey(true)).await;
        let published = f.create_flow("Published", journey(false)).await;
        let publish = f
            .lead(
                "POST",
                &format!(
                    "/api/v1/workspaces/{}/flows/{published}/publish",
                    f.ws.as_uuid()
                ),
                None,
            )
            .await;
        assert_eq!(publish.status, StatusCode::CREATED, "{:?}", publish.body);
        let before = f.counts().await;
        let names: Vec<&str> = before.iter().map(|(name, _)| name.as_str()).collect();
        for table in [
            "flows",
            "flow_versions",
            "events",
            "jobs",
            "decision_records",
            "audit_log",
        ] {
            assert!(names.contains(&table), "{table} is not counted");
        }
        assert!(
            before
                .iter()
                .any(|(name, rows)| name == "flow_versions" && *rows == 1)
        );

        let script = json!([
            {"inject": {"name": "order", "properties": {"total": 500}, "at": "2999-01-01T00:02:00Z"}},
            {"advance_to": "2999-06-01T00:00:00Z"}
        ]);
        let requests = [
            (
                flow.clone(),
                json!({"snapshot": {}, "start_at": "2999-01-01T00:00:00Z", "script": script}),
            ),
            (
                flow.clone(),
                json!({"snapshot": {}, "start_at": "2999-01-01T00:00:00Z",
                "script": [{"advance_to": "2999-06-01T00:00:00Z"}]}),
            ),
            (published.clone(), json!({"version": 1, "snapshot": {}})),
            (
                flow.clone(),
                json!({"graph": {"v": 1, "nodes": [], "edges": []}, "snapshot": {}}),
            ),
            (
                flow.clone(),
                json!({"snapshot": {}, "person_id": uuid::Uuid::now_v7()}),
            ),
            (flow.clone(), json!({"version": 9, "snapshot": {}})),
        ];
        for (id, body) in requests {
            f.lead("POST", &f.uri(&id), Some(body)).await;
        }
        send(
            &f.router,
            "POST",
            &f.uri(&flow),
            Some(json!({"person_id": uuid::Uuid::now_v7()})),
            Auth::Key(&f.full),
        )
        .await;

        assert_eq!(f.counts().await, before);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_bounds_and_bad_requests_are_problems_with_stable_codes(pool: PgPool) {
        let f = fixture(&pool).await;
        let flow = f.create_flow("Journey", journey(false)).await;
        let step = json!({"advance_to": "2999-01-01T00:00:00Z"});
        let events: Vec<Value> = (0..=zirv_flow_store::EXPLAIN_EVENT_CAP)
            .map(|_| json!({"name": "o", "occurred_at": "2025-01-01T00:00:00Z"}))
            .collect();
        let cases: [(&str, Value, StatusCode, &str); 11] = [
            (
                "a script over the cap",
                json!({"snapshot": {}, "script": vec![step.clone(); 51]}),
                StatusCode::UNPROCESSABLE_ENTITY,
                "flow_test_script_too_long",
            ),
            (
                "a script that goes back in time",
                json!({"snapshot": {}, "start_at": "2999-01-01T00:00:00Z",
                    "script": [{"advance_to": "2998-01-01T00:00:00Z"}]}),
                StatusCode::UNPROCESSABLE_ENTITY,
                "flow_test_script_invalid",
            ),
            (
                "a step that is two things",
                json!({"snapshot": {}, "script": [{"advance_to": "2999-01-01T00:00:00Z",
                    "inject": {"name": "x", "at": "2999-01-01T00:00:00Z"}}]}),
                StatusCode::UNPROCESSABLE_ENTITY,
                "flow_test_script_invalid",
            ),
            (
                "a snapshot over the event cap",
                json!({"snapshot": {"events": events}}),
                StatusCode::UNPROCESSABLE_ENTITY,
                "flow_test_snapshot_too_large",
            ),
            (
                "no person",
                json!({}),
                StatusCode::UNPROCESSABLE_ENTITY,
                "flow_test_request_invalid",
            ),
            (
                "two people",
                json!({"snapshot": {}, "person_id": uuid::Uuid::now_v7()}),
                StatusCode::UNPROCESSABLE_ENTITY,
                "flow_test_request_invalid",
            ),
            (
                "a version and a graph",
                json!({"snapshot": {}, "version": 1, "graph": {"v": 1, "nodes": [], "edges": []}}),
                StatusCode::UNPROCESSABLE_ENTITY,
                "flow_test_request_invalid",
            ),
            (
                "a graph that is not a flow document",
                json!({"snapshot": {}, "graph": {"nodes": 1}}),
                StatusCode::UNPROCESSABLE_ENTITY,
                "flow_malformed",
            ),
            (
                "a graph that fails the structural checks",
                json!({"snapshot": {}, "graph": {"v": 1, "nodes": [], "edges": []}}),
                StatusCode::UNPROCESSABLE_ENTITY,
                "flow_invalid",
            ),
            (
                "a version that does not exist",
                json!({"snapshot": {}, "version": 7}),
                StatusCode::NOT_FOUND,
                "",
            ),
            (
                "an unknown snapshot field",
                json!({"snapshot": {"trait": {}}}),
                StatusCode::UNPROCESSABLE_ENTITY,
                "",
            ),
        ];
        for (label, body, status, code) in cases {
            let reply = f.lead("POST", &f.uri(&flow), Some(body)).await;
            assert_eq!(reply.status, status, "{label}: {:?}", reply.body);
            if !code.is_empty() {
                assert_eq!(reply.body["code"], code, "{label}");
            }
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn reading_a_real_person_that_stalls_is_a_503_with_its_own_code(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let state = AppState::new(pool.clone(), std::sync::Arc::new(String::new))
            .with_segment_preview_timeout(std::time::Duration::from_millis(100));
        let f = fixture_with(&pool, state).await;
        let flow = f.create_flow("Journey", journey(false)).await;
        let person = uuid::Uuid::now_v7();
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE people IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .unwrap();

        let reply = send(
            &f.router,
            "POST",
            &f.uri(&flow),
            Some(json!({"person_id": person})),
            Auth::Key(&f.full),
        )
        .await;
        drop(blocker);

        assert_eq!(
            reply.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{:?}",
            reply.body
        );
        assert_eq!(reply.body["code"], "flow_test_timeout");
    }
}
