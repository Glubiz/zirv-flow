//! The audit read API end to end, driven through the router: the access matrix, filters and
//! paging, the action catalogue, both export formats, the export's own audit row, and a
//! 100 000 row export that streams in bounded frames.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;
    use zirv_flow_core::UserId;

    use crate::admin::{self, Context};
    use crate::{AppState, router};

    const PASSWORD: &str = "correct horse battery";

    struct Login {
        cookie: String,
        csrf: String,
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        text: String,
    }

    impl Reply {
        fn json(&self) -> Value {
            serde_json::from_str(&self.text).unwrap_or(Value::Null)
        }
    }

    enum Auth<'a> {
        Anonymous,
        Session(&'a Login),
        Key(&'a str),
    }

    async fn app(pool: &PgPool) -> Router {
        zirv_flow_store::migrate(pool).await.unwrap();
        router(AppState::new(pool.clone(), Arc::new(String::new)))
    }

    fn request(method: &str, uri: &str, body: Option<Value>, auth: &Auth<'_>) -> Request<Body> {
        let mut request = Request::builder().method(method).uri(uri);
        match auth {
            Auth::Anonymous => {}
            Auth::Session(login) => {
                request = request
                    .header(header::COOKIE, &login.cookie)
                    .header("x-csrf-token", &login.csrf);
            }
            Auth::Key(key) => {
                request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
            }
        }
        match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => request.body(Body::empty()).unwrap(),
        }
    }

    async fn call(router: &Router, uri: &str, auth: Auth<'_>) -> Reply {
        let response = router
            .clone()
            .oneshot(request("GET", uri, None, &auth))
            .await
            .unwrap();
        let (status, headers) = (response.status(), response.headers().clone());
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            text: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    async fn status(router: &Router, uri: &str, auth: Auth<'_>) -> StatusCode {
        call(router, uri, auth).await.status
    }

    async fn login(router: &Router, email: &str) -> Login {
        let response = router
            .clone()
            .oneshot(request(
                "POST",
                "/api/v1/session",
                Some(json!({"email": email, "password": PASSWORD})),
                &Auth::Anonymous,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        Login {
            cookie,
            csrf: body["csrf_token"].as_str().unwrap().to_owned(),
        }
    }

    struct Fixture {
        pool: PgPool,
        router: Router,
        owner: Login,
        ws: String,
        ws_id: Uuid,
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        let router = app(pool).await;
        admin::create_owner(pool, &Context::system(), "owner@example.com", PASSWORD)
            .await
            .unwrap();
        let owner = login(&router, "owner@example.com").await;
        let created = router
            .clone()
            .oneshot(request(
                "POST",
                "/api/v1/workspaces",
                Some(json!({"name": "Alpha"})),
                &Auth::Session(&owner),
            ))
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let bytes = created.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let ws = body["id"].as_str().unwrap().to_owned();
        Fixture {
            pool: pool.clone(),
            router,
            owner,
            ws_id: ws.parse().unwrap(),
            ws,
        }
    }

    impl Fixture {
        fn uri(&self, tail: &str) -> String {
            format!("/api/v1/workspaces/{}/audit{tail}", self.ws)
        }

        /// A signed-in user with `role` in the workspace, or with none.
        async fn member(&self, email: &str, role: Option<&str>) -> Login {
            let user: UserId = admin::create_user(&self.pool, &Context::system(), email, PASSWORD)
                .await
                .unwrap()
                .id;
            if let Some(role) = role {
                let response = self
                    .router
                    .clone()
                    .oneshot(request(
                        "PUT",
                        &format!("/api/v1/workspaces/{}/members/{user}", self.ws),
                        Some(json!({"role": role})),
                        &Auth::Session(&self.owner),
                    ))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
            }
            login(&self.router, email).await
        }

        async fn key(&self, scopes: &[&str]) -> String {
            let scopes: Vec<zirv_flow_core::Scope> =
                scopes.iter().map(|scope| scope.parse().unwrap()).collect();
            admin::create_api_key(
                &self.pool,
                &Context::system(),
                zirv_flow_core::WorkspaceId::from_uuid(self.ws_id),
                "audit",
                &scopes,
            )
            .await
            .unwrap()
            .plaintext
        }

        async fn plant(&self, count: i64, action: &str, request_id: Option<&str>) {
            sqlx::query(
                "INSERT INTO audit_log (workspace_id, actor_kind, action, request_id) \
                 SELECT $1, 'system', $2, $3 FROM generate_series(1, $4)",
            )
            .bind(self.ws_id)
            .bind(action)
            .bind(request_id)
            .bind(count)
            .execute(&self.pool)
            .await
            .unwrap();
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_workspace_audit_routes_are_for_admins_and_the_owner(pool: PgPool) {
        let fx = fixture(&pool).await;
        let admin_member = fx.member("admin@example.com", Some("admin")).await;
        let editor = fx.member("editor@example.com", Some("editor")).await;
        let viewer = fx.member("viewer@example.com", Some("viewer")).await;
        let stranger = fx.member("stranger@example.com", None).await;
        let read_key = fx.key(&["read"]).await;
        let admin_key = fx.key(&["admin"]).await;
        for tail in ["", "/actions", "/export"] {
            let uri = fx.uri(tail);
            let ok = StatusCode::OK;
            assert_eq!(status(&fx.router, &uri, Auth::Session(&fx.owner)).await, ok);
            assert_eq!(
                status(&fx.router, &uri, Auth::Session(&admin_member)).await,
                ok
            );
            assert_eq!(status(&fx.router, &uri, Auth::Key(&admin_key)).await, ok);
            for denied in [&editor, &viewer] {
                let forbidden = StatusCode::FORBIDDEN;
                assert_eq!(
                    status(&fx.router, &uri, Auth::Session(denied)).await,
                    forbidden
                );
            }
            assert_eq!(
                status(&fx.router, &uri, Auth::Key(&read_key)).await,
                StatusCode::FORBIDDEN,
                "{uri}"
            );
            assert_eq!(
                status(&fx.router, &uri, Auth::Session(&stranger)).await,
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                status(&fx.router, &uri, Auth::Anonymous).await,
                StatusCode::UNAUTHORIZED
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_instance_audit_routes_are_for_the_owner_only(pool: PgPool) {
        let fx = fixture(&pool).await;
        let member = fx.member("admin@example.com", Some("admin")).await;
        let key = fx.key(&["read", "write", "admin"]).await;

        for uri in ["/api/v1/audit", "/api/v1/audit/export"] {
            let status = |auth| {
                let router = fx.router.clone();
                async move { call(&router, uri, auth).await.status }
            };
            assert_eq!(status(Auth::Session(&fx.owner)).await, StatusCode::OK);
            assert_eq!(status(Auth::Session(&member)).await, StatusCode::FORBIDDEN);
            assert_eq!(status(Auth::Key(&key)).await, StatusCode::FORBIDDEN);
            assert_eq!(status(Auth::Anonymous).await, StatusCode::UNAUTHORIZED);
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_instance_page_holds_instance_rows_and_the_workspace_page_does_not(pool: PgPool) {
        let fx = fixture(&pool).await;

        let instance = call(&fx.router, "/api/v1/audit", Auth::Session(&fx.owner)).await;
        let workspace = call(&fx.router, &fx.uri(""), Auth::Session(&fx.owner)).await;

        let instance = instance.json();
        let actions: Vec<&str> = instance["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["action"].as_str().unwrap())
            .collect();
        assert!(actions.contains(&"user.created"));
        assert!(
            instance["entries"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["workspace_id"].is_null())
        );
        let workspace = workspace.json();
        assert!(
            workspace["entries"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["workspace_id"] == json!(fx.ws))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_list_filters_pages_by_cursor_and_links_the_trace(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.plant(5, "list.created", Some("req-x")).await;

        let first = call(
            &fx.router,
            &fx.uri("?action=list.&limit=2"),
            Auth::Session(&fx.owner),
        )
        .await
        .json();
        let cursor = first["next_cursor"].as_str().unwrap().to_owned();
        let second = call(
            &fx.router,
            &fx.uri(&format!("?action=list.&limit=10&cursor={cursor}")),
            Auth::Session(&fx.owner),
        )
        .await
        .json();
        let by_request = call(
            &fx.router,
            &fx.uri("?request_id=req-x"),
            Auth::Session(&fx.owner),
        )
        .await
        .json();
        let workspace_rows = call(
            &fx.router,
            &fx.uri("?action=workspace.created"),
            Auth::Session(&fx.owner),
        )
        .await
        .json();
        let bad = call(
            &fx.router,
            &fx.uri("?action=nope"),
            Auth::Session(&fx.owner),
        )
        .await;

        assert_eq!(first["entries"].as_array().unwrap().len(), 2);
        assert_eq!(second["entries"].as_array().unwrap().len(), 3);
        assert!(second["next_cursor"].is_null());
        assert_eq!(by_request["entries"].as_array().unwrap().len(), 5);
        let created = &workspace_rows["entries"][0];
        assert_eq!(created["trace_id"].as_str().unwrap().len(), 32, "{created}");
        assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_action_catalogue_is_served_by_area(pool: PgPool) {
        let fx = fixture(&pool).await;

        let body = call(&fx.router, &fx.uri("/actions"), Auth::Session(&fx.owner))
            .await
            .json();

        let areas = body["areas"].as_array().unwrap();
        assert!(areas.iter().any(|a| {
            a["area"] == "flow"
                && a["actions"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("flow.created"))
        }));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn both_export_formats_stream_and_are_audited(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.plant(3, "list.created", Some("=cmd")).await;

        let ndjson = call(
            &fx.router,
            &fx.uri("/export?action=list.&format=ndjson"),
            Auth::Session(&fx.owner),
        )
        .await;
        let csv = call(
            &fx.router,
            &fx.uri("/export?action=list.&format=csv"),
            Auth::Session(&fx.owner),
        )
        .await;
        let instance = call(
            &fx.router,
            "/api/v1/audit/export?format=csv",
            Auth::Session(&fx.owner),
        )
        .await;
        let bad = call(
            &fx.router,
            &fx.uri("/export?format=xml"),
            Auth::Session(&fx.owner),
        )
        .await;

        assert_eq!(ndjson.headers[header::CONTENT_TYPE], "application/x-ndjson");
        let lines: Vec<Value> = ndjson
            .text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(csv.headers[header::CONTENT_TYPE], "text/csv; charset=utf-8");
        let rows: Vec<&str> = csv.text.lines().collect();
        assert_eq!(rows.len(), 4, "a header and three rows");
        assert!(rows[0].starts_with("id,occurred_at,workspace_id,"));
        assert!(rows[1].contains(",'=cmd,"), "{}", rows[1]);
        assert!(instance.text.lines().count() > 1);
        assert_eq!(bad.status, StatusCode::BAD_REQUEST);
        let audited: Vec<(String, Value, Option<Uuid>)> = sqlx::query_as(
            "SELECT action, summary, workspace_id FROM audit_log \
             WHERE action = 'audit.exported' ORDER BY id",
        )
        .fetch_all(&fx.pool)
        .await
        .unwrap();
        assert_eq!(audited.len(), 3);
        assert_eq!(audited[0].1["format"], "ndjson");
        assert_eq!(audited[0].1["rows"], 3);
        assert_eq!(audited[0].1["filters"], json!({"action": "list."}));
        assert_eq!(audited[0].2, Some(fx.ws_id));
        assert_eq!(audited[2].2, None, "the instance export is an instance row");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_hundred_thousand_row_export_streams_in_bounded_frames(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.plant(100_000, "list.created", None).await;

        let response = fx
            .router
            .clone()
            .oneshot(request(
                "GET",
                &fx.uri("/export?action=list.&format=ndjson"),
                None,
                &Auth::Session(&fx.owner),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        let (mut frames, mut largest, mut lines) = (0usize, 0usize, 0usize);
        while let Some(frame) = body.frame().await {
            let data = frame.unwrap().into_data().unwrap();
            frames += 1;
            largest = largest.max(data.len());
            lines += data.iter().filter(|byte| **byte == b'\n').count();
        }

        // The body is produced one batch of at most 500 rows at a time, so the largest piece
        // the server ever holds is a batch, however long the log is: 200 frames of about
        // 200 KiB for 100 000 rows, never one document.
        assert_eq!(lines, 100_000);
        assert_eq!(frames, 200);
        assert!(
            largest < 512 * 1024,
            "the largest frame was {largest} bytes"
        );
    }
}
