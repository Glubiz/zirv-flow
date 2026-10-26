//! The snippets API end to end, driven through the router: the permission matrix, drafts and their
//! revision, publish and its immutability, validation, reference rows, where-used, the archive
//! guard, audit rows and preview through the render pool.

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;
    use zirv_flow_core::{Scope, WorkspaceId};
    use zirv_flow_render::{ChildCommand, PoolConfig, RenderPool};
    use zirv_flow_store::{NewAsset, sha256_hex};

    use crate::admin::{self, Context};
    use crate::{AppState, router};

    struct Reply {
        status: StatusCode,
        body: Value,
    }

    enum Auth<'a> {
        Anonymous,
        Key(&'a str),
    }

    struct Fixture {
        pool: PgPool,
        router: Router,
        ws: WorkspaceId,
        other_ws: WorkspaceId,
        read: String,
        write: String,
        admin: String,
        /// A key with all three scopes, for the setup and the checks around the matrix.
        all: String,
        other_admin: String,
    }

    /// The render host of the pool under test: a no-op unless the pool started it.
    #[test]
    fn render_host_child() {
        if std::env::var("ZIRV_FLOW_TEST_SNIPPET_HOST").is_err() {
            return;
        }
        std::process::exit(i32::from(zirv_flow_render::host::run(None)));
    }

    async fn start_pool() -> RenderPool {
        let command = ChildCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "snippets::acceptance::tests::render_host_child",
                "--nocapture",
            ])
            .args(["--test-threads", "1", "-q"])
            .env("ZIRV_FLOW_TEST_SNIPPET_HOST", "serve")
            .skip_stdout_until("running 1 test\n");
        RenderPool::start(
            command,
            PoolConfig {
                size: 1,
                deadline: Duration::from_secs(10),
            },
        )
        .await
        .unwrap()
    }

    async fn send(
        router: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
        auth: Auth<'_>,
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(uri);
        if let Auth::Key(key) = auth {
            request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
        }
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
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
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
        let key = |workspace: WorkspaceId, scopes: &'static [Scope]| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), workspace, "snippets", scopes)
                    .await
                    .unwrap()
                    .plaintext
            }
        };
        Fixture {
            pool: pool.clone(),
            router,
            ws,
            other_ws,
            read: key(ws, &[Scope::Read]).await,
            write: key(ws, &[Scope::Write]).await,
            admin: key(ws, &[Scope::Admin]).await,
            all: key(ws, &[Scope::Read, Scope::Write, Scope::Admin]).await,
            other_admin: key(other_ws, &[Scope::Admin]).await,
        }
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        fixture_with(pool, Some(start_pool().await)).await
    }

    impl Fixture {
        fn base(&self) -> String {
            format!("/api/v1/workspaces/{}", self.ws.as_uuid())
        }

        /// A call with the all-scopes key, to a path under the workspace.
        async fn admin(&self, method: &str, tail: &str, body: Option<Value>) -> Reply {
            send(
                &self.router,
                method,
                &format!("{}{tail}", self.base()),
                body,
                Auth::Key(&self.all),
            )
            .await
        }

        async fn create(&self, name: &str, content: Value) -> String {
            let reply = self
                .admin(
                    "POST",
                    "/snippets",
                    Some(json!({"name": name, "content": content})),
                )
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().to_owned()
        }

        async fn put_draft(&self, id: &str, content: Value, revision: Option<i32>) -> Reply {
            self.admin(
                "PUT",
                &format!("/snippets/{id}/draft"),
                Some(json!({"content": content, "revision": revision})),
            )
            .await
        }

        async fn publish(&self, id: &str) -> Reply {
            self.admin("POST", &format!("/snippets/{id}/publish"), None)
                .await
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        async fn asset(&self, name: &str, bytes: &[u8]) -> (Uuid, String) {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let stored = zirv_flow_store::store_asset(
                &mut tx,
                self.ws,
                &NewAsset {
                    name,
                    ext: "png",
                    content_type: "image/png",
                    bytes,
                    width: 2,
                    height: 3,
                    created_by: None,
                },
                1 << 30,
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            (stored.asset.id, sha256_hex(&stored.asset.sha256))
        }

        /// A block template whose draft holds a snippet block for `snippet` in a section column.
        async fn template_holding(&self, name: &str, snippet: &str) -> String {
            let document = json!({"schema": 2, "channel": "email", "mode": "blocks", "blocks": {
                "subject": "Hi", "category": "marketing", "sections": [{"id": "s", "columns": [[
                    {"type": "divider", "id": "d"},
                    {"type": "snippet", "id": "sn", "snippet_id": snippet}]]}]}});
            let reply = self
                .admin(
                    "POST",
                    "/templates",
                    Some(json!({"name": name, "channel": "email", "document": document})),
                )
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().to_owned()
        }

        /// A published flow whose graph names `template`; returns its id.
        async fn flow_naming(&self, name: &str, template: &str) -> Uuid {
            let flow = Uuid::now_v7();
            let mut connection = self.pool.acquire().await.unwrap();
            zirv_flow_store::create_flow(
                &mut connection,
                self.ws,
                &zirv_flow_store::NewFlow {
                    id: flow,
                    name: name.into(),
                    graph: graph_naming(Some(template)),
                    settings: json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}}),
                },
                None,
            )
            .await
            .unwrap();
            drop(connection);
            self.publish_flow(flow).await;
            flow
        }

        async fn publish_flow(&self, flow: Uuid) {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let locked = zirv_flow_store::lock_flow(&mut tx, self.ws, flow)
                .await
                .unwrap();
            zirv_flow_store::publish_flow(
                &mut tx,
                &zirv_flow_store::Clock::default(),
                self.ws,
                &locked,
                None,
                &zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t")),
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
        }
    }

    fn graph_naming(template: Option<&str>) -> Value {
        let mut node = json!({"id": "send", "type": "action.send"});
        if let Some(template) = template {
            node["template"] = json!(template);
        }
        json!({"v": 1, "nodes": [node, {"id": "end", "type": "exit"}], "edges": []})
    }

    fn heading(text: &str) -> Value {
        json!({"blocks": [{"type": "heading", "id": "h", "level": 2, "text": text}]})
    }

    fn issue_codes(reply: &Reply) -> Vec<String> {
        reply.body["template_issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|issue| issue["code"].as_str().unwrap().to_owned())
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn each_route_needs_its_permission_and_no_other(pool: PgPool) {
        let f = fixture(&pool).await;
        let snippet = f.create("Offer", heading("Hi")).await;
        let id = |tail: &str| format!("/snippets/{snippet}{tail}");
        let credentials: [(&str, Auth<'_>, u8); 5] = [
            ("anonymous", Auth::Anonymous, 0),
            ("read key", Auth::Key(&f.read), 1),
            ("write key", Auth::Key(&f.write), 2),
            ("admin key", Auth::Key(&f.admin), 3),
            ("other workspace's admin", Auth::Key(&f.other_admin), 9),
        ];
        // A key holds exactly one scope; the level a route needs is 1 read, 2 write, 3 admin.
        let requests: [(&str, &str, String, Option<Value>, u8); 11] = [
            ("list", "GET", "/snippets".into(), None, 1),
            ("get", "GET", id(""), None, 1),
            ("versions", "GET", id("/versions"), None, 1),
            ("where used", "GET", id("/where-used"), None, 1),
            (
                "create",
                "POST",
                "/snippets".into(),
                Some(json!({"name": "Other"})),
                2,
            ),
            ("update", "PUT", id(""), Some(json!({"name": "Renamed"})), 2),
            (
                "save draft",
                "PUT",
                id("/draft"),
                Some(json!({"content": heading("Again")})),
                2,
            ),
            ("preview", "POST", id("/preview"), Some(json!({})), 2),
            ("publish", "POST", id("/publish"), None, 3),
            ("get version", "GET", id("/versions/1"), None, 1),
            ("archive", "DELETE", id(""), None, 3),
        ];

        for (label, method, tail, body, need) in requests {
            for (who, auth, level) in &credentials {
                let auth = match auth {
                    Auth::Anonymous => Auth::Anonymous,
                    Auth::Key(key) => Auth::Key(key),
                };
                let reply = send(
                    &f.router,
                    method,
                    &format!("{}{tail}", f.base()),
                    body.clone(),
                    auth,
                )
                .await;
                match *level {
                    0 => assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{label} as {who}"),
                    9 => assert_eq!(reply.status, StatusCode::NOT_FOUND, "{label} as {who}"),
                    held if held == need => assert!(
                        !matches!(
                            reply.status,
                            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                        ),
                        "{label} as {who}: {} {:?}",
                        reply.status,
                        reply.body
                    ),
                    _ => assert_eq!(reply.status, StatusCode::FORBIDDEN, "{label} as {who}"),
                }
            }
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_publish_makes_version_two_and_version_one_never_changes(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Offer", heading("one")).await;

        let first = f.publish(&id).await;
        f.put_draft(&id, heading("two"), None).await;
        let second = f.publish(&id).await;
        let one = f
            .admin("GET", &format!("/snippets/{id}/versions/1"), None)
            .await;
        let list = f
            .admin("GET", &format!("/snippets/{id}/versions"), None)
            .await;
        let tamper = sqlx::query("UPDATE snippet_versions SET content = '{}'::jsonb")
            .execute(&f.pool)
            .await;

        assert_eq!(first.status, StatusCode::CREATED, "{:?}", first.body);
        assert_eq!(first.body["version"]["version"], 1);
        assert_eq!(second.body["version"]["version"], 2);
        assert_eq!(second.body["snippet"]["current_version"], 2);
        assert_eq!(one.body["content"], heading("one"));
        let numbers: Vec<i64> = list
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["version"].as_i64().unwrap())
            .collect();
        assert_eq!(numbers, [2, 1]);
        assert!(tamper.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn invalid_content_is_refused_at_publish_and_writes_nothing(pool: PgPool) {
        let f = fixture(&pool).await;
        let gone = Uuid::now_v7().to_string();
        let cases: [(&str, Value, &str); 6] = [
            (
                "a section",
                json!({"blocks": [{"type": "divider", "id": "d"}],
                       "sections": [{"id": "s", "columns": [[]]}]}),
                "snippet_section",
            ),
            (
                "a nested snippet block",
                json!({"blocks": [{"type": "snippet", "id": "n", "snippet_id": "x"}]}),
                "snippet_nested",
            ),
            (
                "visible_if",
                json!({"blocks": [{"type": "divider", "id": "d", "visible_if": {"all": []}}]}),
                "snippet_condition",
            ),
            (
                "an unknown merge field",
                json!({"blocks": [{"type": "text", "id": "t",
                                   "content": "<p>{{ person.nope }}</p>"}]}),
                "unknown_merge_field",
            ),
            (
                "a missing asset",
                json!({"blocks": [{"type": "image", "id": "i", "alt": "x",
                                   "src": {"asset": gone}}]}),
                "broken_asset_reference",
            ),
            ("no block at all", json!({"blocks": []}), "missing_content"),
        ];

        for (label, content, expected) in cases {
            let id = f.create(label, content).await;
            let refused = f.publish(&id).await;

            assert_eq!(
                refused.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{label}: {:?}",
                refused.body
            );
            assert_eq!(refused.body["code"], "snippet_invalid", "{label}");
            assert!(
                issue_codes(&refused).contains(&expected.to_owned()),
                "{label}: {:?}",
                refused.body
            );
        }
        assert_eq!(f.count("SELECT count(*) FROM snippet_versions").await, 0);
        assert_eq!(
            f.count("SELECT count(*) FROM audit_log WHERE action = 'snippet.published'")
                .await,
            0
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_theme_logo_image_publishes_and_previews_with_a_warning(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f
            .create(
                "Logo note",
                json!({"blocks": [{"type": "image", "id": "i", "alt": "x",
                                   "src": {"theme": "logo"}}]}),
            )
            .await;

        let preview = f
            .admin("POST", &format!("/snippets/{id}/preview"), Some(json!({})))
            .await;
        let published = f.publish(&id).await;

        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        assert_eq!(preview.status, StatusCode::OK, "{:?}", preview.body);
        assert_eq!(preview.body["rendered"], true, "{:?}", preview.body);
        assert!(preview.body["html"].is_string());
        assert_eq!(preview.body["issues"][0]["code"], "broken_theme_reference");
        assert_eq!(preview.body["issues"][0]["severity"], "warning");
        assert_eq!(preview.body["issues"][0]["at"], "/blocks/block/i/src");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_stale_draft_revision_conflicts_and_a_bad_shape_is_refused(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Offer", heading("one")).await;

        let second = f.put_draft(&id, heading("two"), Some(1)).await;
        let stale = f.put_draft(&id, heading("three"), Some(1)).await;
        let blind = f.put_draft(&id, heading("four"), None).await;
        let unknown = f
            .put_draft(
                &id,
                json!({"blocks": [{"type": "carousel", "id": "c"}]}),
                None,
            )
            .await;
        let text = f.put_draft(&id, json!("text"), None).await;
        let detail = f.admin("GET", &format!("/snippets/{id}"), None).await;

        assert_eq!(second.body["draft"]["revision"], 2, "{:?}", second.body);
        assert_eq!(stale.status, StatusCode::CONFLICT);
        assert_eq!(stale.body["code"], "snippet_revision_mismatch");
        assert_eq!(blind.body["draft"]["revision"], 3);
        assert_eq!(unknown.body["code"], "template_unknown_block");
        assert_eq!(text.body["code"], "snippet_malformed");
        assert_eq!(detail.body["draft"]["content"], heading("four"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn publish_writes_a_reference_row_for_each_asset_the_version_names(pool: PgPool) {
        let f = fixture(&pool).await;
        let (asset, _) = f.asset("photo", b"photo-bytes").await;
        let image = |id: &str| {
            json!({"type": "image", "id": id, "alt": "x",
                                      "src": {"asset": asset.to_string()}})
        };
        let id = f
            .create("Photos", json!({"blocks": [image("a"), image("b")]}))
            .await;

        let published = f.publish(&id).await;

        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        let rows: Vec<(String, Uuid, i32)> = sqlx::query_as(
            "SELECT owner_kind, owner_id, version FROM asset_references WHERE asset_id = $1",
        )
        .bind(asset)
        .fetch_all(&f.pool)
        .await
        .unwrap();
        assert_eq!(rows, [("snippet".to_owned(), id.parse().unwrap(), 1)]);
        let deleted = f.admin("DELETE", &format!("/assets/{asset}"), None).await;
        assert_eq!(deleted.status, StatusCode::CONFLICT, "{:?}", deleted.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn where_used_and_the_publish_response_list_templates_and_active_flows(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Offer", heading("Hi")).await;
        let other = f.create("Other", heading("Hi")).await;
        let kept = f.template_holding("Kept", &id).await;
        let archived = f.template_holding("Archived", &id).await;
        f.template_holding("Elsewhere", &other).await;
        let gone = f
            .admin("DELETE", &format!("/templates/{archived}"), None)
            .await;
        assert_eq!(gone.status, StatusCode::OK, "{:?}", gone.body);
        let running = f.flow_naming("Running", &kept).await;
        // This flow named the template in version 1 and stopped naming it in version 2.
        let moved_on = f.flow_naming("Moved on", &kept).await;
        let mut connection = f.pool.acquire().await.unwrap();
        zirv_flow_store::update_flow_draft(
            &mut connection,
            f.ws,
            moved_on,
            &zirv_flow_store::FlowPatch {
                graph: Some(graph_naming(None)),
                ..zirv_flow_store::FlowPatch::default()
            },
        )
        .await
        .unwrap();
        drop(connection);
        f.publish_flow(moved_on).await;

        let published = f.publish(&id).await;
        let used = f
            .admin("GET", &format!("/snippets/{id}/where-used"), None)
            .await;
        let unused = f
            .admin("GET", &format!("/snippets/{other}/where-used"), None)
            .await;

        for body in [&published.body["used_by"], &used.body] {
            let templates = body["templates"].as_array().unwrap();
            assert_eq!(templates.len(), 1, "{body:?}");
            assert_eq!(templates[0]["id"], json!(kept));
            assert_eq!(templates[0]["in_draft"], true);
            assert_eq!(body["themes"], json!([]));
            let flows = body["active_flows"].as_array().unwrap();
            assert_eq!(flows.len(), 1, "{body:?}");
            assert_eq!(flows[0]["flow_id"], json!(running));
            assert_eq!(flows[0]["template_ids"], json!([kept]));
        }
        assert_eq!(unused.body["templates"].as_array().unwrap().len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_archive_guard_names_the_template_drafts_that_hold_the_snippet(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Offer", heading("Hi")).await;
        let template = f.template_holding("Welcome", &id).await;

        let refused = f.admin("DELETE", &format!("/snippets/{id}"), None).await;
        let released = f
            .admin(
                "PUT",
                &format!("/templates/{template}/draft"),
                Some(
                    json!({"document": {"schema": 2, "channel": "email", "mode": "blocks",
                    "blocks": {"subject": "Hi", "category": "marketing"}}}),
                ),
            )
            .await;
        let archived = f.admin("DELETE", &format!("/snippets/{id}"), None).await;
        let after = f.put_draft(&id, heading("late"), None).await;

        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert_eq!(refused.body["code"], "snippet_in_use");
        assert_eq!(
            refused.body["snippet_templates"],
            json!([{"id": template, "name": "Welcome"}])
        );
        assert_eq!(released.status, StatusCode::OK, "{:?}", released.body);
        assert_eq!(archived.status, StatusCode::OK, "{:?}", archived.body);
        assert!(archived.body["archived_at"].is_string());
        assert_eq!(after.body["code"], "snippet_archived");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_live_name_is_taken_but_an_archived_one_is_free(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Offer", heading("Hi")).await;

        let clash = f
            .admin("POST", "/snippets", Some(json!({"name": "offer"})))
            .await;
        f.admin("DELETE", &format!("/snippets/{id}"), None).await;
        let reuse = f
            .admin("POST", "/snippets", Some(json!({"name": "offer"})))
            .await;
        let live = f.admin("GET", "/snippets", None).await;
        let all = f.admin("GET", "/snippets?archived=true", None).await;

        assert_eq!(clash.status, StatusCode::CONFLICT);
        assert_eq!(reuse.status, StatusCode::CREATED);
        assert_eq!(live.body.as_array().unwrap().len(), 1);
        assert_eq!(all.body.as_array().unwrap().len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_list_counts_the_templates_that_hold_each_snippet(pool: PgPool) {
        let f = fixture(&pool).await;
        let held = f.create("Held", heading("Hi")).await;
        f.create("Free", heading("Hi")).await;
        let document = json!({"schema": 2, "channel": "email", "mode": "blocks", "blocks": {
            "subject": "Hi", "preheader": "", "category": "marketing",
            "sections": [{"id": "sec", "columns": [[
                {"type": "snippet", "id": "s", "snippet_id": held}]]}]}});
        let template = f
            .admin(
                "POST",
                "/templates",
                Some(
                    json!({"name": "Mail", "channel": "email", "category": "marketing",
                    "document": document}),
                ),
            )
            .await;
        assert_eq!(template.status, StatusCode::CREATED, "{:?}", template.body);

        let list = f.admin("GET", "/snippets", None).await;

        let counts: Vec<(&str, i64)> = list
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["name"].as_str().unwrap(),
                    item["used_by_templates"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(counts, [("Free", 0), ("Held", 1)]);
    }
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn audit_rows_hold_ids_and_names_and_never_the_content(pool: PgPool) {
        let f = fixture(&pool).await;
        let marker = "unmistakable-marker-text";
        let id = f.create("Offer", heading(marker)).await;
        f.put_draft(&id, heading(marker), None).await;
        let published = f.publish(&id).await;
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        let archived = f.admin("DELETE", &format!("/snippets/{id}"), None).await;
        assert_eq!(archived.status, StatusCode::OK);

        let actions: Vec<String> = sqlx::query_scalar(
            "SELECT action FROM audit_log WHERE action LIKE 'snippet.%' ORDER BY id",
        )
        .fetch_all(&f.pool)
        .await
        .unwrap();
        let leaked = f
            .count(&format!(
                "SELECT count(*) FROM audit_log WHERE action LIKE 'snippet.%' \
                 AND summary::text LIKE '%{marker}%'"
            ))
            .await;

        assert_eq!(
            actions,
            [
                "snippet.created",
                "snippet.draft_saved",
                "snippet.published",
                "snippet.archived"
            ]
        );
        assert_eq!(leaked, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn preview_renders_the_blocks_with_the_asset_urls_and_warns_of_a_deleted_asset(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let (asset, hash) = f.asset("photo", b"preview-bytes").await;
        let content = json!({"blocks": [
            {"type": "heading", "id": "h", "level": 2, "text": "Preview marker"},
            {"type": "image", "id": "i", "alt": "x", "src": {"asset": asset.to_string()}}]});
        let id = f.create("Offer", content).await;

        let draft = f
            .admin("POST", &format!("/snippets/{id}/preview"), Some(json!({})))
            .await;
        f.publish(&id).await;
        f.put_draft(
            &id,
            json!({"blocks": [{"type": "image", "id": "i", "alt": "x",
                               "src": {"asset": Uuid::now_v7().to_string()}}]}),
            None,
        )
        .await;
        let missing = f
            .admin("POST", &format!("/snippets/{id}/preview"), Some(json!({})))
            .await;
        let version = f
            .admin(
                "POST",
                &format!("/snippets/{id}/preview"),
                Some(json!({"version": 1})),
            )
            .await;
        let no_version = f
            .admin(
                "POST",
                &format!("/snippets/{id}/preview"),
                Some(json!({"version": 9})),
            )
            .await;

        assert_eq!(draft.status, StatusCode::OK, "{:?}", draft.body);
        assert_eq!(draft.body["rendered"], true, "{:?}", draft.body);
        let html = draft.body["html"].as_str().unwrap();
        assert!(html.contains("Preview marker"));
        assert!(html.contains(&format!("/a/{hash}.png")), "{html}");
        assert!(
            draft.body["text"]
                .as_str()
                .unwrap()
                .contains("Preview marker")
        );
        assert_eq!(missing.status, StatusCode::OK);
        assert_eq!(missing.body["rendered"], true, "{:?}", missing.body);
        assert!(missing.body["html"].is_string());
        assert_eq!(missing.body["issues"][0]["code"], "broken_asset_reference");
        assert_eq!(missing.body["issues"][0]["severity"], "warning");
        assert_eq!(version.body["source"], "version");
        assert_eq!(version.body["rendered"], true);
        assert_eq!(no_version.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_deleted_asset_previews_with_a_warning_and_still_refuses_to_publish(pool: PgPool) {
        let f = fixture(&pool).await;
        let (asset, _) = f.asset("photo", b"soon-deleted").await;
        let id = f
            .create(
                "Photo",
                json!({"blocks": [{"type": "image", "id": "i", "alt": "x",
                                   "src": {"asset": asset.to_string()}}]}),
            )
            .await;
        let deleted = f.admin("DELETE", &format!("/assets/{asset}"), None).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{:?}", deleted.body);

        let preview = f
            .admin("POST", &format!("/snippets/{id}/preview"), Some(json!({})))
            .await;
        let published = f.publish(&id).await;

        assert_eq!(preview.body["rendered"], true, "{:?}", preview.body);
        assert_eq!(preview.body["issues"][0]["code"], "broken_asset_reference");
        assert_eq!(preview.body["issues"][0]["severity"], "warning");
        assert_eq!(published.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(published.body["code"], "snippet_invalid");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_a_render_pool_a_publish_and_a_preview_are_unavailable(pool: PgPool) {
        let f = fixture_with(&pool, None).await;
        let id = f.create("Offer", heading("Hi")).await;

        let publish = f.publish(&id).await;
        let preview = f
            .admin("POST", &format!("/snippets/{id}/preview"), Some(json!({})))
            .await;

        assert_eq!(publish.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(publish.body["code"], "render_unavailable");
        assert_eq!(preview.body["code"], "render_unavailable");
        assert_eq!(f.count("SELECT count(*) FROM snippet_versions").await, 0);
        let _ = f.other_ws;
    }
}
