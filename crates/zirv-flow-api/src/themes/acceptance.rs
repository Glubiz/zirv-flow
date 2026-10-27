//! The themes API end to end, driven through the router: the permission matrix, drafts and their
//! revision, publish and its immutability, validation, reference rows, the default, where-used,
//! the snippet footer guard, audit rows and preview through the render pool.

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
        publish: String,
        /// A key with all three scopes, for the setup and the checks around the matrix.
        all: String,
        other_admin: String,
        other_all: String,
    }

    /// The render host of the pool under test: a no-op unless the pool started it.
    #[test]
    fn render_host_child() {
        if std::env::var("ZIRV_FLOW_TEST_THEME_HOST").is_err() {
            return;
        }
        std::process::exit(i32::from(zirv_flow_render::host::run(None)));
    }

    async fn start_pool() -> RenderPool {
        let command = ChildCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "themes::acceptance::tests::render_host_child",
                "--nocapture",
            ])
            .args(["--test-threads", "1", "-q"])
            .env("ZIRV_FLOW_TEST_THEME_HOST", "serve")
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
                admin::create_api_key(&pool, &Context::system(), workspace, "themes", scopes)
                    .await
                    .unwrap()
                    .plaintext
            }
        };
        const ALL: &[Scope] = &[Scope::Read, Scope::Write, Scope::Publish, Scope::Admin];
        Fixture {
            pool: pool.clone(),
            router,
            ws,
            other_ws,
            read: key(ws, &[Scope::Read]).await,
            write: key(ws, &[Scope::Write]).await,
            admin: key(ws, &[Scope::Admin]).await,
            publish: key(ws, &[Scope::Publish]).await,
            all: key(ws, ALL).await,
            other_admin: key(other_ws, &[Scope::Admin]).await,
            other_all: key(other_ws, ALL).await,
        }
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        fixture_with(pool, Some(start_pool().await)).await
    }

    fn tokens(primary: &str) -> Value {
        json!({"primary": primary})
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

        async fn create(&self, name: &str, tokens: Value) -> String {
            let reply = self
                .admin(
                    "POST",
                    "/themes",
                    Some(json!({"name": name, "tokens": tokens})),
                )
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().to_owned()
        }

        async fn put_draft(&self, id: &str, tokens: Value, revision: Option<i32>) -> Reply {
            self.admin(
                "PUT",
                &format!("/themes/{id}/draft"),
                Some(json!({"tokens": tokens, "revision": revision})),
            )
            .await
        }

        async fn publish(&self, id: &str) -> Reply {
            self.admin("POST", &format!("/themes/{id}/publish"), None)
                .await
        }

        /// A theme that is created and published.
        async fn published(&self, name: &str, tokens: Value) -> String {
            let id = self.create(name, tokens).await;
            let reply = self.publish(&id).await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            id
        }

        async fn set_default(&self, id: &str) -> Reply {
            self.admin("PUT", "/themes/default", Some(json!({"theme_id": id})))
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

        /// A snippet with one published version, to use as a footer; returns its id.
        async fn footer_snippet(&self, name: &str, text: &str) -> String {
            let reply = self
                .admin(
                    "POST",
                    "/snippets",
                    Some(json!({"name": name, "content": {"blocks": [
                        {"type": "heading", "id": "h", "level": 3, "text": text}]}})),
                )
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            let id = reply.body["id"].as_str().unwrap().to_owned();
            let published = self
                .admin("POST", &format!("/snippets/{id}/publish"), None)
                .await;
            assert_eq!(
                published.status,
                StatusCode::CREATED,
                "{:?}",
                published.body
            );
            id
        }

        /// A block template whose draft says `theme`; absent leaves the field out.
        async fn template_with(&self, name: &str, theme: Option<Value>) -> String {
            let mut blocks = json!({"subject": "Hi", "category": "marketing", "sections": []});
            if let Some(theme) = theme {
                blocks["theme"] = theme;
            }
            let document = json!({"schema": 2, "channel": "email", "mode": "blocks",
                                  "blocks": blocks});
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

        /// A block template with a published current version, written to the store directly so the
        /// sender check of the template publish does not apply; returns its id.
        async fn live_template(
            &self,
            name: &str,
            category: &str,
            theme: Value,
            sections: Value,
        ) -> String {
            let document = json!({"schema": 2, "channel": "email", "mode": "blocks", "blocks": {
                "subject": "Hi", "sender": "0192b1a4-7c3e-7000-8000-00000000a001", "lang": "en",
                "category": category, "theme": theme, "sections": sections}});
            let reply = self
                .admin(
                    "POST",
                    "/templates",
                    Some(
                        json!({"name": name, "channel": "email", "category": category,
                        "document": document}),
                    ),
                )
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            let id = reply.body["id"].as_str().unwrap().to_owned();
            let template: Uuid = id.parse().unwrap();
            sqlx::query(
                "INSERT INTO template_versions (workspace_id, template_id, version, document, \
                    validation, trace_id) \
                 SELECT workspace_id, template_id, 1, document, '[]'::jsonb, gen_random_uuid() \
                 FROM template_drafts WHERE template_id = $1",
            )
            .bind(template)
            .execute(&self.pool)
            .await
            .unwrap();
            sqlx::query("UPDATE templates SET current_version = 1 WHERE id = $1")
                .bind(template)
                .execute(&self.pool)
                .await
                .unwrap();
            id
        }

        /// A published flow whose graph names `template`; returns its id.
        async fn flow_naming(&self, name: &str, template: &str) -> Uuid {
            let flow = Uuid::now_v7();
            let graph = json!({"v": 1, "nodes": [
                {"id": "send", "type": "action.send", "template": template},
                {"id": "end", "type": "exit"}], "edges": []});
            let mut connection = self.pool.acquire().await.unwrap();
            zirv_flow_store::create_flow(
                &mut connection,
                self.ws,
                &zirv_flow_store::NewFlow {
                    id: flow,
                    name: name.into(),
                    graph,
                    settings: json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}}),
                },
                None,
            )
            .await
            .unwrap();
            drop(connection);
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
            flow
        }
    }

    /// Snippet content with the unsubscribe link, which a footer needs.
    fn unsubscribe_content() -> Value {
        json!({"blocks": [{"type": "text", "id": "legal", "content":
            r#"<p><a href="{{ system.unsubscribe_url }}">Unsubscribe</a></p>"#}]})
    }

    fn section_of(block: Value) -> Value {
        json!([{"id": "s", "columns": [[block]]}])
    }

    fn issue_codes(reply: &Reply) -> Vec<String> {
        reply.body["template_issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|issue| issue["code"].as_str().unwrap().to_owned())
            .collect()
    }

    fn ids(list: &Value) -> Vec<String> {
        list.as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_str().unwrap().to_owned())
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn each_route_needs_its_permission_and_no_other(pool: PgPool) {
        let f = fixture(&pool).await;
        let theme = f.published("Brand", tokens("#112233")).await;
        let id = |tail: &str| format!("/themes/{theme}{tail}");
        let credentials: [(&str, Auth<'_>, u8); 6] = [
            ("anonymous", Auth::Anonymous, 0),
            ("read key", Auth::Key(&f.read), 1),
            ("write key", Auth::Key(&f.write), 2),
            ("admin key", Auth::Key(&f.admin), 3),
            ("publish key", Auth::Key(&f.publish), 4),
            ("other workspace's admin", Auth::Key(&f.other_admin), 9),
        ];
        // A key holds exactly one scope; the level a route needs is 1 read, 2 write, 3 admin, 4 publish.
        let requests: [(&str, &str, String, Option<Value>, u8); 12] = [
            ("list", "GET", "/themes".into(), None, 1),
            ("get", "GET", id(""), None, 1),
            ("versions", "GET", id("/versions"), None, 1),
            ("get version", "GET", id("/versions/1"), None, 1),
            ("where used", "GET", id("/where-used"), None, 1),
            (
                "create",
                "POST",
                "/themes".into(),
                Some(json!({"name": "Other"})),
                2,
            ),
            ("update", "PUT", id(""), Some(json!({"name": "Renamed"})), 2),
            (
                "save draft",
                "PUT",
                id("/draft"),
                Some(json!({"tokens": tokens("#445566")})),
                2,
            ),
            ("preview", "POST", id("/preview"), Some(json!({})), 2),
            ("publish", "POST", id("/publish"), None, 4),
            (
                "set default",
                "PUT",
                "/themes/default".into(),
                Some(json!({"theme_id": theme})),
                3,
            ),
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
        let id = f.create("Brand", tokens("#111111")).await;

        let first = f.publish(&id).await;
        f.put_draft(&id, tokens("#222222"), None).await;
        let second = f.publish(&id).await;
        let one = f
            .admin("GET", &format!("/themes/{id}/versions/1"), None)
            .await;
        let list = f
            .admin("GET", &format!("/themes/{id}/versions"), None)
            .await;
        let update = sqlx::query("UPDATE theme_versions SET tokens = '{}'::jsonb")
            .execute(&f.pool)
            .await;
        let delete = sqlx::query("DELETE FROM theme_versions")
            .execute(&f.pool)
            .await;

        assert_eq!(first.status, StatusCode::CREATED, "{:?}", first.body);
        assert_eq!(first.body["version"]["version"], 1);
        assert_eq!(second.body["version"]["version"], 2);
        assert_eq!(second.body["theme"]["current_version"], 2);
        assert_eq!(one.body["tokens"]["primary"], "#111111");
        let numbers: Vec<i64> = list
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["version"].as_i64().unwrap())
            .collect();
        assert_eq!(numbers, [2, 1]);
        assert!(update.is_err() && delete.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_stale_draft_revision_conflicts_and_a_bad_shape_is_refused(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f.create("Brand", tokens("#111111")).await;

        let saved = f.put_draft(&id, tokens("#222222"), Some(1)).await;
        let stale = f.put_draft(&id, tokens("#333333"), Some(1)).await;
        let unknown = f.put_draft(&id, json!({"colour": "#fff"}), None).await;
        let wrong = f.put_draft(&id, json!({"primary": 3}), None).await;
        let draft = f.admin("GET", &format!("/themes/{id}"), None).await;

        assert_eq!(saved.body["draft"]["revision"], 2, "{:?}", saved.body);
        assert_eq!(stale.status, StatusCode::CONFLICT);
        assert_eq!(stale.body["code"], "theme_revision_mismatch");
        assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(unknown.body["code"], "theme_malformed");
        assert_eq!(wrong.body["code"], "theme_malformed");
        assert_eq!(draft.body["draft"]["tokens"]["primary"], "#222222");
        assert_eq!(draft.body["draft"]["tokens"]["font"], "arial");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn contrast_only_warns_and_a_bad_shape_blocks_and_writes_nothing(pool: PgPool) {
        let f = fixture(&pool).await;
        let pale = f
            .create("Pale", json!({"text": "#cccccc", "background": "#ffffff"}))
            .await;
        let bad = f.create("Bad", json!({"link": "blue"})).await;

        let warned = f.publish(&pale).await;
        let refused = f.publish(&bad).await;

        assert_eq!(warned.status, StatusCode::CREATED, "{:?}", warned.body);
        assert_eq!(
            warned.body["version"]["warnings"][0]["code"],
            "low_contrast"
        );
        assert_eq!(warned.body["version"]["warnings"][0]["severity"], "warning");
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(refused.body["code"], "theme_invalid");
        assert!(issue_codes(&refused).contains(&"invalid_setting".to_owned()));
        let count = f
            .count(&format!(
                "SELECT count(*) FROM theme_versions WHERE theme_id = '{bad}'"
            ))
            .await;
        assert_eq!(count, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn publish_blocks_on_a_missing_logo_and_a_footer_that_is_not_usable(pool: PgPool) {
        let f = fixture(&pool).await;
        let (asset, _) = f.asset("logo", b"logo-bytes").await;
        let live = f.footer_snippet("Footer", "Footer text").await;
        let draft_only = f
            .admin(
                "POST",
                "/snippets",
                Some(json!({"name": "Unpublished", "content": {"blocks": [
                    {"type": "divider", "id": "d"}]}})),
            )
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let archived = f.footer_snippet("Archived", "Old").await;
        let gone = f
            .admin("DELETE", &format!("/snippets/{archived}"), None)
            .await;
        assert_eq!(gone.status, StatusCode::OK, "{:?}", gone.body);
        let id = f.create("Brand", tokens("#112233")).await;

        let mut results = Vec::new();
        for (label, patch) in [
            ("missing logo", json!({"logo": Uuid::now_v7().to_string()})),
            ("unpublished footer", json!({"footer_snippet": draft_only})),
            ("archived footer", json!({"footer_snippet": archived})),
            (
                "unknown footer",
                json!({"footer_snippet": Uuid::now_v7().to_string()}),
            ),
        ] {
            f.put_draft(&id, patch, None).await;
            let reply = f.publish(&id).await;
            results.push((label, reply.status, issue_codes(&reply)));
        }
        f.put_draft(
            &id,
            json!({"logo": asset.to_string(), "footer_snippet": live}),
            None,
        )
        .await;
        let accepted = f.publish(&id).await;

        let broken_logo = ("missing logo", StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!((results[0].0, results[0].1), broken_logo);
        assert_eq!(results[0].2, ["broken_asset_reference"]);
        for result in &results[1..] {
            assert_eq!(result.1, StatusCode::UNPROCESSABLE_ENTITY, "{}", result.0);
            assert_eq!(result.2, ["broken_snippet_reference"], "{}", result.0);
        }
        assert_eq!(accepted.status, StatusCode::CREATED, "{:?}", accepted.body);
        let references = f
            .count(&format!(
                "SELECT count(*) FROM asset_references WHERE owner_kind = 'theme' \
                 AND owner_id = '{id}' AND asset_id = '{asset}' AND version = 1"
            ))
            .await;
        assert_eq!(references, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_default_is_refused_for_an_unpublished_archived_or_foreign_theme(pool: PgPool) {
        let f = fixture(&pool).await;
        let unpublished = f.create("Draft", tokens("#111111")).await;
        let archived = f.published("Old", tokens("#222222")).await;
        f.admin("DELETE", &format!("/themes/{archived}"), None)
            .await;
        let foreign = send(
            &f.router,
            "POST",
            &format!("/api/v1/workspaces/{}/themes", f.other_ws.as_uuid()),
            Some(json!({"name": "Foreign"})),
            Auth::Key(&f.other_all),
        )
        .await
        .body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let unknown = Uuid::now_v7().to_string();

        for refused in [&unpublished, &archived, &foreign, &unknown] {
            let reply = f.set_default(refused).await;
            assert_eq!(
                reply.status,
                StatusCode::CONFLICT,
                "{refused} {:?}",
                reply.body
            );
            assert_eq!(reply.body["code"], "theme_not_publishable_default");
        }
        assert_eq!(
            f.count("SELECT count(*) FROM workspace_default_theme")
                .await,
            0
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_new_default_replaces_the_old_and_the_default_cannot_be_archived(pool: PgPool) {
        let f = fixture(&pool).await;
        let first = f.published("First", tokens("#111111")).await;
        let second = f.published("Second", tokens("#222222")).await;

        let set = f.set_default(&first).await;
        let archive_default = f.admin("DELETE", &format!("/themes/{first}"), None).await;
        f.set_default(&second).await;
        let list = f.admin("GET", "/themes", None).await;
        let archive_old = f.admin("DELETE", &format!("/themes/{first}"), None).await;

        assert_eq!(set.status, StatusCode::OK, "{:?}", set.body);
        assert_eq!(set.body["is_default"], true);
        assert_eq!(archive_default.status, StatusCode::CONFLICT);
        assert_eq!(archive_default.body["code"], "theme_is_default");
        let defaults: Vec<(String, bool)> = list
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                (
                    t["name"].as_str().unwrap().to_owned(),
                    t["is_default"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            defaults,
            [("First".to_owned(), false), ("Second".to_owned(), true)]
        );
        assert_eq!(archive_old.status, StatusCode::OK, "{:?}", archive_old.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_list_carries_the_tokens_and_the_template_counts(pool: PgPool) {
        let f = fixture(&pool).await;
        let brand = f.published("Brand", tokens("#111111")).await;
        f.create("Draft", tokens("#333333")).await;
        let other = f.published("Other", tokens("#222222")).await;
        f.set_default(&brand).await;
        f.template_with("Named", Some(json!({"id": brand}))).await;
        f.template_with("Follows", Some(json!("default"))).await;
        f.template_with("Elsewhere", Some(json!({"id": other})))
            .await;
        let gone = f.template_with("Gone", Some(json!({"id": other}))).await;
        let archived = f.admin("DELETE", &format!("/templates/{gone}"), None).await;
        assert_eq!(archived.status, StatusCode::OK, "{:?}", archived.body);

        let list = f.admin("GET", "/themes", None).await;

        let cards: Vec<(&str, &str, i64)> = list
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["name"].as_str().unwrap(),
                    item["tokens"]["primary"].as_str().unwrap(),
                    item["used_by_templates"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            cards,
            [
                ("Brand", "#111111", 2),
                ("Draft", "#333333", 0),
                ("Other", "#222222", 1)
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn where_used_separates_follow_default_from_named_and_lists_active_flows(pool: PgPool) {
        let f = fixture(&pool).await;
        let brand = f.published("Brand", tokens("#111111")).await;
        let other = f.published("Other", tokens("#222222")).await;
        let follows = f.template_with("Follows", Some(json!("default"))).await;
        let absent = f.template_with("Absent", None).await;
        let named = f.template_with("Named", Some(json!({"id": brand}))).await;
        let elsewhere = f
            .template_with("Elsewhere", Some(json!({"id": other})))
            .await;
        let opted_out = f.template_with("Opted out", Some(json!("none"))).await;
        let flow = f.flow_naming("Sends", &follows).await;
        f.flow_naming("Sends named elsewhere", &elsewhere).await;
        let _ = opted_out;

        let before = f
            .admin("GET", &format!("/themes/{brand}/where-used"), None)
            .await;
        f.set_default(&brand).await;
        let after = f
            .admin("GET", &format!("/themes/{brand}/where-used"), None)
            .await;
        let other_after = f
            .admin("GET", &format!("/themes/{other}/where-used"), None)
            .await;
        f.set_default(&other).await;
        let moved = f
            .admin("GET", &format!("/themes/{brand}/where-used"), None)
            .await;

        assert_eq!(ids(&before.body["named_templates"]), [named.as_str()]);
        assert!(
            before.body["default_templates"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(ids(&after.body["named_templates"]), [named.as_str()]);
        assert_eq!(
            ids(&after.body["default_templates"]),
            [absent, follows.clone()]
        );
        let flows: Vec<&str> = after.body["active_flows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["flow_id"].as_str().unwrap())
            .collect();
        assert_eq!(flows, [flow.to_string()]);
        assert_eq!(ids(&other_after.body["named_templates"]), [elsewhere]);
        assert!(
            other_after.body["default_templates"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            moved.body["default_templates"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_theme_footer_is_a_snippet_use_and_blocks_its_archive(pool: PgPool) {
        let f = fixture(&pool).await;
        let footer = f.footer_snippet("Footer", "Footer text").await;
        let theme = f.create("Brand", json!({"footer_snippet": footer})).await;
        let follows = f.template_with("Follows", Some(json!("default"))).await;
        let flow = f.flow_naming("Sends", &follows).await;

        let draft_only = f
            .admin("GET", &format!("/snippets/{footer}/where-used"), None)
            .await;
        let published = f.publish(&theme).await;
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        let not_default = f
            .admin("GET", &format!("/snippets/{footer}/where-used"), None)
            .await;
        f.set_default(&theme).await;
        let as_default = f
            .admin("GET", &format!("/snippets/{footer}/where-used"), None)
            .await;
        let refused = f
            .admin("DELETE", &format!("/snippets/{footer}"), None)
            .await;
        f.admin("PUT", "/themes/default", Some(json!({"theme_id": theme})))
            .await;

        assert_eq!(ids(&draft_only.body["themes"]), [theme.as_str()]);
        assert_eq!(ids(&not_default.body["themes"]), [theme.as_str()]);
        assert!(not_default.body["templates"].as_array().unwrap().is_empty());
        assert_eq!(ids(&as_default.body["templates"]), [follows]);
        assert_eq!(
            as_default.body["active_flows"][0]["flow_id"],
            flow.to_string()
        );
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert_eq!(refused.body["code"], "snippet_in_use");
        assert_eq!(ids(&refused.body["snippet_themes"]), [theme]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn audit_rows_hold_ids_and_names_and_never_token_values(pool: PgPool) {
        let f = fixture(&pool).await;
        let marker = "#a1b2c3";
        let id = f.create("Brand", tokens(marker)).await;
        f.put_draft(&id, tokens(marker), None).await;
        f.publish(&id).await;
        f.set_default(&id).await;
        let other = f.published("Other", tokens(marker)).await;
        f.set_default(&other).await;
        let archived = f.admin("DELETE", &format!("/themes/{id}"), None).await;
        assert_eq!(archived.status, StatusCode::OK, "{:?}", archived.body);

        let actions: Vec<String> = sqlx::query_scalar(
            "SELECT action FROM audit_log WHERE action LIKE 'theme.%' ORDER BY id",
        )
        .fetch_all(&f.pool)
        .await
        .unwrap();
        let leaked = f
            .count(&format!(
                "SELECT count(*) FROM audit_log WHERE action LIKE 'theme.%' \
                 AND summary::text LIKE '%{marker}%'"
            ))
            .await;

        assert_eq!(
            actions,
            [
                "theme.created",
                "theme.draft_saved",
                "theme.published",
                "theme.default_set",
                "theme.created",
                "theme.published",
                "theme.default_set",
                "theme.archived",
            ]
        );
        assert_eq!(leaked, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn preview_renders_under_the_draft_tokens_with_the_logo_and_the_footer(pool: PgPool) {
        let f = fixture(&pool).await;
        let (asset, hash) = f.asset("logo", b"logo-for-preview").await;
        let footer = f.footer_snippet("Footer", "Footer marker text").await;
        let id = f
            .create(
                "Brand",
                json!({"primary": "#aa1100", "logo": asset.to_string(), "footer_snippet": footer}),
            )
            .await;
        f.publish(&id).await;
        f.put_draft(&id, json!({"primary": "#00bb22"}), None).await;

        let draft = f
            .admin("POST", &format!("/themes/{id}/preview"), Some(json!({})))
            .await;
        let version = f
            .admin(
                "POST",
                &format!("/themes/{id}/preview"),
                Some(json!({"version": 1})),
            )
            .await;
        let no_version = f
            .admin(
                "POST",
                &format!("/themes/{id}/preview"),
                Some(json!({"version": 9})),
            )
            .await;

        assert_eq!(draft.status, StatusCode::OK, "{:?}", draft.body);
        assert_eq!(draft.body["rendered"], true, "{:?}", draft.body);
        let html = draft.body["html"].as_str().unwrap().to_lowercase();
        assert!(html.contains("#00bb22"), "{html}");
        assert!(!html.contains("#aa1100"));
        assert!(draft.body["text"].as_str().unwrap().contains("A heading"));
        let published = version.body["html"].as_str().unwrap();
        assert!(published.to_lowercase().contains("#aa1100"));
        assert!(published.contains(&format!("/a/{hash}.png")), "{published}");
        assert!(published.contains("Footer marker text"));
        assert_eq!(version.body["source"], "version");
        assert_eq!(no_version.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn preview_with_a_missing_logo_or_footer_warns_and_still_renders(pool: PgPool) {
        let f = fixture(&pool).await;
        let id = f
            .create(
                "Brand",
                json!({"logo": Uuid::now_v7().to_string(),
                       "footer_snippet": Uuid::now_v7().to_string()}),
            )
            .await;

        let preview = f
            .admin("POST", &format!("/themes/{id}/preview"), Some(json!({})))
            .await;
        let invalid = f.put_draft(&id, json!({"button": "nope"}), None).await;
        let blocked = f
            .admin("POST", &format!("/themes/{id}/preview"), Some(json!({})))
            .await;

        assert_eq!(preview.status, StatusCode::OK, "{:?}", preview.body);
        assert_eq!(preview.body["rendered"], true, "{:?}", preview.body);
        assert!(preview.body["html"].is_string());
        let issues: Vec<(&str, &str)> = preview.body["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| (i["code"].as_str().unwrap(), i["severity"].as_str().unwrap()))
            .collect();
        assert_eq!(
            issues,
            [
                ("broken_asset_reference", "warning"),
                ("broken_snippet_reference", "warning")
            ]
        );
        assert_eq!(invalid.status, StatusCode::OK);
        assert_eq!(blocked.body["rendered"], false);
        assert!(blocked.body["html"].is_null());
        assert_eq!(blocked.body["issues"][0]["severity"], "blocking");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_live_name_is_taken_but_an_archived_one_is_free(pool: PgPool) {
        let f = fixture(&pool).await;
        let first = f.create("Brand", tokens("#111111")).await;

        let clash = f
            .admin("POST", "/themes", Some(json!({"name": "BRAND"})))
            .await;
        let archived = f.admin("DELETE", &format!("/themes/{first}"), None).await;
        let reuse = f
            .admin("POST", "/themes", Some(json!({"name": "brand"})))
            .await;
        let rename = f
            .admin(
                "PUT",
                &format!("/themes/{first}"),
                Some(json!({"name": "Else"})),
            )
            .await;

        assert_eq!(clash.status, StatusCode::CONFLICT);
        assert_eq!(archived.status, StatusCode::OK);
        assert_eq!(reuse.status, StatusCode::CREATED, "{:?}", reuse.body);
        assert_eq!(rename.body["code"], "theme_archived");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_a_render_pool_a_publish_and_a_preview_are_unavailable(pool: PgPool) {
        let f = fixture_with(&pool, None).await;
        let id = f.create("Brand", tokens("#111111")).await;

        let publish = f.publish(&id).await;
        let preview = f
            .admin("POST", &format!("/themes/{id}/preview"), Some(json!({})))
            .await;

        assert_eq!(publish.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(publish.body["code"], "render_unavailable");
        assert_eq!(preview.body["code"], "render_unavailable");
        assert_eq!(f.count("SELECT count(*) FROM theme_versions").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_uppercase_footer_id_is_stored_lowercase_and_the_archive_guard_sees_it(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let footer = f.footer_snippet("Footer", "Footer text").await;
        let theme = f
            .create("Brand", json!({"footer_snippet": footer.to_uppercase()}))
            .await;

        let stored = f.admin("GET", &format!("/themes/{theme}"), None).await;
        let published = f.publish(&theme).await;
        let refused = f
            .admin("DELETE", &format!("/snippets/{footer}"), None)
            .await;

        assert_eq!(stored.body["draft"]["tokens"]["footer_snippet"], footer);
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
        assert_eq!(refused.body["code"], "snippet_in_use", "{:?}", refused.body);
        assert_eq!(ids(&refused.body["snippet_themes"]), [theme.as_str()]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_theme_version_that_drops_its_footer_is_refused_while_a_live_marketing_template_follows_it(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let footer = f
            .admin(
                "POST",
                "/snippets",
                Some(json!({"name": "Footer", "content": unsubscribe_content()})),
            )
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        f.admin("POST", &format!("/snippets/{footer}/publish"), None)
            .await;
        let theme = f
            .published("Brand", json!({"footer_snippet": footer}))
            .await;
        f.set_default(&theme).await;
        let heading =
            || section_of(json!({"type": "heading", "id": "h", "level": 1, "text": "Hi"}));
        let marketing = f
            .live_template("News", "marketing", json!("default"), heading())
            .await;
        f.live_template("Receipt", "transactional", json!("default"), heading())
            .await;
        f.put_draft(&theme, tokens("#112233"), None).await;

        let refused = f.publish(&theme).await;
        let versions_after_refusal = f.count("SELECT count(*) FROM theme_versions").await;
        let archived = f
            .admin("DELETE", &format!("/templates/{marketing}"), None)
            .await;
        let accepted = f.publish(&theme).await;

        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(refused.body["code"], "theme_breaks_templates");
        assert_eq!(refused.body["total"], 1);
        assert_eq!(ids(&refused.body["templates"]), [marketing.as_str()]);
        assert_eq!(
            refused.body["templates"][0]["codes"],
            json!(["missing_unsubscribe"])
        );
        assert_eq!(versions_after_refusal, 1);
        assert_eq!(archived.status, StatusCode::OK, "{:?}", archived.body);
        assert_eq!(accepted.status, StatusCode::CREATED, "{:?}", accepted.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_named_theme_version_that_breaks_the_template_naming_it_is_refused(pool: PgPool) {
        let f = fixture(&pool).await;
        let footer = f
            .admin(
                "POST",
                "/snippets",
                Some(json!({"name": "Footer", "content": unsubscribe_content()})),
            )
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        f.admin("POST", &format!("/snippets/{footer}/publish"), None)
            .await;
        let theme = f
            .published("Brand", json!({"footer_snippet": footer}))
            .await;
        let named = f
            .live_template(
                "Named",
                "marketing",
                json!({"id": theme}),
                section_of(json!({"type": "heading", "id": "h", "level": 1, "text": "Hi"})),
            )
            .await;
        f.put_draft(&theme, tokens("#112233"), None).await;

        let refused = f.publish(&theme).await;

        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(refused.body["code"], "theme_breaks_templates");
        assert_eq!(ids(&refused.body["templates"]), [named]);
    }
}
