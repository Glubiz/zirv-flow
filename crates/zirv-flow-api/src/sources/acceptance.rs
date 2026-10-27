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
    use crate::test_support::{tables_hold_no, the_scan_finds_a_planted};
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

    #[derive(Clone, Copy)]
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

    /// Sends a request. A body that still carries an inline credential for a source or
    /// destination is expanded to a workspace credential first (`expand_inline_credential`).
    async fn call(
        router: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
        auth: Auth<'_>,
    ) -> Reply {
        let sent = crate::test_support::expand_inline_credential(
            |method, uri, body| async move {
                let reply = call_once(router, &method, &uri, body, auth).await;
                (reply.status, reply.body)
            },
            method,
            uri,
            body,
        )
        .await;
        match sent {
            Ok(body) => call_once(router, method, uri, body, auth).await,
            Err((status, body)) => Reply {
                status,
                headers: Default::default(),
                body,
            },
        }
    }

    async fn call_once(
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

    /// A fixture with a keyring whose egress policy allows only loopback addresses.
    async fn fixture_with_loopback_egress(pool: &PgPool) -> Fixture {
        let policy = zirv_flow_core::egress::EgressPolicy::parse(&[
            "127.0.0.0/8".to_owned(),
            "::1/128".to_owned(),
        ])
        .unwrap();
        fixture_with_egress(
            pool,
            Some(Arc::new(Keyring::new(MasterKey::generate(), None).unwrap())),
            Some(policy),
        )
        .await
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
        admin::set_member(pool, &Context::system(), ws, user.id, Role::Editor)
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
        // The workspace credential the source used is its own object and stays, with its secret.
        for (table, expected) in [("sources", 0), ("streams", 0), ("encrypted_secrets", 1)] {
            let count: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(count, expected, "{table}");
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
        // Each create made its credential first; the refused one left its credential, not a source.
        assert_eq!(kept, 3, "a refused create stores no source");
        let sources: i64 = sqlx::query_scalar("SELECT count(*) FROM sources")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sources, 2);
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
            "credential_id": uuid::Uuid::now_v7()}))
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
        assert_eq!(fields(&wrong_credential), ["credential_id"]);
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
        // The credential of the first source and one made for each of the three refused bodies
        // that named a valid credential: a credential is its own object, not part of a source.
        assert_eq!(
            secrets_stored, 4,
            "a refused create leaves no source behind"
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
                "secret.created",
                "secret.deleted",
                "source.updated",
                "source.tested",
                "source.discovered",
                "stream.configured",
                "source.deleted",
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
        the_scan_finds_a_planted(&pool, &[MARKER]).await;
    }

    const PG_PASSWORD: &str = "zz-pg-password-5521";
    const PG_REPLACED_PASSWORD: &str = "zz-pg-replaced-6632";
    const MYSQL_PASSWORD: &str = "zz-mysql-password-7743";
    const MYSQL_REPLACED_PASSWORD: &str = "zz-mysql-replaced-8854";
    const SASL_PASSWORD: &str = "zz-sasl-password-9965";
    const SASL_REPLACED_PASSWORD: &str = "zz-sasl-replaced-1076";

    impl Fixture {
        /// Every read, check and write a source offers, with its credential stored.
        async fn exercise_a_stored_credential(&self, id: &str, replacement: Value) {
            let one = format!("/sources/{id}");
            self.as_admin("GET", &one, None).await;
            self.as_admin("GET", "/sources", None).await;
            self.send("GET", "/sources", None, Auth::Session(&self.member))
                .await;
            self.send("GET", &one, None, Auth::Key(&self.read)).await;
            self.as_admin("POST", &format!("{one}/test"), None).await;
            self.as_admin("POST", &format!("{one}/discover"), None)
                .await;
            self.as_admin("GET", &format!("{one}/streams"), None).await;
            let runs = self.as_admin("GET", &format!("{one}/runs"), None).await;
            assert_eq!(runs.status, StatusCode::OK, "{:?}", runs.body);
            let patched = self
                .as_admin("PATCH", &one, Some(json!({"credential": replacement})))
                .await;
            assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);
            let renamed = self
                .as_admin("PATCH", &one, Some(json!({"name": "Renamed"})))
                .await;
            assert_eq!(renamed.status, StatusCode::OK, "{:?}", renamed.body);
            self.as_admin("GET", &one, None).await;
        }

        /// No response seen, audit entry or table row holds any of the secrets, and the table scan
        /// is able to see them.
        async fn assert_secrets_hidden(&self, secrets: &[&str]) {
            let seen = self.seen.lock().unwrap().join("\n");
            let audit = format!(
                "{:?}",
                zirv_flow_store::list_audit(&self.pool, 500).await.unwrap()
            );
            for secret in secrets {
                assert!(!seen.contains(secret), "a response held {secret}");
                assert!(!audit.contains(secret), "the audit held {secret}");
            }
            tables_hold_no(&self.pool, secrets).await;
            the_scan_finds_a_planted(&self.pool, secrets).await;
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_postgres_source_password_is_ciphertext_at_rest_and_in_no_response_or_audit_entry(
        pool: PgPool,
    ) {
        let fx = fixture_with_loopback_egress(&pool).await;
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
                    "credential": {"username": "zz_pg_reader", "password": PG_PASSWORD}
                })),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let id = created.body["id"].as_str().unwrap().to_owned();

        fx.exercise_a_stored_credential(
            &id,
            json!({"username": "zz_pg_reader", "password": PG_REPLACED_PASSWORD}),
        )
        .await;

        fx.assert_secrets_hidden(&[PG_PASSWORD, PG_REPLACED_PASSWORD])
            .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_mysql_source_password_is_ciphertext_at_rest_and_in_no_response_or_audit_entry(
        pool: PgPool,
    ) {
        let fx = fixture_with_loopback_egress(&pool).await;
        let (host, port) = mysql_address();
        let created = fx
            .as_admin(
                "POST",
                "/sources",
                Some(json!({
                    "kind": "mysql",
                    "name": "shop",
                    "config": {"host": host, "port": port, "database": "shop", "tls": "off"},
                    "credential": {"username": "zz_mysql_reader", "password": MYSQL_PASSWORD}
                })),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let id = created.body["id"].as_str().unwrap().to_owned();

        fx.exercise_a_stored_credential(
            &id,
            json!({"username": "zz_mysql_reader", "password": MYSQL_REPLACED_PASSWORD}),
        )
        .await;

        fx.assert_secrets_hidden(&[MYSQL_PASSWORD, MYSQL_REPLACED_PASSWORD])
            .await;
    }

    /// The host and port of the test MySQL server, from `ZIRV_FLOW_TEST_MYSQL_URL`.
    fn mysql_address() -> (String, u16) {
        let url = std::env::var("ZIRV_FLOW_TEST_MYSQL_URL").expect("set ZIRV_FLOW_TEST_MYSQL_URL");
        let authority = url.split("://").nth(1).unwrap().split('/').next().unwrap();
        let address = authority.rsplit('@').next().unwrap();
        let (host, port) = address.rsplit_once(':').unwrap();
        (host.to_owned(), port.parse().unwrap())
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
    async fn a_preview_shows_a_record_the_delete_signal_removes_as_deleted_even_without_an_identifier(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.keyed_source().await;
        // Record 1 is soft-deleted and has no valid identifier; record 0 is live.
        sqlx::query("UPDATE sources SET config = config || '{\"records\": 2, \"soft_deleted\": \"1\", \"invalid\": \"1\"}'::jsonb")
            .execute(&pool)
            .await
            .unwrap();
        let mapping = json!({
            "columns": [email_rule()],
            "delete_when": {"column": "deleted_flag", "rule": "truthy"}
        });
        let saved = fx.put_stream(&id, &["id"], Some(mapping)).await;
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);

        let shown = fx.preview(&id, json!({}), Auth::Key(&fx.admin)).await;

        assert_eq!(shown.status, StatusCode::OK, "{:?}", shown.body);
        assert_eq!(shown.body["records"][0]["key"], "stream_1-0");
        assert_eq!(shown.body["records"][0]["result"], "mapped");
        assert_eq!(shown.body["records"][1]["key"], "stream_1-1");
        assert_eq!(shown.body["records"][1]["result"], "deleted");
        assert_eq!(shown.body["records"][1]["deleted"], true);
        assert_eq!(shown.body["records"][1]["errors"], json!([]));
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
                "lag",
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
                    (json!("username"), json!(true)),
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
                    "credential": {"username": "reader", "password": "pw"}}),
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
            json!(["password", "username"])
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
        let fx = fixture_with_loopback_egress(&pool).await;
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
                    "credential": {"username": role, "password": "zf_prev_pw"}
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

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_stream_stores_and_returns_its_delete_signal_reconcile_schedule_and_sweep_limit(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.mapped_source(1).await;
        let tail = format!("/sources/{id}/streams/stream_1");
        let mut mapping = mapping_body();
        mapping["delete_when"] = json!({"column": "updated_at", "rule": "present"});
        let settings = json!({
            "enabled": true, "sync_mode": "incremental", "cursor_field": "updated_at",
            "reconcile_schedule": "0 3 * * *", "sweep_limit": 0.25, "mapping": mapping
        });

        let saved = fx.as_admin("PUT", &tail, Some(settings)).await;

        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert_eq!(saved.body["reconcile_schedule"], "0 3 * * *");
        assert_eq!(saved.body["sweep_limit"], 0.25);
        assert_eq!(
            saved.body["mapping"]["delete_when"],
            json!({"column": "updated_at", "rule": "present"})
        );
        // The cron is mirrored to the row the leader fires, keyed by the stream.
        let rows: Vec<(String, serde_json::Value, bool)> = sqlx::query_as(
            "SELECT cron, payload, enabled FROM schedules WHERE stream_id IS NOT NULL",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "0 3 * * *");
        assert_eq!(rows[0].1["reconcile"], true);
        assert!(rows[0].2);
        // The audit entry says that they were set and holds no column or cron text.
        let entry = zirv_flow_store::list_audit(&pool, 5)
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.action == "stream.configured")
            .unwrap();
        assert_eq!(entry.summary["delete_signal_set"], true);
        assert_eq!(entry.summary["reconcile_schedule_set"], true);
        assert_eq!(entry.summary["sweep_limit"], 0.25);
        assert!(!entry.summary.to_string().contains("updated_at"));
        assert!(!entry.summary.to_string().contains("0 3"));

        // Absent settings clear the cron and reset the limit; the mapping stays.
        let cleared = fx
            .as_admin(
                "PUT",
                &tail,
                Some(json!({"enabled": true, "sync_mode": "incremental",
                    "cursor_field": "updated_at"})),
            )
            .await;
        assert_eq!(cleared.body["reconcile_schedule"], Value::Null);
        assert_eq!(cleared.body["sweep_limit"], 0.5);
        assert_eq!(cleared.body["mapping"], saved.body["mapping"]);
        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM schedules WHERE stream_id IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(left, 0);
        let member = fx
            .send(
                "PUT",
                &tail,
                Some(json!({"enabled": true, "sync_mode": "full_refresh", "sweep_limit": 1})),
                Auth::Session(&fx.member),
            )
            .await;
        assert_eq!(member.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn enabling_or_disabling_a_source_re_mirrors_its_streams_reconcile_schedules(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.mapped_source(1).await;
        let source = format!("/sources/{id}");
        let tail = format!("{source}/streams/stream_1");
        let enabled_of = || async {
            sqlx::query_scalar::<_, bool>(
                "SELECT enabled FROM schedules WHERE stream_id IS NOT NULL",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        };

        // The cron is set while the source is disabled, so the row is stored disabled.
        let off = fx
            .as_admin("PATCH", &source, Some(json!({"enabled": false})))
            .await;
        assert_eq!(off.status, StatusCode::OK, "{:?}", off.body);
        let saved = fx
            .as_admin(
                "PUT",
                &tail,
                Some(json!({"enabled": true, "sync_mode": "incremental",
                    "cursor_field": "updated_at", "reconcile_schedule": "0 3 * * *"})),
            )
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert!(!enabled_of().await);

        let on = fx
            .as_admin("PATCH", &source, Some(json!({"enabled": true})))
            .await;
        assert_eq!(on.status, StatusCode::OK, "{:?}", on.body);
        assert!(enabled_of().await, "enabling the source enables the row");

        fx.as_admin("PATCH", &source, Some(json!({"enabled": false})))
            .await;
        assert!(
            !enabled_of().await,
            "disabling the source disables it again"
        );

        // A disabled stream stays disabled whatever the source does.
        fx.as_admin("PATCH", &source, Some(json!({"enabled": true})))
            .await;
        sqlx::query("UPDATE streams SET enabled = false, enabled_at = NULL")
            .execute(&pool)
            .await
            .unwrap();
        fx.as_admin("PATCH", &source, Some(json!({"enabled": false})))
            .await;
        fx.as_admin("PATCH", &source, Some(json!({"enabled": true})))
            .await;
        assert!(!enabled_of().await);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn bad_deletion_settings_are_refused_and_store_nothing(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.mapped_source(1).await;
        let tail = format!("/sources/{id}/streams/stream_1");
        let incremental = |extra: Value| {
            let mut body = json!({"enabled": true, "sync_mode": "incremental",
                "cursor_field": "updated_at"});
            for (key, value) in extra.as_object().unwrap() {
                body[key] = value.clone();
            }
            body
        };

        let bad_cron = fx
            .as_admin(
                "PUT",
                &tail,
                Some(incremental(json!({"reconcile_schedule": "nope"}))),
            )
            .await;
        let full = fx
            .as_admin(
                "PUT",
                &tail,
                Some(json!({"enabled": true, "sync_mode": "full_refresh",
                    "reconcile_schedule": "0 3 * * *"})),
            )
            .await;
        let ghost = fx
            .as_admin(
                "PUT",
                &tail,
                Some(incremental(json!({"mapping": {
                    "columns": [{"column": "email", "target": {"type": "identifier", "kind": "email"}}],
                    "delete_when": {"column": "ghost"}}}))),
            )
            .await;
        let mut limits = Vec::new();
        for limit in [json!(0), json!(-0.5), json!(1.5)] {
            limits.push(
                fx.as_admin(
                    "PUT",
                    &tail,
                    Some(incremental(json!({"sweep_limit": limit}))),
                )
                .await,
            );
        }

        assert_eq!(bad_cron.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            bad_cron.body["detail"]
                .as_str()
                .unwrap()
                .contains("reconcile_schedule")
        );
        assert_eq!(full.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(full.body["code"], "reconcile_needs_incremental");
        assert_eq!(ghost.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            ghost.body["detail"],
            "mapping: unknown_column (column \"ghost\")"
        );
        for limit in &limits {
            assert_eq!(limit.status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(limit.body["code"], "invalid_sweep_limit");
        }
        // Nothing changed: still the full refresh set up by the fixture, with the defaults.
        let streams = fx
            .as_admin("GET", &format!("/sources/{id}/streams"), None)
            .await;
        assert_eq!(streams.body[0]["sync_mode"], "full_refresh");
        assert_eq!(streams.body[0]["sweep_limit"], 0.5);
        assert_eq!(streams.body[0]["reconcile_schedule"], Value::Null);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_delete_signal_may_be_a_path_into_a_declared_field_and_an_undeclared_one_is_refused(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.mapped_source(1).await;
        let tail = format!("/sources/{id}/streams/stream_1");
        let settings = |column: &str| {
            let mut mapping = mapping_body();
            mapping["delete_when"] = json!({"column": column, "rule": "truthy"});
            json!({"enabled": true, "sync_mode": "full_refresh", "mapping": mapping})
        };

        let ghost = fx
            .as_admin("PUT", &tail, Some(settings("ghost.deleted")))
            .await;
        let nested = fx
            .as_admin("PUT", &tail, Some(settings("email.deleted")))
            .await;

        assert_eq!(ghost.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            ghost.body["detail"],
            "mapping: unknown_column (column \"ghost.deleted\")"
        );
        assert_eq!(nested.status, StatusCode::OK, "{:?}", nested.body);
        assert_eq!(
            nested.body["mapping"]["delete_when"],
            json!({"column": "email.deleted", "rule": "truthy"})
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reconciliation_is_started_by_admins_on_an_incremental_stream_and_shows_its_kind(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.mapped_source(1).await;
        let runs = format!("/sources/{id}/runs");
        let reconcile = json!({"stream": "stream_1", "kind": "reconcile"});

        // stream_1 is a full refresh: nothing to reconcile.
        let full = fx.as_admin("POST", &runs, Some(reconcile.clone())).await;
        let no_stream = fx
            .as_admin("POST", &runs, Some(json!({"kind": "reconcile"})))
            .await;
        fx.as_admin(
            "PUT",
            &format!("/sources/{id}/streams/stream_1"),
            Some(json!({"enabled": true, "sync_mode": "incremental",
                "cursor_field": "updated_at"})),
        )
        .await;
        let member = fx
            .send(
                "POST",
                &runs,
                Some(reconcile.clone()),
                Auth::Session(&fx.member),
            )
            .await;
        let started = fx.as_admin("POST", &runs, Some(reconcile.clone())).await;
        let again = fx.as_admin("POST", &runs, Some(reconcile)).await;

        assert_eq!(full.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(full.body["code"], "reconcile_needs_incremental");
        assert_eq!(no_stream.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(member.status, StatusCode::FORBIDDEN);
        assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
        assert_eq!(started.body[0]["kind"], "reconcile");
        assert_eq!(again.status, StatusCode::CONFLICT);
        assert_eq!(again.body["code"], "sync_run_active");
        let run_id = started.body[0]["id"].as_str().unwrap();
        let read = fx
            .send(
                "GET",
                &format!("{runs}/{run_id}"),
                None,
                Auth::Session(&fx.member),
            )
            .await;
        assert_eq!(read.body["kind"], "reconcile");
        let listed = fx.send("GET", &runs, None, Auth::Session(&fx.member)).await;
        assert_eq!(listed.body["runs"][0]["kind"], "reconcile");
        let entry = zirv_flow_store::list_audit(&pool, 20)
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.action == "sync_run.started")
            .unwrap();
        assert_eq!(entry.summary["kind"], "reconcile");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_normal_run_reports_the_kind_sync(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.mapped_source(1).await;

        let started = fx
            .as_admin(
                "POST",
                &format!("/sources/{id}/runs"),
                Some(json!({"stream": "stream_1"})),
            )
            .await;

        assert_eq!(started.body[0]["kind"], "sync");
    }

    // The Kafka source: stream-mode streams, their consumer runs and their lag.

    impl Fixture {
        /// A Kafka source (nothing connects at creation) whose one stream `orders` is
        /// discovered in stream mode with the `plan` trait registered.
        async fn kafka_source(&self) -> String {
            let broker =
                std::env::var("ZIRV_KAFKA_BROKER").unwrap_or_else(|_| "localhost:9092".to_owned());
            self.kafka_source_with(json!({"brokers": broker, "security": "plaintext"}), None)
                .await
        }

        /// `kafka_source` with its own connection settings and an optional credential.
        async fn kafka_source_with(&self, connection: Value, credential: Option<Value>) -> String {
            zirv_flow_store::create_trait_definition(
                &self.pool,
                self.ws,
                &trait_definition("plan", false),
            )
            .await
            .unwrap();
            let mut config = json!({"topics": "orders"});
            config
                .as_object_mut()
                .unwrap()
                .extend(connection.as_object().unwrap().clone());
            let mut body = json!({"kind": "kafka", "name": "Events", "config": config});
            if let Some(credential) = credential {
                body["credential"] = credential;
            }
            let created = self.as_admin("POST", "/sources", Some(body)).await;
            assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
            let id = created.body["id"].as_str().unwrap().to_owned();
            let descriptor = json!({
                "name": "orders",
                "fields": [
                    {"name": "_key", "field_type": "string"},
                    {"name": "_timestamp", "field_type": "timestamp"},
                    {"name": "email", "field_type": "string"},
                    {"name": "plan", "field_type": "string"},
                ],
                "sync_modes": ["stream"],
                "default_cursor_field": null,
                "default_primary_key": ["_key"],
            });
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            zirv_flow_store::upsert_discovered_streams(
                &mut tx,
                self.ws,
                id.parse().unwrap(),
                &[zirv_flow_store::DiscoveredStream {
                    name: "orders",
                    sync_mode: "stream",
                    cursor_field: None,
                    primary_key: &["_key".to_owned()],
                    descriptor: &descriptor,
                    suggested_mapping: None,
                }],
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            id
        }

        fn consumer_settings(enabled: bool) -> Value {
            json!({
                "enabled": enabled, "sync_mode": "stream", "primary_key": ["_key"],
                "mapping": {"columns": [
                    {"column": "email", "target": {"type": "identifier", "kind": "email"}},
                    {"column": "plan", "target": {"type": "trait", "key": "plan"}}
                ]}
            })
        }

        async fn consumer_runs(&self) -> Vec<(String, String)> {
            sqlx::query_as("SELECT state, sync_mode FROM sync_runs ORDER BY created_at")
                .fetch_all(&self.pool)
                .await
                .unwrap()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL and Kafka; set DATABASE_URL, eval scripts/kafka-fixture.sh"]
    async fn kafka_sasl_credentials_are_ciphertext_at_rest_and_in_no_response_or_audit_entry(
        pool: PgPool,
    ) {
        let fx = fixture_with_loopback_egress(&pool).await;
        let broker = std::env::var("ZIRV_KAFKA_SASL_BROKER").expect("run kafka-fixture.sh");
        let id = fx
            .kafka_source_with(
                json!({"brokers": broker, "security": "plaintext", "sasl": "plain"}),
                Some(json!({"username": "zz-sasl-user", "password": SASL_PASSWORD})),
            )
            .await;

        let test = fx
            .as_admin("POST", &format!("/sources/{id}/test"), None)
            .await;
        assert_eq!(test.status, StatusCode::OK, "{:?}", test.body);
        assert_eq!(test.body["ok"], false, "the broker refuses the password");
        fx.as_admin(
            "PUT",
            &format!("/sources/{id}/streams/orders"),
            Some(Fixture::consumer_settings(true)),
        )
        .await;
        fx.exercise_a_stored_credential(
            &id,
            json!({"username": "zz-sasl-user", "password": SASL_REPLACED_PASSWORD}),
        )
        .await;

        fx.assert_secrets_hidden(&[SASL_PASSWORD, SASL_REPLACED_PASSWORD])
            .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_kafka_stream_is_configured_in_stream_mode_and_enabling_it_starts_one_consumer(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.kafka_source().await;
        let tail = format!("/sources/{id}/streams/orders");

        let saved = fx
            .as_admin("PUT", &tail, Some(Fixture::consumer_settings(true)))
            .await;

        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert_eq!(saved.body["sync_mode"], "stream");
        assert_eq!(saved.body["discovered"]["sync_modes"], json!(["stream"]));
        assert_eq!(
            fx.consumer_runs().await,
            [("queued".to_owned(), "stream".to_owned())]
        );
        let job: (String, Value) = sqlx::query_as("SELECT kind, payload FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(job.0, "kafka.consume");
        assert_eq!(job.1["generation"], 0);
        let entry = zirv_flow_store::list_audit(&pool, 5)
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.action == "stream.configured")
            .unwrap();
        assert_eq!(entry.summary["run_started"], true);

        // Saving again while the run is active starts nothing more.
        let again = fx
            .as_admin("PUT", &tail, Some(Fixture::consumer_settings(true)))
            .await;
        assert_eq!(again.status, StatusCode::OK);
        assert_eq!(fx.consumer_runs().await.len(), 1);
        let entry = zirv_flow_store::list_audit(&pool, 5)
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.action == "stream.configured")
            .unwrap();
        assert_eq!(entry.summary["run_started"], false);

        // Disabling the stream cancels the consumer.
        let off = fx
            .as_admin("PUT", &tail, Some(Fixture::consumer_settings(false)))
            .await;
        assert_eq!(off.status, StatusCode::OK);
        assert_eq!(
            fx.consumer_runs().await,
            [("cancelled".to_owned(), "stream".to_owned())]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn disabling_the_source_cancels_its_consumers_and_run_now_refuses_a_second(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.kafka_source().await;
        let tail = format!("/sources/{id}/streams/orders");
        fx.as_admin("PUT", &tail, Some(Fixture::consumer_settings(true)))
            .await;

        let second = fx
            .as_admin(
                "POST",
                &format!("/sources/{id}/runs"),
                Some(json!({"stream": "orders"})),
            )
            .await;
        assert_eq!(second.status, StatusCode::CONFLICT, "{:?}", second.body);
        let reconcile = fx
            .as_admin(
                "POST",
                &format!("/sources/{id}/runs"),
                Some(json!({"stream": "orders", "kind": "reconcile"})),
            )
            .await;
        assert_eq!(reconcile.status, StatusCode::UNPROCESSABLE_ENTITY);
        let off = fx
            .as_admin(
                "PATCH",
                &format!("/sources/{id}"),
                Some(json!({"enabled": false})),
            )
            .await;
        assert_eq!(off.status, StatusCode::OK, "{:?}", off.body);

        assert_eq!(
            fx.consumer_runs().await,
            [("cancelled".to_owned(), "stream".to_owned())]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_stream_mode_stream_refuses_reconcile_and_sweep_settings_and_other_modes(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.kafka_source().await;
        let tail = format!("/sources/{id}/streams/orders");
        let with = |extra: Value| {
            let mut body = Fixture::consumer_settings(false);
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            body
        };

        let reconcile = fx
            .as_admin(
                "PUT",
                &tail,
                Some(with(json!({"reconcile_schedule": "0 3 * * *"}))),
            )
            .await;
        let sweep = fx
            .as_admin("PUT", &tail, Some(with(json!({"sweep_limit": 0.25}))))
            .await;
        let incremental = fx
            .as_admin(
                "PUT",
                &tail,
                Some(with(
                    json!({"sync_mode": "incremental", "cursor_field": "_timestamp"}),
                )),
            )
            .await;
        let full = fx
            .as_admin(
                "PUT",
                &tail,
                Some(with(json!({"sync_mode": "full_refresh"}))),
            )
            .await;

        for refused in [&reconcile, &sweep, &incremental, &full] {
            assert_eq!(
                refused.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{:?}",
                refused.body
            );
        }
        assert_eq!(fx.consumer_runs().await, []);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn lag_is_shown_to_members_with_the_freshness_and_only_for_a_stream_mode_stream(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.kafka_source().await;
        let tail = format!("/sources/{id}/streams/orders");
        fx.as_admin("PUT", &tail, Some(Fixture::consumer_settings(true)))
            .await;
        let before = fx.stream_freshness(&id, Auth::Session(&fx.member)).await;
        assert_eq!(before[0]["freshness"]["lag"], Value::Null, "no fetch yet");

        sqlx::query(
            "INSERT INTO stream_offsets \
                (workspace_id, stream_id, partition, next_offset, high_watermark, observed_at) \
             SELECT workspace_id, id, p, n, w, now() FROM streams, \
                (VALUES (0, 10::bigint, 14::bigint), (1, 5, 5), (2, 0, 1200)) AS t(p, n, w)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let shown = fx.stream_freshness(&id, Auth::Session(&fx.member)).await;

        let lag = &shown[0]["freshness"]["lag"];
        assert_eq!(lag["total"], 1204);
        assert_eq!(
            lag["partitions"],
            json!([
                {"partition": 0, "lag": 4},
                {"partition": 1, "lag": 0},
                {"partition": 2, "lag": 1200}
            ])
        );
        // A pull stream of another source has no lag.
        let fake = fx.create_ok("CRM", "ok", 1).await;
        fx.as_admin("POST", &format!("/sources/{fake}/discover"), None)
            .await;
        fx.as_admin(
            "PUT",
            &format!("/sources/{fake}/streams/stream_1"),
            Some(json!({"enabled": false, "sync_mode": "full_refresh"})),
        )
        .await;
        let pulled = fx.stream_freshness(&fake, Auth::Session(&fx.member)).await;
        assert_eq!(pulled[0]["freshness"]["lag"], Value::Null);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL and a Kafka broker; set DATABASE_URL and run kafka-fixture.sh"]
    async fn a_preview_of_a_topic_with_fewer_messages_than_the_limit_returns_without_timing_out(
        pool: PgPool,
    ) {
        let broker = std::env::var("ZIRV_KAFKA_BROKER").expect("run kafka-fixture.sh");
        let topic = format!(
            "api-preview-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let client = rskafka::client::ClientBuilder::new(vec![broker.clone()])
            .build()
            .await
            .unwrap();
        client
            .controller_client()
            .unwrap()
            .create_topic(&topic, 1, 1, 10_000)
            .await
            .unwrap();
        let partition = client
            .partition_client(
                &topic,
                0,
                rskafka::client::partition::UnknownTopicHandling::Retry,
            )
            .await
            .unwrap();
        let message = |key: &str, value: &str| rskafka::record::Record {
            key: Some(key.as_bytes().to_vec()),
            value: Some(value.as_bytes().to_vec()),
            headers: Default::default(),
            timestamp: rskafka::chrono::Utc::now(),
        };
        partition
            .produce(
                vec![
                    message("k1", r#"{"email":"a@example.com","plan":"pro"}"#),
                    message("k2", r#"{"email":"b@example.com","plan":"free"}"#),
                ],
                rskafka::client::partition::Compression::NoCompression,
            )
            .await
            .unwrap();
        let fx = fixture_with_egress(
            &pool,
            None,
            Some(
                zirv_flow_core::egress::EgressPolicy::parse(&[
                    "127.0.0.0/8".to_owned(),
                    "::1/128".to_owned(),
                ])
                .unwrap(),
            ),
        )
        .await;
        zirv_flow_store::create_trait_definition(&fx.pool, fx.ws, &trait_definition("plan", false))
            .await
            .unwrap();
        let created = fx
            .as_admin(
                "POST",
                "/sources",
                Some(json!({
                    "kind": "kafka", "name": "Events",
                    "config": {"brokers": broker, "topics": topic, "security": "plaintext"},
                })),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let id = created.body["id"].as_str().unwrap().to_owned();
        let discovered = fx
            .as_admin("POST", &format!("/sources/{id}/discover"), None)
            .await;
        assert_eq!(discovered.status, StatusCode::OK, "{:?}", discovered.body);
        let saved = fx
            .as_admin(
                "PUT",
                &format!("/sources/{id}/streams/{topic}"),
                Some(Fixture::consumer_settings(false)),
            )
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);

        let started = std::time::Instant::now();
        let preview = fx
            .as_admin(
                "POST",
                &format!("/sources/{id}/streams/{topic}/preview"),
                Some(json!({"limit": 10})),
            )
            .await;

        assert_eq!(preview.status, StatusCode::OK, "{:?}", preview.body);
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        assert_eq!(preview.body["read"], 2);
        assert_eq!(preview.body["truncated"], false);
        let keys: Vec<_> = preview.body["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record["key"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(keys, ["k1", "k2"]);
    }
}
