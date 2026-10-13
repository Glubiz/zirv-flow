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
        fixture_with_egress(pool, keyring, None).await
    }

    /// `fixture_with`, and the egress policy the server enforces when one is given.
    async fn fixture_with_egress(
        pool: &PgPool,
        keyring: Option<Arc<Keyring>>,
        egress: Option<zirv_flow_core::egress::EgressPolicy>,
    ) -> Fixture {
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
        if let Some(policy) = egress {
            state = state.with_egress_policy(Arc::new(policy));
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

    fn trait_definition(key: &str, pii: bool) -> zirv_flow_store::NewTraitDefinition {
        zirv_flow_store::NewTraitDefinition {
            key: key.into(),
            trait_type: zirv_flow_core::people::TraitType::String,
            precedence: zirv_flow_core::people::Precedence::Latest,
            source_order: Vec::new(),
            pii,
            channel_address: false,
            inferred: false,
            description: String::new(),
        }
    }

    fn mapping_body() -> Value {
        json!({"key_column": "id", "columns": [
            {"column": "email", "target": {"type": "identifier", "kind": "email"}},
            {"column": "name", "target": {"type": "trait", "key": "plan"}}
        ]})
    }

    impl Fixture {
        /// A source with `streams` discovered streams, `stream_1` enabled with a mapping.
        async fn mapped_source(&self, streams: u32) -> String {
            zirv_flow_store::create_trait_definition(
                &self.pool,
                self.ws,
                &trait_definition("plan", false),
            )
            .await
            .unwrap();
            let id = self.create_ok("CRM", "ok", streams).await;
            self.as_admin("POST", &format!("/sources/{id}/discover"), None)
                .await;
            let put = self
                .as_admin(
                    "PUT",
                    &format!("/sources/{id}/streams/stream_1"),
                    Some(json!({"enabled": true, "sync_mode": "full_refresh",
                        "mapping": mapping_body()})),
                )
                .await;
            assert_eq!(put.status, StatusCode::OK, "{:?}", put.body);
            id
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_stream_mapping_is_checked_against_the_streams_fields_and_the_registry(pool: PgPool) {
        let fx = fixture(&pool).await;
        zirv_flow_store::create_trait_definition(&pool, fx.ws, &trait_definition("plan", false))
            .await
            .unwrap();
        let id = fx.create_ok("CRM", "ok", 1).await;
        fx.as_admin("POST", &format!("/sources/{id}/discover"), None)
            .await;
        let tail = format!("/sources/{id}/streams/stream_1");
        let settings = |mapping: Value| json!({"enabled": true, "sync_mode": "full_refresh", "mapping": mapping});

        let unknown_column = fx
            .as_admin(
                "PUT",
                &tail,
                Some(settings(json!({"columns": [
                    {"column": "mail", "target": {"type": "identifier", "kind": "email"}}]}))),
            )
            .await;
        let unknown_trait = fx
            .as_admin(
                "PUT",
                &tail,
                Some(settings(json!({"columns": [
                    {"column": "email", "target": {"type": "identifier", "kind": "email"}},
                    {"column": "name", "target": {"type": "trait", "key": "nope"}}]}))),
            )
            .await;
        let list_target = fx
            .as_admin(
                "PUT",
                &tail,
                Some(settings(json!({"columns": [
                    {"column": "email", "target": {"type": "identifier", "kind": "email"}},
                    {"column": "name", "target": {"type": "list",
                        "list_id": "00000000-0000-0000-0000-000000000000"}}]}))),
            )
            .await;
        let none = fx
            .as_admin("GET", &format!("/sources/{id}/streams"), None)
            .await;
        assert_eq!(unknown_column.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            unknown_column.body["detail"],
            "mapping: unknown_column (column \"mail\")"
        );
        assert_eq!(unknown_trait.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(list_target.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(none.body[0]["mapping"], Value::Null);

        let saved = fx
            .as_admin("PUT", &tail, Some(settings(mapping_body())))
            .await;
        // A later change of the other settings leaves the mapping as it is.
        let kept = fx
            .as_admin(
                "PUT",
                &tail,
                Some(json!({"enabled": false, "sync_mode": "full_refresh"})),
            )
            .await;

        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert_eq!(saved.body["mapping"]["key_column"], "id");
        assert_eq!(kept.body["enabled"], false);
        assert_eq!(kept.body["mapping"], saved.body["mapping"]);
        let member = fx
            .send(
                "PUT",
                &tail,
                Some(settings(mapping_body())),
                Auth::Session(&fx.member),
            )
            .await;
        assert_eq!(member.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn admins_start_and_cancel_runs_members_read_them_and_both_are_audited(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.mapped_source(2).await;
        let runs = format!("/sources/{id}/runs");
        // stream_2 is enabled but has no mapping.
        fx.as_admin(
            "PUT",
            &format!("/sources/{id}/streams/stream_2"),
            Some(json!({"enabled": true, "sync_mode": "full_refresh"})),
        )
        .await;

        let unmapped = fx
            .as_admin("POST", &runs, Some(json!({"stream": "stream_2"})))
            .await;
        let unknown = fx
            .as_admin("POST", &runs, Some(json!({"stream": "nope"})))
            .await;
        let member = fx
            .send("POST", &runs, Some(json!({})), Auth::Session(&fx.member))
            .await;
        let reader = fx
            .send("POST", &runs, Some(json!({})), Auth::Key(&fx.read))
            .await;
        let started = fx
            .as_admin("POST", &runs, Some(json!({"stream": "stream_1"})))
            .await;
        let again = fx
            .as_admin("POST", &runs, Some(json!({"stream": "stream_1"})))
            .await;
        let all = fx.as_admin("POST", &runs, Some(json!({}))).await;

        assert_eq!(unmapped.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(unmapped.body["code"], "stream_not_mapped");
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        assert_eq!(member.status, StatusCode::FORBIDDEN);
        assert_eq!(reader.status, StatusCode::FORBIDDEN);
        assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
        let run = &started.body[0];
        assert_eq!(started.body.as_array().unwrap().len(), 1);
        assert_eq!(run["state"], "queued");
        assert_eq!(run["trigger"], "manual");
        assert_eq!(run["stream"], "stream_1");
        assert_eq!(run["attempt"], 0);
        assert_eq!(run["trace_id"].as_str().unwrap().len(), 32);
        assert_eq!(again.status, StatusCode::CONFLICT);
        assert_eq!(again.body["code"], "sync_run_active");
        assert_eq!(
            all.status,
            StatusCode::CONFLICT,
            "every eligible stream is active"
        );
        let run_id = run["id"].as_str().unwrap().to_owned();

        // The queued run has a job under the run's trace, caused by the run.
        let job: (String, String, uuid::Uuid) = sqlx::query_as(
            "SELECT cause_kind, cause_id, trace_id FROM jobs WHERE kind = 'sync.run'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (job.0.as_str(), job.1.as_str()),
            ("sync_run", run_id.as_str())
        );
        assert_eq!(
            job.2.simple().to_string(),
            run["trace_id"].as_str().unwrap()
        );

        // Members and read keys list and read; a foreign id is a 404.
        for who in ["member", "key"] {
            let list = fx.send("GET", &runs, None, who_is(&fx, who)).await;
            assert_eq!(list.status, StatusCode::OK, "{who}");
            assert_eq!(list.body["runs"][0]["id"], run_id.as_str());
            assert_eq!(list.body["next_cursor"], Value::Null);
            let one = fx
                .send("GET", &format!("{runs}/{run_id}"), None, who_is(&fx, who))
                .await;
            assert_eq!(one.status, StatusCode::OK, "{who}");
            assert_eq!(one.body["id"], run_id.as_str());
            assert_eq!(one.body["rejects"], json!([]));
            let denied = fx
                .send(
                    "POST",
                    &format!("{runs}/{run_id}/cancel"),
                    None,
                    who_is(&fx, who),
                )
                .await;
            assert_eq!(denied.status, StatusCode::FORBIDDEN, "{who}");
        }
        let other_stream = fx
            .as_admin("GET", &format!("{runs}?stream=stream_2"), None)
            .await;
        assert_eq!(other_stream.body["runs"], json!([]));
        let missing = fx
            .as_admin("GET", &format!("{runs}/{}", uuid::Uuid::now_v7()), None)
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        let bad_limit = fx.as_admin("GET", &format!("{runs}?limit=0"), None).await;
        assert_eq!(bad_limit.status, StatusCode::BAD_REQUEST);

        let cancelled = fx
            .as_admin("POST", &format!("{runs}/{run_id}/cancel"), None)
            .await;
        let twice = fx
            .as_admin("POST", &format!("{runs}/{run_id}/cancel"), None)
            .await;
        let unknown_run = fx
            .as_admin(
                "POST",
                &format!("{runs}/{}/cancel", uuid::Uuid::now_v7()),
                None,
            )
            .await;
        assert_eq!(cancelled.status, StatusCode::OK, "{:?}", cancelled.body);
        assert_eq!(cancelled.body["state"], "cancelled");
        assert_eq!(twice.status, StatusCode::CONFLICT);
        assert_eq!(unknown_run.status, StatusCode::NOT_FOUND);

        // Paging: three runs of the stream, two per page.
        for _ in 0..2 {
            let more = fx
                .as_admin("POST", &runs, Some(json!({"stream": "stream_1"})))
                .await;
            let id = more.body[0]["id"].as_str().unwrap().to_owned();
            fx.as_admin("POST", &format!("{runs}/{id}/cancel"), None)
                .await;
        }
        let first = fx.as_admin("GET", &format!("{runs}?limit=2"), None).await;
        let cursor = first.body["next_cursor"].as_str().unwrap().to_owned();
        let second = fx
            .as_admin("GET", &format!("{runs}?limit=2&cursor={cursor}"), None)
            .await;
        assert_eq!(first.body["runs"].as_array().unwrap().len(), 2);
        assert_eq!(second.body["runs"].as_array().unwrap().len(), 1);
        assert_eq!(second.body["runs"][0]["id"], run_id.as_str());

        // Audited, with ids only.
        let actions = fx.audit_actions().await;
        assert_eq!(
            actions.iter().filter(|a| *a == "sync_run.started").count(),
            3
        );
        assert_eq!(
            actions
                .iter()
                .filter(|a| *a == "sync_run.cancelled")
                .count(),
            3
        );
        let entries = zirv_flow_store::list_audit(&pool, 200).await.unwrap();
        for entry in entries
            .iter()
            .filter(|entry| entry.action.starts_with("sync_run."))
        {
            let shown = entry.summary.to_string();
            assert!(
                !shown.contains("example.com") && !shown.contains("ok"),
                "{shown}"
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reject_sample_shows_only_values_the_mapping_sends_to_non_pii_traits(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.mapped_source(1).await;
        let (stream_id, source_id): (uuid::Uuid, uuid::Uuid) =
            sqlx::query_as("SELECT id, source_id FROM streams WHERE name = 'stream_1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let run_id = uuid::Uuid::now_v7();
        sqlx::query(
            "INSERT INTO sync_runs (id, workspace_id, source_id, stream_id, state, trigger, \
                sync_mode, trace_id, records_read, records_rejected, error_code) \
             VALUES ($1, $2, $3, $4, 'failed', 'manual', 'full_refresh', gen_random_uuid(), 2, 1, \
                     'reject_threshold')",
        )
        .bind(run_id)
        .bind(fx.ws.as_uuid())
        .bind(source_id)
        .bind(stream_id)
        .execute(&pool)
        .await
        .unwrap();
        // A sample stored with raw values, as if the trait had been public when it was written.
        sqlx::query(
            "INSERT INTO ingest_rejects (id, workspace_id, received_at, source, kind, name, reason, \
                reasons, sample, trace_id, sync_run_id) \
             VALUES ($1, $2, now(), 's', 'record', 'stream_1', 'invalid_email', \
                '[{\"property\": \"email\", \"reason\": \"invalid_email\"}]', $3, gen_random_uuid(), $4)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(fx.ws.as_uuid())
        .bind(json!({"properties": {"name": "Ada Lovelace", "email": "zz-pii-marker@example.com",
            "id": "zz-key-marker"}, "masked": []}))
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        let tail = format!("/sources/{id}/runs/{run_id}");

        let public = fx.send("GET", &tail, None, Auth::Session(&fx.member)).await;
        sqlx::query("UPDATE trait_definitions SET pii = true WHERE key = 'plan'")
            .execute(&pool)
            .await
            .unwrap();
        let after = fx.send("GET", &tail, None, Auth::Key(&fx.read)).await;

        assert_eq!(public.status, StatusCode::OK, "{:?}", public.body);
        let reject = &public.body["rejects"][0];
        assert_eq!(reject["reason"], "invalid_email");
        assert_eq!(reject["reasons"][0]["property"], "email");
        assert_eq!(reject["sample"]["properties"]["name"], "Ada Lovelace");
        assert_eq!(reject["sample"]["properties"]["email"], Value::Null);
        assert_eq!(reject["sample"]["properties"]["id"], Value::Null);
        assert_eq!(reject["sample"]["masked"], json!(["email", "id"]));
        let shown = public.body.to_string();
        assert!(!shown.contains("zz-pii-marker") && !shown.contains("zz-key-marker"));
        // Once the trait is marked PII, its old samples hide the value too.
        assert_eq!(
            after.body["rejects"][0]["sample"]["properties"]["name"],
            Value::Null
        );
        assert!(!after.body.to_string().contains("Ada Lovelace"));
        assert_eq!(public.body["error_code"], "reject_threshold");
        assert_eq!(public.body["records_rejected"], 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sources_schedule_is_mirrored_to_the_schedule_the_leader_fires(pool: PgPool) {
        let fx = fixture(&pool).await;
        let rows = || async {
            sqlx::query_as::<_, (String, String, bool)>(
                "SELECT kind, cron, enabled FROM schedules WHERE source_id IS NOT NULL",
            )
            .fetch_all(&pool)
            .await
            .unwrap()
        };
        let created = fx
            .as_admin(
                "POST",
                "/sources",
                Some(json!({"kind": "fake", "name": "CRM", "config": {},
                    "credential": {"token": "ok"}, "schedule": "0 * * * *"})),
            )
            .await;
        let id = created.body["id"].as_str().unwrap().to_owned();
        assert_eq!(
            rows().await,
            [("sync.run".to_owned(), "0 * * * *".to_owned(), true)]
        );

        fx.as_admin(
            "PATCH",
            &format!("/sources/{id}"),
            Some(json!({"schedule": "30 * * * *"})),
        )
        .await;
        assert_eq!(
            rows().await,
            [("sync.run".to_owned(), "30 * * * *".to_owned(), true)]
        );
        fx.as_admin(
            "PATCH",
            &format!("/sources/{id}"),
            Some(json!({"enabled": false})),
        )
        .await;
        assert_eq!(
            rows().await,
            [("sync.run".to_owned(), "30 * * * *".to_owned(), false)]
        );
        fx.as_admin(
            "PATCH",
            &format!("/sources/{id}"),
            Some(json!({"schedule": ""})),
        )
        .await;
        assert!(
            rows().await.is_empty(),
            "clearing the schedule stops the firing"
        );
        fx.as_admin(
            "PATCH",
            &format!("/sources/{id}"),
            Some(json!({"schedule": "0 * * * *", "enabled": true})),
        )
        .await;
        assert_eq!(rows().await.len(), 1);
        let deleted = fx.as_admin("DELETE", &format!("/sources/{id}"), None).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        assert!(
            rows().await.is_empty(),
            "deleting the source stops the firing"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn changing_a_cursor_or_a_source_config_during_an_active_run_is_a_409(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.mapped_source(1).await;
        let stream = format!("/sources/{id}/streams/stream_1");
        let started = fx
            .as_admin(
                "POST",
                &format!("/sources/{id}/runs"),
                Some(json!({"stream": "stream_1"})),
            )
            .await;
        let run_id = started.body[0]["id"].as_str().unwrap().to_owned();
        let incremental = json!({"enabled": true, "sync_mode": "incremental",
            "cursor_field": "updated_at", "mapping": mapping_body()});

        let cursor = fx.as_admin("PUT", &stream, Some(incremental.clone())).await;
        let config = fx
            .as_admin(
                "PATCH",
                &format!("/sources/{id}"),
                Some(json!({"config": {"streams": 3}})),
            )
            .await;
        let staleness = fx
            .as_admin(
                "PUT",
                &stream,
                Some(json!({"enabled": true, "sync_mode": "full_refresh",
                    "staleness_threshold_seconds": 60})),
            )
            .await;
        let name = fx
            .as_admin(
                "PATCH",
                &format!("/sources/{id}"),
                Some(json!({"name": "CRM 2"})),
            )
            .await;

        for refused in [&cursor, &config] {
            assert_eq!(refused.status, StatusCode::CONFLICT, "{:?}", refused.body);
            assert_eq!(refused.body["code"], "sync_run_active");
            assert!(
                refused.body["detail"]
                    .as_str()
                    .unwrap()
                    .contains("cancel it")
            );
        }
        assert_eq!(staleness.status, StatusCode::OK, "{:?}", staleness.body);
        assert_eq!(name.status, StatusCode::OK);
        fx.as_admin("POST", &format!("/sources/{id}/runs/{run_id}/cancel"), None)
            .await;
        let after = fx.as_admin("PUT", &stream, Some(incremental)).await;
        assert_eq!(after.status, StatusCode::OK, "{:?}", after.body);
        assert_eq!(after.body["sync_mode"], "incremental");
    }

    // Mapping lists, paths and events on a stream, and the preview.

    impl Fixture {
        /// A source whose `stream_1` is discovered, keyed by `id`, with the `plan` trait.
        async fn keyed_source(&self) -> String {
            zirv_flow_store::create_trait_definition(
                &self.pool,
                self.ws,
                &trait_definition("plan", false),
            )
            .await
            .unwrap();
            let id = self.create_ok("CRM", "ok", 1).await;
            self.as_admin("POST", &format!("/sources/{id}/discover"), None)
                .await;
            id
        }

        async fn put_stream(&self, id: &str, key: &[&str], mapping: Option<Value>) -> Reply {
            let mut body =
                json!({"enabled": true, "sync_mode": "full_refresh", "primary_key": key});
            if let Some(mapping) = mapping {
                body["mapping"] = mapping;
            }
            self.as_admin(
                "PUT",
                &format!("/sources/{id}/streams/stream_1"),
                Some(body),
            )
            .await
        }

        async fn preview(&self, id: &str, body: Value, auth: Auth<'_>) -> Reply {
            self.send(
                "POST",
                &format!("/sources/{id}/streams/stream_1/preview"),
                Some(body),
                auth,
            )
            .await
        }

        /// A session of a workspace admin.
        async fn admin_session(&self) -> Login {
            let user =
                admin::create_user(&self.pool, &Context::system(), "boss@example.com", PASSWORD)
                    .await
                    .unwrap();
            admin::set_member(
                &self.pool,
                &Context::system(),
                self.ws,
                user.id,
                Role::Admin,
            )
            .await
            .unwrap();
            let reply = call(
                &self.router,
                "POST",
                "/api/v1/session",
                Some(json!({"email": "boss@example.com", "password": PASSWORD})),
                Auth::Anonymous,
            )
            .await;
            assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
            Login {
                cookie: reply.headers[header::SET_COOKIE]
                    .to_str()
                    .unwrap()
                    .split(';')
                    .next()
                    .unwrap()
                    .to_owned(),
                csrf: reply.body["csrf_token"].as_str().unwrap().to_owned(),
            }
        }
    }

    fn email_rule() -> Value {
        json!({"column": "email", "target": {"type": "identifier", "kind": "email"}})
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_list_target_is_accepted_when_the_list_exists_and_refused_when_it_does_not(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.keyed_source().await;
        let list = zirv_flow_store::create_list(&pool, fx.ws, "news", "", None)
            .await
            .unwrap();
        let with_list = |list: &str| {
            json!({"columns": [email_rule(),
                {"column": "name", "target": {"type": "list", "list_id": list}}]})
        };

        let saved = fx
            .put_stream(&id, &["id"], Some(with_list(&list.id.to_string())))
            .await;
        let missing = fx
            .put_stream(
                &id,
                &["id"],
                Some(with_list("00000000-0000-7000-8000-000000000000")),
            )
            .await;

        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert_eq!(
            saved.body["mapping"]["columns"][1]["target"]["list_id"],
            list.id.to_string()
        );
        assert_eq!(missing.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            missing.body["detail"],
            "mapping: unknown_list (column \"name\")"
        );
        // A list of another workspace is as good as none.
        let other = zirv_flow_store::create_workspace(
            &pool,
            &WorkspaceName::parse("theirs").unwrap(),
            &Slug::parse("theirs").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let foreign = zirv_flow_store::create_list(&pool, other, "news", "", None)
            .await
            .unwrap();
        let refused = fx
            .put_stream(&id, &["id"], Some(with_list(&foreign.id.to_string())))
            .await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_mapping_needs_a_primary_key_a_valid_name_and_columns_the_stream_has(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.keyed_source().await;
        let with_event = |event: Value| json!({"columns": [email_rule()], "event": event});
        let good = with_event(json!({
            "name": "seen",
            "timestamp": {"column": "updated_at"},
            "properties": [{"property": "who", "column": "name"}]
        }));

        let keyless = fx.put_stream(&id, &[], Some(good.clone())).await;
        let saved = fx.put_stream(&id, &["id"], Some(good)).await;
        let keyless_later = fx.put_stream(&id, &[], None).await;
        let bad_name = fx
            .put_stream(&id, &["id"], Some(with_event(json!({"name": "."}))))
            .await;
        let bad_property = fx
            .put_stream(
                &id,
                &["id"],
                Some(with_event(json!({"name": "e", "properties": [
                    {"property": "", "column": "name"}]}))),
            )
            .await;
        let ghost = fx
            .put_stream(
                &id,
                &["id"],
                Some(with_event(json!({"name": "e", "properties": [
                    {"property": "p", "column": "ghost"}]}))),
            )
            .await;

        assert_eq!(keyless.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            keyless.body["detail"],
            "mapping: event_requires_primary_key"
        );
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert_eq!(saved.body["mapping"]["event"]["name"], "seen");
        assert_eq!(keyless_later.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(bad_name.body["detail"], "mapping: invalid_event_name");
        assert_eq!(
            bad_property.body["detail"],
            "mapping: invalid_event_property (column \"\")"
        );
        assert_eq!(
            ghost.body["detail"],
            "mapping: unknown_column (column \"ghost\")"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_column_may_be_a_path_into_a_field_of_the_stream(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.keyed_source().await;
        let nested = |column: &str| {
            json!({"columns": [
                {"column": column, "target": {"type": "identifier", "kind": "email"}}]})
        };

        let into_a_field = fx
            .put_stream(&id, &["id"], Some(nested("email.address")))
            .await;
        let into_nothing = fx
            .put_stream(&id, &["id"], Some(nested("nope.address")))
            .await;

        assert_eq!(
            into_a_field.status,
            StatusCode::OK,
            "{:?}",
            into_a_field.body
        );
        assert_eq!(
            into_nothing.body["detail"],
            "mapping: unknown_column (column \"nope.address\")"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_an_admin_with_a_csrf_token_may_preview_and_other_callers_are_refused(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.keyed_source().await;
        fx.put_stream(&id, &["id"], Some(json!({"columns": [email_rule()]})))
            .await;
        let boss = fx.admin_session().await;
        let tail = format!("/sources/{id}/streams/stream_1/preview");

        let as_member = fx.preview(&id, json!({}), Auth::Session(&fx.member)).await;
        let as_read_key = fx.preview(&id, json!({}), Auth::Key(&fx.read)).await;
        let anonymous = fx.preview(&id, json!({}), Auth::Anonymous).await;
        let as_admin_key = fx.preview(&id, json!({}), Auth::Key(&fx.admin)).await;
        let as_admin = fx.preview(&id, json!({}), Auth::Session(&boss)).await;
        // The same admin session without its CSRF token.
        let request = Request::builder()
            .method("POST")
            .uri(fx.path(&tail))
            .header(header::COOKIE, &boss.cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let no_csrf = fx.router.clone().oneshot(request).await.unwrap().status();

        assert_eq!(as_member.status, StatusCode::FORBIDDEN);
        assert_eq!(as_read_key.status, StatusCode::FORBIDDEN);
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(no_csrf, StatusCode::FORBIDDEN);
        assert_eq!(
            as_admin_key.status,
            StatusCode::OK,
            "{:?}",
            as_admin_key.body
        );
        assert_eq!(as_admin.status, StatusCode::OK, "{:?}", as_admin.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_maps_the_first_records_with_the_run_s_function_and_writes_nothing(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.keyed_source().await;
        let list = zirv_flow_store::create_list(&pool, fx.ws, "news", "", None)
            .await
            .unwrap();
        let mapping = json!({
            "columns": [
                email_rule(),
                {"column": "name", "target": {"type": "trait", "key": "plan"}},
                {"column": "id", "target": {"type": "list", "list_id": list.id}}
            ],
            "event": {"name": "seen", "timestamp": {"column": "updated_at"},
                      "properties": [{"property": "who", "column": "name"}]}
        });
        let saved = fx.put_stream(&id, &["id"], Some(mapping)).await;
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        let tables = [
            "people",
            "identities",
            "source_records",
            "events",
            "ingest_rejects",
            "sync_runs",
            "list_members",
            "decision_records",
            "audit_log",
            "jobs",
        ];
        let counts = |pool: PgPool| async move {
            let mut out = Vec::new();
            for table in tables {
                let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT count(*) FROM {table}"
                )))
                .fetch_one(&pool)
                .await
                .unwrap();
                out.push(n);
            }
            out
        };
        let before = counts(pool.clone()).await;

        let three = fx
            .preview(&id, json!({"limit": 3}), Auth::Key(&fx.admin))
            .await;
        let all = fx.preview(&id, json!({}), Auth::Key(&fx.admin)).await;

        assert_eq!(three.status, StatusCode::OK, "{:?}", three.body);
        assert_eq!(three.body["read"], 3);
        assert_eq!(three.body["truncated"], true);
        let first = &three.body["records"][0];
        assert_eq!(first["result"], "mapped");
        assert_eq!(first["key"], "stream_1-0");
        assert_eq!(first["identifiers"][0]["kind"], "email");
        assert_eq!(first["identifiers"][0]["value"], "stream_1-0@example.com");
        assert_eq!(first["traits"]["plan"], "stream_1 record 0");
        assert_eq!(first["lists"], json!([]), "id 0 is not a truthy flag");
        assert_eq!(three.body["records"][1]["lists"][0], list.id.to_string());
        assert_eq!(first["event"]["name"], "seen");
        assert_eq!(first["event"]["occurred_at"], "2026-01-01T00:00:00Z");
        assert_eq!(first["event"]["properties"]["who"], "stream_1 record 0");
        // The fake stream holds five records: the default limit shows them all.
        assert_eq!(all.body["read"], 5);
        assert_eq!(all.body["truncated"], false);
        assert_eq!(
            counts(pool.clone()).await,
            before,
            "a preview writes nothing"
        );
        let audit = fx.audit_actions().await;
        assert!(
            !audit.iter().any(|action| action.contains("preview")),
            "{audit:?}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_of_a_draft_uses_the_draft_and_a_rejected_record_lists_codes_only(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.keyed_source().await;
        // The fake source can be made to hold records whose email is not an email.
        sqlx::query("UPDATE sources SET config = config || '{\"invalid\": \"1\"}'::jsonb")
            .execute(&pool)
            .await
            .unwrap();
        fx.put_stream(&id, &["id"], Some(json!({"columns": [email_rule()]})))
            .await;

        let reply = fx
            .preview(
                &id,
                json!({"mapping": {"columns": [
                    {"column": "email", "target": {"type": "identifier", "kind": "email"},
                     "transforms": [{"op": "upper"}]}]}, "limit": 3}),
                Auth::Key(&fx.admin),
            )
            .await;

        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        assert_eq!(
            reply.body["records"][0]["identifiers"][0]["value"], "stream_1-0@example.com",
            "the email kind normalizes after the transforms"
        );
        let rejected = &reply.body["records"][1];
        assert_eq!(rejected["result"], "rejected");
        assert_eq!(rejected["errors"][0]["field"], "email");
        assert_eq!(rejected["errors"][0]["reason"], "invalid_identifier");
        assert!(!rejected.to_string().contains("not-an-email"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_refuses_what_a_save_refuses_and_what_it_cannot_find(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.keyed_source().await;
        let key = Auth::Key(&fx.admin);

        let unmapped = fx.preview(&id, json!({}), Auth::Key(&fx.admin)).await;
        let invalid_draft = fx
            .preview(
                &id,
                json!({"mapping": {"columns": [
                    {"column": "mail", "target": {"type": "identifier", "kind": "email"}}]}}),
                Auth::Key(&fx.admin),
            )
            .await;
        let keyless_event = {
            fx.put_stream(&id, &[], None).await;
            fx.preview(
                &id,
                json!({"mapping": {"columns": [email_rule()], "event": {"name": "e"}}}),
                Auth::Key(&fx.admin),
            )
            .await
        };
        let unknown_list = fx
            .preview(
                &id,
                json!({"mapping": {"columns": [email_rule(),
                    {"column": "name", "target": {"type": "list",
                     "list_id": "00000000-0000-7000-8000-000000000000"}}]}}),
                Auth::Key(&fx.admin),
            )
            .await;
        let low = fx
            .preview(&id, json!({"limit": 0}), Auth::Key(&fx.admin))
            .await;
        let high = fx
            .preview(&id, json!({"limit": 21}), Auth::Key(&fx.admin))
            .await;
        let no_stream = fx
            .send(
                "POST",
                &format!("/sources/{id}/streams/nope/preview"),
                Some(json!({})),
                key,
            )
            .await;
        let no_source = fx
            .send(
                "POST",
                "/sources/00000000-0000-7000-8000-000000000000/streams/stream_1/preview",
                Some(json!({})),
                Auth::Key(&fx.admin),
            )
            .await;
        sqlx::query("UPDATE sources SET kind = 'gone'")
            .execute(&pool)
            .await
            .unwrap();
        fx.put_stream(&id, &["id"], Some(json!({"columns": [email_rule()]})))
            .await;
        let unavailable = fx.preview(&id, json!({}), Auth::Key(&fx.admin)).await;

        assert_eq!(unmapped.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(unmapped.body["detail"], "stream_not_mapped");
        assert_eq!(
            invalid_draft.body["detail"],
            "mapping: unknown_column (column \"mail\")"
        );
        assert_eq!(
            keyless_event.body["detail"],
            "mapping: event_requires_primary_key"
        );
        assert_eq!(
            unknown_list.body["detail"],
            "mapping: unknown_list (column \"name\")"
        );
        assert_eq!(low.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(high.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(no_stream.status, StatusCode::NOT_FOUND);
        assert_eq!(no_source.status, StatusCode::NOT_FOUND);
        assert_eq!(unavailable.status, StatusCode::CONFLICT);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_in_another_workspace_finds_no_source_and_reads_nothing(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.keyed_source().await;
        let boss = fx.admin_session().await;
        let foreign_ws = zirv_flow_store::create_workspace(
            &pool,
            &WorkspaceName::parse("theirs").unwrap(),
            &Slug::parse("theirs").unwrap(),
        )
        .await
        .unwrap()
        .id;

        let reply = call(
            &fx.router,
            "POST",
            &format!(
                "/api/v1/workspaces/{}/sources/{id}/streams/stream_1/preview",
                foreign_ws.as_uuid()
            ),
            Some(json!({})),
            Auth::Session(&boss),
        )
        .await;

        assert_eq!(reply.status, StatusCode::NOT_FOUND);
    }

    impl Fixture {
        async fn stream_freshness(&self, id: &str, who: Auth<'_>) -> Value {
            let reply = self
                .send("GET", &format!("/sources/{id}/streams"), None, who)
                .await;
            assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
            reply.body
        }

        /// Pretends the stream was enabled `seconds` ago.
        async fn enabled_ago(&self, seconds: i64) {
            sqlx::query(
                "UPDATE streams SET enabled_at = now() - make_interval(secs => $1) \
                 WHERE enabled",
            )
            .bind(seconds as f64)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        async fn source_state(&self, id: &str) -> Value {
            let one = self.as_admin("GET", &format!("/sources/{id}"), None).await;
            let listed = self.as_admin("GET", "/sources", None).await;
            let in_list = listed
                .body
                .as_array()
                .unwrap()
                .iter()
                .find(|source| source["id"] == id)
                .unwrap();
            assert_eq!(one.body["freshness"], in_list["freshness"]);
            one.body["freshness"].clone()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn freshness_follows_the_threshold_the_schedule_and_the_last_run(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.mapped_source(2).await;
        let stream = format!("/sources/{id}/streams/stream_1");
        let other = fx.create_ok("Idle", "ok", 1).await;

        // Just enabled, with no schedule and no threshold: fresh and never stale.
        let streams = fx.stream_freshness(&id, Auth::Key(&fx.read)).await;
        let freshness = &streams[0]["freshness"];
        assert_eq!(streams[0]["name"], "stream_1");
        assert_eq!(freshness["state"], "fresh");
        assert_eq!(freshness["last_success_at"], Value::Null);
        assert_eq!(freshness["threshold_seconds"], Value::Null);
        assert_eq!(freshness["threshold_is_default"], false);
        assert_eq!(streams[1]["freshness"], Value::Null, "stream_2 is disabled");
        assert_eq!(fx.source_state(&id).await, "fresh");
        assert_eq!(
            fx.source_state(&other).await,
            Value::Null,
            "no enabled stream"
        );

        // An explicit threshold passed with no run makes the stream stale, at once.
        fx.enabled_ago(3600).await;
        let put = fx
            .as_admin(
                "PUT",
                &stream,
                Some(json!({"enabled": true, "sync_mode": "full_refresh",
                    "staleness_threshold_seconds": 60})),
            )
            .await;
        assert_eq!(put.status, StatusCode::OK, "{:?}", put.body);
        assert_eq!(put.body["staleness_threshold_seconds"], 60);
        assert_eq!(put.body["freshness"]["state"], "stale");
        assert_eq!(put.body["freshness"]["threshold_seconds"], 60);
        assert_eq!(put.body["freshness"]["threshold_is_default"], false);
        assert_eq!(fx.source_state(&id).await, "stale");

        // Clearing it takes the threshold away again: nothing without a schedule is stale.
        let cleared = fx
            .as_admin(
                "PUT",
                &stream,
                Some(json!({"enabled": true, "sync_mode": "full_refresh"})),
            )
            .await;
        assert_eq!(cleared.body["freshness"]["state"], "fresh");
        assert_eq!(cleared.body["freshness"]["threshold_seconds"], Value::Null);

        // The schedule gives the default: twice the hourly interval.
        let scheduled = fx
            .as_admin(
                "PATCH",
                &format!("/sources/{id}"),
                Some(json!({"schedule": "0 * * * *"})),
            )
            .await;
        assert_eq!(scheduled.body["freshness"], "fresh");
        let within = fx.stream_freshness(&id, Auth::Key(&fx.read)).await;
        assert_eq!(within[0]["freshness"]["threshold_seconds"], 7200);
        assert_eq!(within[0]["freshness"]["threshold_is_default"], true);
        assert_eq!(within[0]["freshness"]["state"], "fresh");
        fx.enabled_ago(3 * 3600).await;
        let past = fx.stream_freshness(&id, Auth::Key(&fx.read)).await;
        assert_eq!(past[0]["freshness"]["state"], "stale");
        assert_eq!(fx.source_state(&id).await, "stale");

        // A failed last run is failing, and the source takes its worst stream.
        zirv_flow_store::record_stream_outcome(
            &mut pool.acquire().await.unwrap(),
            fx.ws,
            streams[0]["id"].as_str().unwrap().parse().unwrap(),
            zirv_flow_store::StreamOutcome::Failed,
            time::OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        let failing = fx.stream_freshness(&id, Auth::Key(&fx.read)).await;
        assert_eq!(failing[0]["freshness"]["state"], "failing");
        assert_eq!(fx.source_state(&id).await, "failing");

        // A success recovers it and shows when.
        zirv_flow_store::record_stream_outcome(
            &mut pool.acquire().await.unwrap(),
            fx.ws,
            streams[0]["id"].as_str().unwrap().parse().unwrap(),
            zirv_flow_store::StreamOutcome::Succeeded,
            time::OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        let recovered = fx.stream_freshness(&id, Auth::Key(&fx.read)).await;
        assert_eq!(recovered[0]["freshness"]["state"], "fresh");
        assert!(recovered[0]["freshness"]["last_success_at"].is_string());

        // A disabled source reports nothing.
        let off = fx
            .as_admin(
                "PATCH",
                &format!("/sources/{id}"),
                Some(json!({"enabled": false})),
            )
            .await;
        assert_eq!(off.body["freshness"], Value::Null);
        let disabled = fx.stream_freshness(&id, Auth::Key(&fx.read)).await;
        assert_eq!(disabled[0]["freshness"], Value::Null);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_an_admin_sees_the_high_water_mark(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.mapped_source(1).await;
        sqlx::query("UPDATE streams SET checkpoint = $1 WHERE enabled")
            .bind(json!({"after": "zz-cursor-marker@example.com"}))
            .execute(&pool)
            .await
            .unwrap();

        let admin = fx.stream_freshness(&id, Auth::Key(&fx.admin)).await;
        let member = fx.stream_freshness(&id, Auth::Session(&fx.member)).await;
        let reader = fx.stream_freshness(&id, Auth::Key(&fx.read)).await;

        assert_eq!(
            admin[0]["freshness"]["high_water_mark"],
            json!({"after": "zz-cursor-marker@example.com"})
        );
        assert_eq!(member[0]["freshness"]["high_water_mark"], Value::Null);
        assert_eq!(reader[0]["freshness"]["high_water_mark"], Value::Null);
        assert!(!member.to_string().contains("zz-cursor-marker"));
        assert!(!reader.to_string().contains("zz-cursor-marker"));
        // The freshness shape names no source, host or credential.
        let keys: std::collections::BTreeSet<_> = member[0]["freshness"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            keys,
            [
                "high_water_mark",
                "last_success_at",
                "state",
                "threshold_is_default",
                "threshold_seconds"
            ]
            .map(String::from)
            .into()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_an_admin_edits_the_threshold_and_the_change_is_audited_without_a_value(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.mapped_source(1).await;
        sqlx::query("UPDATE streams SET checkpoint = $1 WHERE enabled")
            .bind(json!({"after": "zz-cursor-marker@example.com"}))
            .execute(&pool)
            .await
            .unwrap();
        let stream = format!("/sources/{id}/streams/stream_1");
        let settings = |seconds: Value| {
            json!({"enabled": true, "sync_mode": "full_refresh",
                "staleness_threshold_seconds": seconds})
        };

        let member = fx
            .send(
                "PUT",
                &stream,
                Some(settings(json!(90))),
                Auth::Session(&fx.member),
            )
            .await;
        let reader = fx
            .send(
                "PUT",
                &stream,
                Some(settings(json!(90))),
                Auth::Key(&fx.read),
            )
            .await;
        let set = fx.as_admin("PUT", &stream, Some(settings(json!(90)))).await;
        let cleared = fx
            .as_admin("PUT", &stream, Some(settings(Value::Null)))
            .await;
        let refused = fx.as_admin("PUT", &stream, Some(settings(json!(0)))).await;

        assert_eq!(member.status, StatusCode::FORBIDDEN);
        assert_eq!(reader.status, StatusCode::FORBIDDEN);
        assert_eq!(set.status, StatusCode::OK, "{:?}", set.body);
        assert_eq!(set.body["staleness_threshold_seconds"], 90);
        assert_eq!(cleared.body["staleness_threshold_seconds"], Value::Null);
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        let entries: Vec<_> = zirv_flow_store::list_audit(&pool, 200)
            .await
            .unwrap()
            .into_iter()
            .filter(|entry| entry.action == "stream.configured")
            .collect();
        // The mapped_source fixture configured the stream once; then the two successful edits.
        let thresholds: Vec<_> = entries
            .iter()
            .rev()
            .map(|entry| entry.summary["staleness_threshold_seconds"].clone())
            .collect();
        assert_eq!(thresholds, [Value::Null, json!(90), Value::Null]);
        let summaries: String = entries
            .iter()
            .map(|entry| entry.summary.to_string())
            .collect();
        assert!(!summaries.contains("zz-cursor-marker"));
        // The threshold edit did not touch the stream's checkpoint.
        assert_eq!(
            fx.stream_freshness(&id, Auth::Key(&fx.admin)).await[0]["freshness"]["high_water_mark"],
            json!({"after": "zz-cursor-marker@example.com"})
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_kinds_list_the_database_sources_with_their_setting_kinds(pool: PgPool) {
        let fx = fixture(&pool).await;

        let kinds = call(
            &fx.router,
            "GET",
            "/api/v1/source-kinds",
            None,
            Auth::Key(&fx.read),
        )
        .await;

        assert_eq!(kinds.status, StatusCode::OK);
        for kind in ["postgres", "mysql"] {
            let entry = kinds
                .body
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["kind"] == kind)
                .unwrap_or_else(|| panic!("{kind} is listed"));
            let field = |key: &str| {
                entry["config_fields"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|field| field["key"] == key)
                    .unwrap_or_else(|| panic!("{kind} has the setting {key}"))
            };
            assert_eq!(field("host")["kind"], "text");
            assert_eq!(field("host")["required"], true);
            assert_eq!(field("port")["kind"], "number");
            assert_eq!(field("ca_certificate")["kind"], "long_text");
            assert_eq!(field("queries")["kind"], "queries");
            assert_eq!(field("tls")["kind"], "select");
            assert_eq!(field("tls")["options"], json!(["verify", "off"]));
            let credentials: Vec<_> = entry["credential_fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| (field["key"].clone(), field["secret"].clone()))
                .collect();
            assert_eq!(
                credentials,
                [
                    (json!("user"), json!(true)),
                    (json!("password"), json!(true))
                ]
            );
            assert_eq!(entry["capabilities"]["emits_deletes"], false);
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_database_source_validates_its_settings_and_round_trips_its_queries(pool: PgPool) {
        let fx = fixture(&pool).await;
        let create = |config: Value| {
            fx.as_admin(
                "POST",
                "/sources",
                Some(
                    json!({"kind": "postgres", "name": "warehouse", "config": config,
                    "credential": {"user": "reader", "password": "pw"}}),
                ),
            )
        };
        let fields = |reply: &Reply| -> Vec<String> {
            reply.body["errors"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| {
                    format!(
                        "{}: {}",
                        e["field"].as_str().unwrap(),
                        e["reason"].as_str().unwrap()
                    )
                })
                .collect()
        };

        let bad_host = create(json!({"host": "db/../x", "database": "shop"})).await;
        let bad_query = create(json!({"host": "db.example.com", "database": "shop",
            "queries": [{"name": "vip", "sql": "DROP TABLE secrets"}]}))
        .await;
        let bad_ca = create(json!({"host": "db.example.com", "database": "shop",
            "ca_certificate": "not a certificate"}))
        .await;

        assert_eq!(
            bad_host.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            bad_host.body
        );
        assert_eq!(fields(&bad_host).len(), 1);
        assert!(
            fields(&bad_host)[0].starts_with("config.host: "),
            "{:?}",
            fields(&bad_host)
        );
        assert!(
            fields(&bad_query)[0].starts_with("config.queries: "),
            "{:?}",
            fields(&bad_query)
        );
        assert!(!fields(&bad_query)[0].contains("secrets"));
        assert!(
            fields(&bad_ca)[0].starts_with("config.ca_certificate: "),
            "{:?}",
            fields(&bad_ca)
        );
        let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM sources")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, 0, "a refused source stores nothing");

        let queries = json!([{"name": "vip", "sql": "SELECT id, email FROM customers WHERE vip"}]);
        let created = create(json!({"host": "db.example.com", "database": "shop",
            "queries": queries}))
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let id = created.body["id"].as_str().unwrap().to_owned();
        assert_eq!(created.body["config"]["queries"], queries);
        assert_eq!(
            created.body["credential_fields_set"],
            json!(["password", "user"])
        );

        let more = json!([
            {"name": "vip", "sql": "SELECT id, email FROM customers WHERE vip"},
            {"name": "recent", "sql": "SELECT id FROM orders"}
        ]);
        let patched = fx
            .as_admin(
                "PATCH",
                &format!("/sources/{id}"),
                Some(
                    json!({"config": {"host": "db.example.com", "database": "shop",
                    "queries": more}}),
                ),
            )
            .await;
        assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);
        let read = fx.as_admin("GET", &format!("/sources/{id}"), None).await;
        assert_eq!(read.body["config"]["queries"], more);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_postgres_table_stream_previews_through_the_mapping_editor_without_writing(
        pool: PgPool,
    ) {
        // The test database is the upstream: a table, and a role that can only read it.
        let role = format!("zf_prev_{}", uuid::Uuid::now_v7().simple());
        for statement in [
            "CREATE TABLE items (id bigint PRIMARY KEY, email text NOT NULL, name text, \
             updated_at timestamptz)"
                .to_owned(),
            "INSERT INTO items SELECT n, 'user' || n || '@example.com', 'User ' || n, \
             timestamptz '2024-01-01 00:00:00+00' + n * interval '1 minute' \
             FROM generate_series(1, 30) AS n"
                .to_owned(),
            format!("CREATE ROLE {role} LOGIN PASSWORD 'zf_prev_pw'"),
            format!("GRANT SELECT ON items TO {role}"),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(statement))
                .execute(&pool)
                .await
                .unwrap();
        }
        let policy = zirv_flow_core::egress::EgressPolicy::parse(&[
            "127.0.0.0/8".to_owned(),
            "::1/128".to_owned(),
        ])
        .unwrap();
        let fx = fixture_with_egress(
            &pool,
            Some(Arc::new(Keyring::new(MasterKey::generate(), None).unwrap())),
            Some(policy),
        )
        .await;
        zirv_flow_store::create_trait_definition(&pool, fx.ws, &trait_definition("plan", false))
            .await
            .unwrap();
        let options = pool.connect_options();
        let created = fx
            .as_admin(
                "POST",
                "/sources",
                Some(json!({
                    "kind": "postgres",
                    "name": "warehouse",
                    "config": {
                        "host": options.get_host(),
                        "port": options.get_port(),
                        "database": options.get_database().unwrap(),
                        "tls": "off"
                    },
                    "credential": {"user": role, "password": "zf_prev_pw"}
                })),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let id = created.body["id"].as_str().unwrap().to_owned();
        let discovered = fx
            .as_admin("POST", &format!("/sources/{id}/discover"), None)
            .await;
        assert_eq!(discovered.status, StatusCode::OK, "{:?}", discovered.body);
        let mapping = json!({
            "columns": [
                email_rule(),
                {"column": "name", "target": {"type": "trait", "key": "plan"}}
            ],
            "event": {"name": "seen", "timestamp": {"column": "updated_at"},
                      "properties": [{"property": "who", "column": "name"}]}
        });
        let saved = fx
            .as_admin(
                "PUT",
                &format!("/sources/{id}/streams/items"),
                Some(json!({"enabled": true, "sync_mode": "full_refresh",
                            "primary_key": ["id"], "mapping": mapping})),
            )
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        let before: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM people) + (SELECT count(*) FROM events) \
             + (SELECT count(*) FROM sync_runs) + (SELECT count(*) FROM jobs)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let saved_preview = fx
            .as_admin(
                "POST",
                &format!("/sources/{id}/streams/items/preview"),
                Some(json!({"limit": 3})),
            )
            .await;
        let draft = fx
            .as_admin(
                "POST",
                &format!("/sources/{id}/streams/items/preview"),
                Some(json!({"limit": 20, "mapping": {"columns": [email_rule()]}})),
            )
            .await;
        // A role belongs to the cluster, not the test database: drop it before any assertion.
        for statement in [format!("DROP OWNED BY {role}"), format!("DROP ROLE {role}")] {
            sqlx::query(sqlx::AssertSqlSafe(statement))
                .execute(&pool)
                .await
                .unwrap();
        }

        assert_eq!(
            saved_preview.status,
            StatusCode::OK,
            "{:?}",
            saved_preview.body
        );
        assert_eq!(saved_preview.body["read"], 3);
        assert_eq!(saved_preview.body["truncated"], true);
        let first = &saved_preview.body["records"][0];
        assert_eq!(first["result"], "mapped");
        assert_eq!(first["key"], "1");
        assert_eq!(first["identifiers"][0]["value"], "user1@example.com");
        assert_eq!(first["traits"]["plan"], "User 1");
        assert_eq!(first["event"]["properties"]["who"], "User 1");
        // PostgreSQL hands the bare timestamptz over as text ("2024-01-01 00:01:00+00"); the mapping
        // reads it as the event time with no transform.
        assert_eq!(first["event"]["occurred_at"], "2024-01-01T00:01:00Z");
        assert_eq!(draft.status, StatusCode::OK, "{:?}", draft.body);
        assert_eq!(draft.body["read"], 20);
        assert_eq!(draft.body["truncated"], true, "30 rows, 20 shown");
        assert_eq!(draft.body["records"][19]["key"], "20");
        let after: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM people) + (SELECT count(*) FROM events) \
             + (SELECT count(*) FROM sync_runs) + (SELECT count(*) FROM jobs)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(after, before, "a preview writes nothing");
    }
}
