//! The sources API end to end, driven through the router against the fake source.

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
    use zirv_flow_core::{
        HmacRoot, Keyring, MasterKey, Role, Scope, Slug, WorkspaceId, WorkspaceName,
        deployment_key_aad, seal,
    };
    use zirv_flow_store::secrets;

    use crate::admin::{self, Context};
    use crate::{AppState, router};

    const PASSWORD: &str = "correct horse battery";
    /// Distinct from anything else a response or a table may hold.
    const MARKER: &str = "zz-credential-marker-4711";
    const REPLACED_MARKER: &str = "zz-replaced-marker-0815";

    struct Reply {
        status: StatusCode,
        #[allow(dead_code)]
        headers: HeaderMap,
        body: Value,
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
        admin: String,
        member: Login,
        /// Every response body seen, as text, to scan for credential values.
        seen: std::sync::Mutex<Vec<String>>,
    }

    async fn call(
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
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    async fn fixture_with(pool: &PgPool, keyring: Option<Arc<Keyring>>) -> Fixture {
        zirv_flow_store::migrate(pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let mut state = AppState::new(pool.clone(), Arc::new(String::new));
        if let Some(keyring) = keyring {
            let root = HmacRoot::bootstrap_from(keyring.current()).to_secret_bytes();
            let sealed = seal(
                &keyring,
                root.expose(),
                &deployment_key_aad(secrets::HMAC_ROOT),
            );
            secrets::insert_hmac_root_if_absent(pool, &sealed)
                .await
                .unwrap();
            state = state.with_keyring(keyring);
        }
        let router = router(state);
        let key = |scopes: &'static [Scope]| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), ws, "sources", scopes)
                    .await
                    .unwrap()
                    .plaintext
            }
        };
        let read = key(&[Scope::Read]).await;
        let admin_key = key(&[Scope::Admin, Scope::Read]).await;
        let user = admin::create_user(pool, &Context::system(), "member@example.com", PASSWORD)
            .await
            .unwrap();
        admin::set_member(pool, &Context::system(), ws, user.id, Role::Member)
            .await
            .unwrap();
        let reply = call(
            &router,
            "POST",
            "/api/v1/session",
            Some(json!({"email": "member@example.com", "password": PASSWORD})),
            Auth::Anonymous,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        let cookie = reply.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        Fixture {
            pool: pool.clone(),
            router,
            ws,
            read,
            admin: admin_key,
            member: Login {
                cookie,
                csrf: reply.body["csrf_token"].as_str().unwrap().to_owned(),
            },
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        fixture_with(
            pool,
            Some(Arc::new(Keyring::new(MasterKey::generate(), None).unwrap())),
        )
        .await
    }

    /// The member's session or the read-only key.
    fn who_is<'a>(fx: &'a Fixture, who: &str) -> Auth<'a> {
        if who == "member" {
            Auth::Session(&fx.member)
        } else {
            Auth::Key(&fx.read)
        }
    }

    impl Fixture {
        fn path(&self, tail: &str) -> String {
            format!("/api/v1/workspaces/{}{tail}", self.ws.as_uuid())
        }

        async fn send(
            &self,
            method: &str,
            tail: &str,
            body: Option<Value>,
            auth: Auth<'_>,
        ) -> Reply {
            let reply = call(&self.router, method, &self.path(tail), body, auth).await;
            self.seen.lock().unwrap().push(reply.body.to_string());
            reply
        }

        async fn as_admin(&self, method: &str, tail: &str, body: Option<Value>) -> Reply {
            self.send(method, tail, body, Auth::Key(&self.admin)).await
        }

        async fn create(&self, name: &str, token: &str, streams: u32) -> Reply {
            self.as_admin(
                "POST",
                "/sources",
                Some(json!({
                    "kind": "fake",
                    "name": name,
                    "config": {"streams": streams},
                    "credential": {"token": token},
                })),
            )
            .await
        }

        async fn create_ok(&self, name: &str, token: &str, streams: u32) -> String {
            let reply = self.create(name, token, streams).await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().to_owned()
        }

        async fn audit_actions(&self) -> Vec<String> {
            let mut entries = zirv_flow_store::list_audit(&self.pool, 200).await.unwrap();
            entries.reverse();
            entries.into_iter().map(|entry| entry.action).collect()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_kinds_list_the_fake_with_its_field_specs_to_any_signed_in_caller(pool: PgPool) {
        let fx = fixture(&pool).await;

        let as_key = call(
            &fx.router,
            "GET",
            "/api/v1/source-kinds",
            None,
            Auth::Key(&fx.read),
        )
        .await;
        let as_member = call(
            &fx.router,
            "GET",
            "/api/v1/source-kinds",
            None,
            Auth::Session(&fx.member),
        )
        .await;
        let anonymous = call(
            &fx.router,
            "GET",
            "/api/v1/source-kinds",
            None,
            Auth::Anonymous,
        )
        .await;

        assert_eq!(as_key.status, StatusCode::OK);
        assert_eq!(as_member.body, as_key.body);
        assert_eq!(as_key.body[0]["kind"], "fake");
        assert_eq!(as_key.body[0]["config_fields"][0]["key"], "streams");
        assert_eq!(as_key.body[0]["credential_fields"][0]["secret"], true);
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_admin_creates_reads_lists_updates_tests_discovers_configures_and_deletes(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;

        let created = fx.create("CRM", "ok", 2).await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let id = created.body["id"].as_str().unwrap().to_owned();
        assert_eq!(created.body["credential_fields_set"], json!(["token"]));
        assert_eq!(created.body["config"], json!({"streams": 2}));
        assert_eq!(created.body["enabled"], true);

        let read = fx.as_admin("GET", &format!("/sources/{id}"), None).await;
        let listed = fx.as_admin("GET", "/sources", None).await;
        assert_eq!(read.body, created.body);
        assert_eq!(listed.body[0]["id"], id.as_str());

        let passing = fx
            .as_admin("POST", &format!("/sources/{id}/test"), None)
            .await;
        assert_eq!(passing.body, json!({"ok": true, "error": null}));

        let renamed = fx
            .as_admin(
                "PATCH",
                &format!("/sources/{id}"),
                Some(
                    json!({"name": "CRM 2", "enabled": false, "schedule": "0 * * * *",
                    "credential": {"token": "wrong"}}),
                ),
            )
            .await;
        assert_eq!(renamed.status, StatusCode::OK, "{:?}", renamed.body);
        assert_eq!(renamed.body["name"], "CRM 2");
        assert_eq!(renamed.body["enabled"], false);
        assert_eq!(renamed.body["schedule"], "0 * * * *");
        let failing = fx
            .as_admin("POST", &format!("/sources/{id}/test"), None)
            .await;
        assert_eq!(failing.status, StatusCode::OK);
        assert_eq!(
            failing.body,
            json!({"ok": false, "error": "the source refused the credential"})
        );
        let cleared = fx
            .as_admin(
                "PATCH",
                &format!("/sources/{id}"),
                Some(json!({"schedule": ""})),
            )
            .await;
        assert_eq!(cleared.body["schedule"], Value::Null);

        let discovered = fx
            .as_admin("POST", &format!("/sources/{id}/discover"), None)
            .await;
        assert_eq!(discovered.status, StatusCode::OK, "{:?}", discovered.body);
        let names: Vec<_> = discovered
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].clone())
            .collect();
        assert_eq!(names, [json!("stream_1"), json!("stream_2")]);
        assert_eq!(discovered.body[0]["enabled"], false);
        assert_eq!(
            discovered.body[0]["discovered"]["default_cursor_field"],
            "updated_at"
        );

        let no_cursor = fx
            .as_admin(
                "PUT",
                &format!("/sources/{id}/streams/stream_1"),
                Some(json!({"enabled": true, "sync_mode": "incremental"})),
            )
            .await;
        assert_eq!(no_cursor.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(no_cursor.body["errors"][0]["field"], "cursor_field");
        let configured = fx
            .as_admin(
                "PUT",
                &format!("/sources/{id}/streams/stream_1"),
                Some(json!({"enabled": true, "sync_mode": "incremental", "cursor_field": "updated_at",
                    "primary_key": ["id"], "staleness_threshold_seconds": 3600})),
            )
            .await;
        assert_eq!(configured.status, StatusCode::OK, "{:?}", configured.body);
        assert_eq!(configured.body["sync_mode"], "incremental");
        assert_eq!(configured.body["primary_key"], json!(["id"]));

        let again = fx
            .as_admin("POST", &format!("/sources/{id}/discover"), None)
            .await;
        assert_eq!(
            again.body[0]["enabled"], true,
            "discovery keeps the user's settings"
        );
        assert_eq!(again.body[0]["staleness_threshold_seconds"], 3600);
        let streams = fx
            .as_admin("GET", &format!("/sources/{id}/streams"), None)
            .await;
        assert_eq!(streams.body, again.body);

        let deleted = fx.as_admin("DELETE", &format!("/sources/{id}"), None).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        let gone = fx.as_admin("GET", &format!("/sources/{id}"), None).await;
        assert_eq!(gone.status, StatusCode::NOT_FOUND);
        for table in ["sources", "streams", "encrypted_secrets"] {
            let count: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(count, 0, "{table}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_member_and_a_read_key_list_and_read_but_every_mutation_is_forbidden(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.create_ok("CRM", "ok", 1).await;
        fx.as_admin("POST", &format!("/sources/{id}/discover"), None)
            .await;

        for who in ["member", "read key"] {
            let reads = [
                "/sources".to_owned(),
                format!("/sources/{id}"),
                format!("/sources/{id}/streams"),
            ];
            for tail in &reads {
                let reply = fx.send("GET", tail, None, who_is(&fx, who)).await;
                assert_eq!(reply.status, StatusCode::OK, "{tail}: {:?}", reply.body);
            }
            let writes = [
                (
                    "POST",
                    "/sources".to_owned(),
                    json!({"kind": "fake", "name": "x", "config": {}, "credential": {"token": "ok"}}),
                ),
                ("PATCH", format!("/sources/{id}"), json!({"name": "y"})),
                ("DELETE", format!("/sources/{id}"), Value::Null),
                ("POST", format!("/sources/{id}/test"), Value::Null),
                ("POST", format!("/sources/{id}/discover"), Value::Null),
                (
                    "PUT",
                    format!("/sources/{id}/streams/stream_1"),
                    json!({"enabled": true, "sync_mode": "full_refresh"}),
                ),
            ];
            for (method, tail, body) in writes {
                let body = (!body.is_null()).then_some(body);
                let reply = fx.send(method, &tail, body, who_is(&fx, who)).await;
                assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {tail}");
            }
        }
        let still = fx.as_admin("GET", &format!("/sources/{id}"), None).await;
        assert_eq!(still.body["name"], "CRM");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_duplicate_name_is_a_conflict_and_a_foreign_source_is_not_found(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.create_ok("CRM", "ok", 1).await;
        let other = zirv_flow_store::create_workspace(
            &pool,
            &WorkspaceName::parse("theirs").unwrap(),
            &Slug::parse("theirs").unwrap(),
        )
        .await
        .unwrap()
        .id;

        let duplicate = fx.create("CRM", "ok", 1).await;
        let second = fx.create_ok("Other", "ok", 1).await;
        let rename = fx
            .as_admin(
                "PATCH",
                &format!("/sources/{second}"),
                Some(json!({"name": "CRM"})),
            )
            .await;
        let foreign = call(
            &fx.router,
            "GET",
            &format!("/api/v1/workspaces/{}/sources/{id}", other.as_uuid()),
            None,
            Auth::Key(&fx.admin),
        )
        .await;

        assert_eq!(duplicate.status, StatusCode::CONFLICT);
        assert_eq!(rename.status, StatusCode::CONFLICT);
        assert_eq!(foreign.status, StatusCode::NOT_FOUND);
        let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM encrypted_secrets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kept, 2, "a refused create rolls its secret back");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn invalid_input_is_a_422_naming_each_field_and_stores_nothing(pool: PgPool) {
        let fx = fixture(&pool).await;
        let create = |body: Value| fx.as_admin("POST", "/sources", Some(body));

        let unknown_kind = create(json!({"kind": "nope", "name": "x", "config": {}})).await;
        let bad_config = create(json!({"kind": "fake", "name": "x",
            "config": {"streams": "many", "extra": 1}, "credential": {"token": "ok"}}))
        .await;
        let no_credential = create(json!({"kind": "fake", "name": "x", "config": {}})).await;
        let wrong_credential = create(json!({"kind": "fake", "name": "x", "config": {},
            "credential": {"token": "ok", "password": "p"}}))
        .await;
        let bad_schedule = create(json!({"kind": "fake", "name": "x", "config": {},
            "credential": {"token": "ok"}, "schedule": "sometimes"}))
        .await;
        let bad_name = create(json!({"kind": "fake", "name": "  ", "config": {},
            "credential": {"token": "ok"}}))
        .await;

        let fields = |reply: &Reply| -> Vec<String> {
            reply.body["errors"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["field"].as_str().unwrap().to_owned())
                .collect()
        };
        for reply in [
            &unknown_kind,
            &bad_config,
            &no_credential,
            &wrong_credential,
            &bad_schedule,
            &bad_name,
        ] {
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{:?}",
                reply.body
            );
        }
        assert_eq!(fields(&unknown_kind), ["kind"]);
        assert_eq!(fields(&bad_config), ["config.extra", "config.streams"]);
        assert_eq!(fields(&no_credential), ["credential.token"]);
        assert_eq!(fields(&wrong_credential), ["credential.password"]);
        assert_eq!(fields(&bad_schedule), ["schedule"]);
        assert_eq!(fields(&bad_name), ["name"]);
        let id = fx.create_ok("CRM", "ok", 1).await;
        let empty = fx
            .as_admin("PATCH", &format!("/sources/{id}"), Some(json!({})))
            .await;
        let empty_credential = fx
            .as_admin(
                "PATCH",
                &format!("/sources/{id}"),
                Some(json!({"credential": {}})),
            )
            .await;
        assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(empty_credential.status, StatusCode::UNPROCESSABLE_ENTITY);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sources")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        let secrets_stored: i64 = sqlx::query_scalar("SELECT count(*) FROM encrypted_secrets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            secrets_stored, 1,
            "a refused create leaves no secret behind"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn invalid_stream_settings_are_refused_with_reasons(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.create_ok("CRM", "ok", 1).await;
        fx.as_admin("POST", &format!("/sources/{id}/discover"), None)
            .await;
        let put = |body: Value| async {
            let tail = format!("/sources/{id}/streams/stream_1");
            fx.as_admin("PUT", &tail, Some(body)).await
        };

        let bad = put(
            json!({"enabled": true, "sync_mode": "weekly", "cursor_field": "nope",
            "primary_key": ["missing"], "staleness_threshold_seconds": 0}),
        )
        .await;
        let missing = fx
            .as_admin(
                "PUT",
                &format!("/sources/{id}/streams/no_such"),
                Some(json!({"enabled": true, "sync_mode": "full_refresh"})),
            )
            .await;

        assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
        let fields: Vec<_> = bad.body["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["field"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            fields,
            [
                "sync_mode",
                "cursor_field",
                "primary_key",
                "staleness_threshold_seconds"
            ]
        );
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        let stream = fx
            .as_admin("GET", &format!("/sources/{id}/streams"), None)
            .await;
        assert_eq!(stream.body[0]["enabled"], false);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_mutation_is_audited_without_a_credential_value(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.create_ok("CRM", MARKER, 1).await;
        fx.as_admin(
            "PATCH",
            &format!("/sources/{id}"),
            Some(json!({"credential": {"token": REPLACED_MARKER}})),
        )
        .await;
        fx.as_admin("POST", &format!("/sources/{id}/test"), None)
            .await;
        fx.as_admin("POST", &format!("/sources/{id}/discover"), None)
            .await;
        fx.as_admin(
            "PUT",
            &format!("/sources/{id}/streams/stream_1"),
            Some(json!({"enabled": true, "sync_mode": "full_refresh"})),
        )
        .await;
        fx.as_admin("DELETE", &format!("/sources/{id}"), None).await;

        let actions = fx.audit_actions().await;
        let ours: Vec<_> = actions
            .iter()
            .filter(|action| {
                action.starts_with("source.")
                    || action.starts_with("stream.")
                    || action.starts_with("secret.")
            })
            .map(String::as_str)
            .collect();

        assert_eq!(
            ours,
            [
                "secret.created",
                "source.created",
                "secret.replaced",
                "source.updated",
                "source.tested",
                "source.discovered",
                "stream.configured",
                "source.deleted",
                "secret.deleted",
            ]
        );
        let summaries: String = zirv_flow_store::list_audit(&pool, 200)
            .await
            .unwrap()
            .iter()
            .map(|entry| entry.summary.to_string())
            .collect();
        assert!(!summaries.contains(MARKER) && !summaries.contains(REPLACED_MARKER));
        let created = zirv_flow_store::list_audit(&pool, 200)
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.action == "source.created")
            .unwrap();
        assert_eq!(created.summary["credential_fields"], json!(["token"]));
    }

    /// The text of every row of every table, in both bytea encodings, holds no plaintext marker.
    async fn tables_hold_no(pool: &PgPool, markers: &[&str]) {
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT tablename FROM pg_tables WHERE schemaname = current_schema()",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        for encoding in ["hex", "escape"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "SET bytea_output = '{encoding}'"
            )))
            .execute(&mut *connection)
            .await
            .unwrap();
            for table in &tables {
                let text: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT string_agg(t::text, E'\\n') FROM \"{table}\" t"
                )))
                .fetch_one(&mut *connection)
                .await
                .unwrap();
                let text = text.unwrap_or_default();
                for marker in markers {
                    let hex: String = marker.bytes().map(|byte| format!("{byte:02x}")).collect();
                    assert!(!text.contains(marker), "{marker} in {table} ({encoding})");
                    assert!(
                        !text.contains(&hex),
                        "{marker} as hex in {table} ({encoding})"
                    );
                }
            }
        }
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_credential_is_ciphertext_at_rest_and_in_no_response_or_log_line(pool: PgPool) {
        let captured = Captured::default();
        let _logging = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(captured.clone())
                .finish(),
        );
        let fx = fixture(&pool).await;

        let id = fx.create_ok("CRM", MARKER, 2).await;
        fx.as_admin("GET", &format!("/sources/{id}"), None).await;
        fx.as_admin("GET", "/sources", None).await;
        fx.as_admin("POST", &format!("/sources/{id}/test"), None)
            .await;
        fx.as_admin("POST", &format!("/sources/{id}/discover"), None)
            .await;
        fx.as_admin("GET", &format!("/sources/{id}/streams"), None)
            .await;
        fx.as_admin(
            "PUT",
            &format!("/sources/{id}/streams/stream_1"),
            Some(json!({"enabled": true, "sync_mode": "full_refresh"})),
        )
        .await;
        fx.as_admin(
            "PATCH",
            &format!("/sources/{id}"),
            Some(json!({"credential": {"token": REPLACED_MARKER}})),
        )
        .await;
        let after = fx.as_admin("GET", &format!("/sources/{id}"), None).await;
        // A refused request must not echo the value either.
        fx.as_admin(
            "POST",
            "/sources",
            Some(json!({"kind": "fake", "name": "x",
            "config": {"streams": MARKER}, "credential": {"token": MARKER, "other": MARKER}})),
        )
        .await;
        fx.send("GET", "/sources", None, Auth::Session(&fx.member))
            .await;

        assert_eq!(after.body["credential_fields_set"], json!(["token"]));
        let seen = fx.seen.lock().unwrap().join("\n");
        assert!(
            !seen.contains(MARKER) && !seen.contains(REPLACED_MARKER),
            "a response held a credential"
        );
        let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(
            !logs.contains(MARKER) && !logs.contains(REPLACED_MARKER),
            "a log line held a credential"
        );
        tables_hold_no(&pool, &[MARKER, REPLACED_MARKER]).await;
        // Control: the scan does find a plaintext marker.
        sqlx::query("CREATE TABLE scan_control (note text)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO scan_control (note) VALUES ($1)")
            .bind(MARKER)
            .execute(&pool)
            .await
            .unwrap();
        let caught = std::panic::AssertUnwindSafe(tables_hold_no(&pool, &[MARKER]));
        assert!(
            futures_util::FutureExt::catch_unwind(caught).await.is_err(),
            "the scan must detect a marker"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_a_master_key_a_source_with_a_credential_is_unavailable(pool: PgPool) {
        let fx = fixture_with(&pool, None).await;

        let created = fx.create("CRM", "ok", 1).await;

        assert_eq!(created.status, StatusCode::SERVICE_UNAVAILABLE);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sources")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failing_check_flag_is_reported_as_a_failed_test_not_an_error(pool: PgPool) {
        let fx = fixture(&pool).await;
        let reply = fx
            .as_admin(
                "POST",
                "/sources",
                Some(
                    json!({"kind": "fake", "name": "CRM", "config": {"fail_check": true},
                    "credential": {"token": "ok"}}),
                ),
            )
            .await;
        let id = reply.body["id"].as_str().unwrap().to_owned();

        let tested = fx
            .as_admin("POST", &format!("/sources/{id}/test"), None)
            .await;

        assert_eq!(tested.status, StatusCode::OK);
        assert_eq!(tested.body["ok"], false);
        assert_eq!(tested.body["error"], "the source could not be reached");
    }
}
