//! The retention API end to end, driven through the router: the eight windows, the two a
//! workspace admin may shorten and the refusals, the role matrix, the audit row and the last run.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use time::macros::datetime;
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

    enum Auth<'a> {
        Anonymous,
        Session(&'a Login),
        Key(&'a str),
    }

    struct Reply {
        status: StatusCode,
        body: Value,
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

    async fn send(
        router: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
        auth: Auth<'_>,
    ) -> Reply {
        let response = router
            .clone()
            .oneshot(request(method, uri, body, &auth))
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
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
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        zirv_flow_store::migrate(pool).await.unwrap();
        let router = router(AppState::new(pool.clone(), Arc::new(String::new)));
        admin::create_owner(pool, &Context::system(), "owner@example.com", PASSWORD)
            .await
            .unwrap();
        let owner = login(&router, "owner@example.com").await;
        let created = send(
            &router,
            "POST",
            "/api/v1/workspaces",
            Some(json!({"name": "Alpha"})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED);
        let ws = created.body["id"].as_str().unwrap().to_owned();
        Fixture {
            pool: pool.clone(),
            router,
            owner,
            ws,
        }
    }

    impl Fixture {
        fn uri(&self) -> String {
            format!("/api/v1/workspaces/{}/retention", self.ws)
        }

        async fn get(&self) -> Reply {
            send(
                &self.router,
                "GET",
                &self.uri(),
                None,
                Auth::Session(&self.owner),
            )
            .await
        }

        async fn put(&self, body: Value) -> Reply {
            send(
                &self.router,
                "PUT",
                &self.uri(),
                Some(body),
                Auth::Session(&self.owner),
            )
            .await
        }

        async fn member(&self, email: &str, role: Option<&str>) -> Login {
            let user: UserId = admin::create_user(&self.pool, &Context::system(), email, PASSWORD)
                .await
                .unwrap()
                .id;
            if let Some(role) = role {
                let response = send(
                    &self.router,
                    "PUT",
                    &format!("/api/v1/workspaces/{}/members/{user}", self.ws),
                    Some(json!({"role": role})),
                    Auth::Session(&self.owner),
                )
                .await;
                assert_eq!(response.status, StatusCode::OK);
            }
            login(&self.router, email).await
        }
    }

    fn row<'a>(body: &'a Value, key: &str) -> &'a Value {
        body["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|window| window["key"] == key)
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn get_lists_all_eight_windows_with_sources_bounds_and_editability(pool: PgPool) {
        let fx = fixture(&pool).await;

        let reply = fx.get().await;

        assert_eq!(reply.status, StatusCode::OK);
        let keys: Vec<&str> = reply.body["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|window| window["key"].as_str().unwrap())
            .collect();
        assert_eq!(
            keys,
            [
                "events",
                "messages",
                "decision_log",
                "audit",
                "ingest_rejects",
                "import_files",
                "job_history",
                "assistant_transcripts"
            ]
        );
        let expected = [
            ("events", json!(760), json!(30), json!(1095), false),
            ("messages", json!(395), json!(30), json!(395), true),
            ("decision_log", json!(395), json!(30), json!(395), true),
            ("audit", json!(730), json!(90), json!(3650), false),
            ("ingest_rejects", json!(30), Value::Null, Value::Null, false),
            ("import_files", json!(7), json!(1), json!(90), false),
            ("job_history", json!(14), json!(1), json!(90), false),
            (
                "assistant_transcripts",
                Value::Null,
                json!(1),
                json!(30),
                false,
            ),
        ];
        for (key, value, min, max, editable) in expected {
            let window = row(&reply.body, key);
            assert_eq!(window["value"], value, "{key}");
            assert_eq!((&window["min"], &window["max"]), (&min, &max), "{key}");
            assert_eq!(window["editable"], editable, "{key}");
            assert_eq!(window["unit"], "days", "{key}");
            assert!(window["last_run"].is_null(), "{key}");
        }
        assert!(
            reply.body["windows"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|window| window["key"] != "assistant_transcripts")
                .all(|window| window["source"] == "deployment")
        );
        assert_eq!(
            row(&reply.body, "assistant_transcripts")["source"],
            "workspace"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn put_shortens_messages_and_the_decision_log_and_null_returns_to_the_deployment(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;

        let shortened = fx
            .put(json!({"messages_days": 60, "decision_days": 90}))
            .await;

        assert_eq!(shortened.status, StatusCode::OK);
        let messages = row(&shortened.body, "messages");
        assert_eq!(
            (&messages["value"], &messages["source"]),
            (&json!(60), &json!("workspace"))
        );
        let decisions = row(&shortened.body, "decision_log");
        assert_eq!(
            (&decisions["value"], &decisions["source"]),
            (&json!(90), &json!("workspace"))
        );
        assert_eq!(row(&fx.get().await.body, "messages")["value"], 60);

        let partly = fx
            .put(json!({"messages_days": null, "decision_days": 45}))
            .await;

        let messages = row(&partly.body, "messages");
        assert_eq!(
            (&messages["value"], &messages["source"]),
            (&json!(395), &json!("deployment"))
        );
        assert_eq!(row(&partly.body, "decision_log")["value"], 45);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_field_left_out_keeps_its_window_and_an_explicit_null_clears_it(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.put(json!({"messages_days": 60, "decision_days": 90}))
            .await;

        let only_messages = fx.put(json!({"messages_days": 45})).await;

        assert_eq!(row(&only_messages.body, "messages")["value"], 45);
        let decisions = row(&only_messages.body, "decision_log");
        assert_eq!(
            (&decisions["value"], &decisions["source"]),
            (&json!(90), &json!("workspace"))
        );
        let cleared = fx.put(json!({"decision_days": null})).await;
        let decisions = row(&cleared.body, "decision_log");
        assert_eq!(
            (&decisions["value"], &decisions["source"]),
            (&json!(395), &json!("deployment"))
        );
        assert_eq!(row(&cleared.body, "messages")["value"], 45);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn put_refuses_longer_than_the_deployment_below_the_minimum_and_unknown_fields(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;

        for (body, expected) in [
            (json!({"messages_days": 396}), "between 30 and 395"),
            (json!({"messages_days": 29}), "between 30 and 395"),
            (json!({"decision_days": 0}), "between 30 and 395"),
            (json!({"decision_days": 5000}), "bounds are 30 to 760"),
        ] {
            let refused = fx.put(body.clone()).await;

            assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_eq!(refused.body["code"], "retention_out_of_bounds", "{body}");
            assert!(
                refused.body["detail"].as_str().unwrap().contains(expected),
                "{body}: {}",
                refused.body
            );
        }
        for field in [
            "audit_days",
            "job_history_days",
            "import_file_days",
            "events_days",
        ] {
            let refused = fx.put(json!({"messages_days": 60, field: 30})).await;

            assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{field}");
        }
        assert_eq!(
            row(&fx.get().await.body, "messages")["source"],
            "deployment"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_admins_and_the_owner_may_read_or_change_the_windows(pool: PgPool) {
        let fx = fixture(&pool).await;
        let admin_member = fx.member("admin@example.com", Some("admin")).await;
        let publisher = fx.member("publisher@example.com", Some("publisher")).await;
        let editor = fx.member("editor@example.com", Some("editor")).await;
        let viewer = fx.member("viewer@example.com", Some("viewer")).await;
        let stranger = fx.member("stranger@example.com", None).await;
        let uri = fx.uri();
        let body = || Some(json!({"messages_days": 60}));

        for method in ["GET", "PUT"] {
            let body = if method == "PUT" { body() } else { None };
            for allowed in [&fx.owner, &admin_member] {
                let reply = send(
                    &fx.router,
                    method,
                    &uri,
                    body.clone(),
                    Auth::Session(allowed),
                )
                .await;
                assert_eq!(reply.status, StatusCode::OK, "{method}");
            }
            for denied in [&publisher, &editor, &viewer] {
                let reply = send(
                    &fx.router,
                    method,
                    &uri,
                    body.clone(),
                    Auth::Session(denied),
                )
                .await;
                assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method}");
            }
            let reply = send(
                &fx.router,
                method,
                &uri,
                body.clone(),
                Auth::Session(&stranger),
            )
            .await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method}");
            let reply = send(&fx.router, method, &uri, body.clone(), Auth::Anonymous).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{method}");
        }
        let read = admin::create_api_key(
            &fx.pool,
            &Context::system(),
            zirv_flow_core::WorkspaceId::from_uuid(fx.ws.parse::<Uuid>().unwrap()),
            "retention",
            &["read".parse().unwrap(), "write".parse().unwrap()],
        )
        .await
        .unwrap()
        .plaintext;
        let reply = send(&fx.router, "GET", &uri, None, Auth::Key(&read)).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_audit_row_holds_window_names_and_numbers_only(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.put(json!({"messages_days": 60, "decision_days": null}))
            .await;

        let (action, summary): (String, Value) = sqlx::query_as(
            "SELECT action, summary FROM audit_log WHERE action = 'retention.updated'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(action, "retention.updated");
        assert_eq!(summary, json!({"messages_days": 60, "decision_days": null}));
        fx.put(json!({"decision_days": 45})).await;
        let latest: Value = sqlx::query_scalar(
            "SELECT summary FROM audit_log WHERE action = 'retention.updated' \
             ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(latest, json!({"decision_days": 45}));
        let refused = fx.put(json!({"messages_days": 9999})).await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'retention.updated'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rows, 2, "a refused change writes no audit row");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn get_shows_the_last_run_of_a_window(pool: PgPool) {
        let fx = fixture(&pool).await;
        zirv_flow_store::record_retention_run(&pool, "audit", datetime!(2026-06-01 4:19 UTC), 12)
            .await
            .unwrap();
        zirv_flow_store::record_retention_run(&pool, "messages", datetime!(2026-06-01 3:17 UTC), 0)
            .await
            .unwrap();

        let reply = fx.get().await;

        assert_eq!(
            row(&reply.body, "audit")["last_run"],
            json!({"ran_at": "2026-06-01T04:19:00Z", "deleted": 12})
        );
        assert_eq!(row(&reply.body, "messages")["last_run"]["deleted"], 0);
        assert!(row(&reply.body, "job_history")["last_run"].is_null());
    }
}
