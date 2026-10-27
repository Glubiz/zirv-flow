//! The destinations API end to end, driven through the router against the fake destination.

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
    const MARKER: &str = "zz-destination-marker-4711";
    const REPLACED_MARKER: &str = "zz-destination-replaced-0815";

    struct Reply {
        status: StatusCode,
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
        other: WorkspaceId,
        read: String,
        admin: String,
        /// An admin key of another workspace.
        stranger: String,
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
        fixture_with_policy(pool, keyring, Arc::default()).await
    }

    async fn fixture_with_policy(
        pool: &PgPool,
        keyring: Option<Arc<Keyring>>,
        policy: Arc<zirv_flow_core::egress::EgressPolicy>,
    ) -> Fixture {
        zirv_flow_store::migrate(pool).await.unwrap();
        let create = |slug: &'static str| async move {
            let name = WorkspaceName::parse(slug).unwrap();
            let slug = Slug::parse(slug).unwrap();
            zirv_flow_store::create_workspace(pool, &name, &slug)
                .await
                .unwrap()
                .id
        };
        let ws = create("ours").await;
        let other = create("theirs").await;
        let mut state =
            AppState::new(pool.clone(), Arc::new(String::new)).with_egress_policy(policy);
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
        let key = |workspace: WorkspaceId, scopes: &'static [Scope]| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), workspace, "destinations", scopes)
                    .await
                    .unwrap()
                    .plaintext
            }
        };
        let read = key(ws, &[Scope::Read]).await;
        let admin_key = key(ws, &[Scope::Admin, Scope::Read]).await;
        let stranger = key(other, &[Scope::Admin, Scope::Read]).await;
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
            other,
            read,
            admin: admin_key,
            stranger,
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

        async fn create(&self, name: &str, token: &str) -> Reply {
            self.as_admin(
                "POST",
                "/destinations",
                Some(json!({
                    "kind": "fake",
                    "name": name,
                    "config": {"mode": "ok"},
                    "credential": {"token": token},
                })),
            )
            .await
        }

        async fn create_ok(&self, name: &str, token: &str) -> String {
            let reply = self.create(name, token).await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().to_owned()
        }

        async fn audit_actions(&self) -> Vec<String> {
            let mut entries = zirv_flow_store::list_audit(&self.pool, 200).await.unwrap();
            entries.reverse();
            entries
                .into_iter()
                .map(|entry| entry.action)
                .filter(|action| {
                    action.starts_with("destination.") || action.starts_with("secret.")
                })
                .collect()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_destinations_endpoint_is_admin_only_and_no_access_is_404(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.create_ok("Mail", "ok").await;
        let body =
            json!({"kind": "fake", "name": "X", "config": {}, "credential": {"token": "ok"}});
        let patch = json!({"name": "Y"});
        let endpoints: Vec<(&str, String, Option<Value>)> = vec![
            ("GET", "/destinations".into(), None),
            ("POST", "/destinations".into(), Some(body)),
            ("GET", format!("/destinations/{id}"), None),
            ("PATCH", format!("/destinations/{id}"), Some(patch)),
            ("POST", format!("/destinations/{id}/check"), None),
            ("DELETE", format!("/destinations/{id}"), None),
        ];

        for (method, tail, body) in &endpoints {
            let anonymous = fx.send(method, tail, body.clone(), Auth::Anonymous).await;
            let read_key = fx
                .send(method, tail, body.clone(), Auth::Key(&fx.read))
                .await;
            let member = fx
                .send(method, tail, body.clone(), Auth::Session(&fx.member))
                .await;
            let stranger = fx
                .send(method, tail, body.clone(), Auth::Key(&fx.stranger))
                .await;

            assert_eq!(
                anonymous.status,
                StatusCode::UNAUTHORIZED,
                "{method} {tail}"
            );
            assert_eq!(read_key.status, StatusCode::FORBIDDEN, "{method} {tail}");
            assert_eq!(member.status, StatusCode::FORBIDDEN, "{method} {tail}");
            assert_eq!(stranger.status, StatusCode::NOT_FOUND, "{method} {tail}");
        }
        // None of the refused calls changed anything.
        let listed = fx.as_admin("GET", "/destinations", None).await;
        assert_eq!(listed.body.as_array().unwrap().len(), 1);
        assert_eq!(listed.body[0]["name"], "Mail");
        // An admin of ours reaches every endpoint.
        let read = fx
            .as_admin("GET", &format!("/destinations/{id}"), None)
            .await;
        assert_eq!(read.status, StatusCode::OK);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_admin_of_one_workspace_cannot_reach_a_destination_of_another(pool: PgPool) {
        let fx = fixture(&pool).await;
        // Workspace B's destination, made by B's own admin.
        let created = call(
            &fx.router,
            "POST",
            &format!("/api/v1/workspaces/{}/destinations", fx.other.as_uuid()),
            Some(json!({"kind": "fake", "name": "Theirs", "config": {},
                        "credential": {"token": "ok"}})),
            Auth::Key(&fx.stranger),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let theirs = created.body["id"].as_str().unwrap().to_owned();
        let snapshot = || async {
            let row: String = sqlx::query_scalar(
                "SELECT d::text || coalesce((SELECT b::text FROM destination_rate_buckets b \
                                              WHERE b.destination_id = d.id), '') \
                 FROM destinations d WHERE d.id = $1::uuid",
            )
            .bind(&theirs)
            .fetch_one(&pool)
            .await
            .unwrap();
            let secrets: i64 = sqlx::query_scalar("SELECT count(*) FROM encrypted_secrets")
                .fetch_one(&pool)
                .await
                .unwrap();
            (row, secrets)
        };
        let before = snapshot().await;

        // A (ours) names B's id on every endpoint that takes one.
        let attempts = [
            ("GET", format!("/destinations/{theirs}"), None),
            (
                "PATCH",
                format!("/destinations/{theirs}"),
                Some(json!({"name": "Taken", "rate": 1, "burst": 1,
                            "credential": {"token": "stolen"}})),
            ),
            ("POST", format!("/destinations/{theirs}/check"), None),
            ("DELETE", format!("/destinations/{theirs}"), None),
        ];
        for (method, tail, body) in attempts {
            let reply = fx.as_admin(method, &tail, body).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {tail}");
        }
        // The list shows none of B's, and creating under A never touches B.
        let listed = fx.as_admin("GET", "/destinations", None).await;
        assert_eq!(listed.body, json!([]));

        assert_eq!(snapshot().await, before, "B's destination is unchanged");
        // B's own admin still reads it untouched.
        let own = call(
            &fx.router,
            "GET",
            &format!(
                "/api/v1/workspaces/{}/destinations/{theirs}",
                fx.other.as_uuid()
            ),
            None,
            Auth::Key(&fx.stranger),
        )
        .await;
        assert_eq!(own.body["name"], "Theirs");
        assert_eq!(own.body["archived"], false);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_admin_creates_reads_lists_updates_checks_and_archives(pool: PgPool) {
        let fx = fixture(&pool).await;

        let created = fx.create("Mail", "ok").await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let id = created.body["id"].as_str().unwrap().to_owned();
        assert_eq!(created.body["credential_fields_set"], json!(["token"]));
        assert_eq!(created.body["config"], json!({"mode": "ok"}));
        assert_eq!(
            (
                created.body["rate"].as_i64(),
                created.body["burst"].as_i64()
            ),
            (Some(10), Some(10))
        );
        assert_eq!(created.body["archived"], false);

        let read = fx
            .as_admin("GET", &format!("/destinations/{id}"), None)
            .await;
        let listed = fx.as_admin("GET", "/destinations", None).await;
        assert_eq!(read.body, created.body);
        assert_eq!(listed.body[0]["id"], id.as_str());

        let passing = fx
            .as_admin("POST", &format!("/destinations/{id}/check"), None)
            .await;
        assert_eq!(passing.status, StatusCode::OK);
        assert_eq!(
            passing.body,
            json!({"ok": true, "error": null, "code": null})
        );

        let changed = fx
            .as_admin(
                "PATCH",
                &format!("/destinations/{id}"),
                Some(json!({"name": "Mail 2", "rate": 50, "burst": 80,
                            "credential": {"token": "wrong"}})),
            )
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.body);
        assert_eq!(changed.body["name"], "Mail 2");
        assert_eq!(
            (
                changed.body["rate"].as_i64(),
                changed.body["burst"].as_i64()
            ),
            (Some(50), Some(80))
        );
        let failing = fx
            .as_admin("POST", &format!("/destinations/{id}/check"), None)
            .await;
        assert_eq!(failing.status, StatusCode::OK);
        assert_eq!(
            failing.body,
            json!({"ok": false, "error": "the destination refused the credential",
                   "code": "credential_refused"})
        );

        let archived = fx
            .as_admin("DELETE", &format!("/destinations/{id}"), None)
            .await;
        assert_eq!(archived.status, StatusCode::NO_CONTENT);
        let listed = fx.as_admin("GET", "/destinations", None).await;
        assert_eq!(listed.body, json!([]));
        let read = fx
            .as_admin("GET", &format!("/destinations/{id}"), None)
            .await;
        assert_eq!(read.status, StatusCode::OK);
        assert_eq!(read.body["archived"], true);
        for (method, tail, body) in [
            (
                "PATCH",
                format!("/destinations/{id}"),
                Some(json!({"name": "Z"})),
            ),
            ("POST", format!("/destinations/{id}/check"), None),
            ("DELETE", format!("/destinations/{id}"), None),
        ] {
            let reply = fx.as_admin(method, &tail, body).await;
            assert_eq!(reply.status, StatusCode::CONFLICT, "{method}");
            assert_eq!(reply.body["code"], "destination_archived", "{method}");
        }
        // The archived name is free again.
        fx.create_ok("Mail 2", "ok").await;
        let missing = fx
            .as_admin(
                "GET",
                &format!("/destinations/{}", uuid::Uuid::now_v7()),
                None,
            )
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);

        assert_eq!(
            fx.audit_actions().await,
            [
                "secret.created",
                "destination.created",
                "destination.checked",
                "secret.created",
                "secret.deleted",
                "destination.updated",
                "destination.checked",
                "destination.archived",
                "secret.created",
                "destination.created",
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn invalid_requests_are_refused_with_the_field_and_a_taken_name_is_a_conflict(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        fx.create_ok("Mail", "ok").await;
        let post = |body: Value| fx.as_admin("POST", "/destinations", Some(body));

        let unknown_kind = post(json!({"kind": "nonesuch", "name": "A", "config": {}})).await;
        let stray_setting = post(json!({"kind": "fake", "name": "A", "config": {"nope": 1},
                                        "credential": {"token": "ok"}}))
        .await;
        let no_credential = post(json!({"kind": "fake", "name": "A", "config": {}})).await;
        let bad_rate = post(json!({"kind": "fake", "name": "A", "config": {},
                                   "credential": {"token": "ok"}, "rate": 0}))
        .await;
        let bad_burst = post(json!({"kind": "fake", "name": "A", "config": {},
                                    "credential": {"token": "ok"}, "burst": 100001}))
        .await;
        let bad_name = post(json!({"kind": "fake", "name": "  ", "config": {},
                                   "credential": {"token": "ok"}}))
        .await;
        let taken = post(json!({"kind": "fake", "name": "MAIL", "config": {},
                                "credential": {"token": "ok"}}))
        .await;
        let empty_patch = fx
            .as_admin(
                "PATCH",
                &format!(
                    "/destinations/{}",
                    fx.as_admin("GET", "/destinations", None).await.body[0]["id"]
                        .as_str()
                        .unwrap()
                ),
                Some(json!({})),
            )
            .await;

        for reply in [
            &unknown_kind,
            &stray_setting,
            &no_credential,
            &bad_rate,
            &bad_burst,
            &bad_name,
            &empty_patch,
        ] {
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{:?}",
                reply.body
            );
        }
        assert!(
            stray_setting.body["detail"]
                .as_str()
                .unwrap()
                .contains("config.nope")
        );
        assert!(
            no_credential.body["detail"]
                .as_str()
                .unwrap()
                .contains("credential.token")
        );
        assert!(bad_rate.body["detail"].as_str().unwrap().contains("rate"));
        assert_eq!(taken.status, StatusCode::CONFLICT);
        assert_eq!(
            fx.as_admin("GET", "/destinations", None)
                .await
                .body
                .as_array()
                .unwrap()
                .len(),
            1,
            "a refused create stores nothing"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_credential_is_sealed_with_its_own_purpose_and_never_returned_logged_or_audited(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.create_ok("Mail", MARKER).await;
        fx.as_admin("GET", &format!("/destinations/{id}"), None)
            .await;
        fx.as_admin("GET", "/destinations", None).await;
        fx.as_admin("POST", &format!("/destinations/{id}/check"), None)
            .await;
        fx.as_admin(
            "PATCH",
            &format!("/destinations/{id}"),
            Some(json!({"credential": {"token": REPLACED_MARKER}})),
        )
        .await;
        fx.as_admin("POST", &format!("/destinations/{id}/check"), None)
            .await;

        let purposes: Vec<String> = sqlx::query_scalar("SELECT purpose FROM encrypted_secrets")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(purposes, ["workspace_credential"]);
        let seen = fx.seen.lock().unwrap().join("\n");
        assert!(
            !seen.contains(MARKER) && !seen.contains(REPLACED_MARKER),
            "{seen}"
        );
        let audit = zirv_flow_store::list_audit(&pool, 200)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| format!("{entry:?}"))
            .collect::<String>();
        assert!(!audit.contains(MARKER) && !audit.contains(REPLACED_MARKER));
        let row: String =
            sqlx::query_scalar("SELECT d::text FROM destinations d WHERE d.id = $1::uuid")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        // The field names now live on the credential the destination uses.
        let credential: String = sqlx::query_scalar("SELECT c::text FROM credentials c")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(credential.contains("token") && !credential.contains(MARKER));
        assert!(!row.contains(MARKER));
        tables_hold_no(&pool, &[MARKER, REPLACED_MARKER]).await;
        the_scan_finds_a_planted(&pool, &[MARKER]).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_operator_allowlist_admits_only_the_named_private_address(pool: PgPool) {
        let policy = zirv_flow_core::egress::EgressPolicy::parse(&["10.0.0.5/32".into()]).unwrap();
        let keys = Arc::new(Keyring::new(MasterKey::generate(), None).unwrap());
        let fx = fixture_with_policy(&pool, Some(keys), Arc::new(policy)).await;
        for (host, allowed) in [
            ("10.0.0.5", true),
            ("10.0.0.6", false),
            ("169.254.169.254", false),
        ] {
            let created = fx.as_admin("POST", "/destinations", Some(json!({
                "kind": "webhook", "name": host, "config": {"url": format!("http://{host}/")},
                "credential": {"signing_secret": "secret"}
            }))).await;
            assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
            let id = created.body["id"].as_str().unwrap();
            let checked = fx
                .as_admin("POST", &format!("/destinations/{id}/check"), None)
                .await;
            if allowed {
                assert_eq!(checked.status, StatusCode::OK);
                assert_eq!(checked.body["ok"], true);
            } else {
                assert_eq!(checked.status, StatusCode::UNPROCESSABLE_ENTITY);
                assert_eq!(checked.body["code"], "egress_denied");
            }
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn real_destinations_refuse_private_targets_with_a_typed_problem(pool: PgPool) {
        let fx = fixture(&pool).await;
        for kind in ["webhook", "smtp", "provider"] {
            let mut manifest: Value = serde_json::from_str(include_str!(
                "../../../zirv-flow-connectors/tests/fixtures/providers/bearer_timestamped.json"
            ))
            .unwrap();
            manifest["base_url"] = json!("http://169.254.169.254/");
            let (config, credential) = match kind {
                "webhook" => (
                    json!({"url": "http://169.254.169.254/secret-path?token=secret-query"}),
                    json!({"signing_secret": "secret"}),
                ),
                "smtp" => (
                    json!({}),
                    json!({"host": "169.254.169.254", "port": "25", "tls": "starttls", "username": "user", "password": "secret"}),
                ),
                _ => (
                    json!({"manifest": manifest}),
                    json!({"token": "secret", "feedback_secret": "secret"}),
                ),
            };
            let created = fx
                .as_admin(
                    "POST",
                    "/destinations",
                    Some(json!({
                        "kind": kind, "name": kind, "config": config, "credential": credential,
                    })),
                )
                .await;
            assert_eq!(
                created.status,
                StatusCode::CREATED,
                "{kind}: {}",
                created.body
            );
            let id = created.body["id"].as_str().unwrap();
            let checked = fx
                .as_admin("POST", &format!("/destinations/{id}/check"), None)
                .await;
            assert_eq!(
                checked.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{kind}: {}",
                checked.body
            );
            assert_eq!(
                checked.headers[header::CONTENT_TYPE],
                "application/problem+json"
            );
            assert_eq!(checked.body["code"], "egress_denied");
            for secret in ["169.254.169.254", "secret-path", "secret-query"] {
                assert!(!checked.body.to_string().contains(secret));
            }
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_check_that_the_egress_policy_refuses_is_a_typed_problem(pool: PgPool) {
        let fx = fixture(&pool).await;
        let created = fx
            .as_admin(
                "POST",
                "/destinations",
                Some(
                    json!({"kind": "fake", "name": "Private", "config": {"host": "10.0.0.5"},
                            "credential": {"token": "ok"}}),
                ),
            )
            .await;
        let id = created.body["id"].as_str().unwrap().to_owned();

        let checked = fx
            .as_admin("POST", &format!("/destinations/{id}/check"), None)
            .await;

        assert_eq!(
            checked.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            checked.body
        );
        assert_eq!(
            checked.headers[header::CONTENT_TYPE],
            "application/problem+json"
        );
        assert_eq!(checked.body["code"], "egress_denied");
        let text = checked.body.to_string();
        assert!(!text.contains("10.0.0.5"), "the host is not named: {text}");
        assert!(!text.contains("http"), "no URL: {text}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_credential_needs_the_master_key(pool: PgPool) {
        let fx = fixture_with(&pool, None).await;

        let sealed = fx.create("Mail", "ok").await;
        let without = fx
            .as_admin(
                "POST",
                "/destinations",
                Some(json!({"kind": "fake", "name": "Bare", "config": {}, "credential": {}})),
            )
            .await;

        assert_eq!(
            sealed.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{:?}",
            sealed.body
        );
        // The fake requires its token, so a credential-less create is refused before sealing.
        assert_eq!(without.status, StatusCode::UNPROCESSABLE_ENTITY);
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM destinations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    async fn attempt(
        fx: &Fixture,
        workspace: WorkspaceId,
        destination: &str,
        outcome: zirv_flow_store::AttemptOutcome,
        status: Option<u16>,
        minutes_ago: i32,
    ) {
        let row = zirv_flow_store::record_delivery_attempt(
            &fx.pool,
            workspace,
            &zirv_flow_store::NewDeliveryAttempt {
                destination_id: destination.parse().unwrap(),
                subject_kind: zirv_flow_store::SubjectKind::Message,
                subject_id: uuid::Uuid::now_v7(),
                attempt: 1,
                outcome,
                status_class: zirv_flow_store::StatusClass::of(status),
                duration_ms: 5,
            },
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE delivery_attempts SET attempted_at = now() - make_interval(mins => $2)              WHERE id = $1",
        )
        .bind(row.id)
        .bind(minutes_ago)
        .execute(&fx.pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_stats_count_attempts_by_outcome_and_status_class_inside_the_window(pool: PgPool) {
        use zirv_flow_store::AttemptOutcome::{Delivered, Retryable, Terminal};
        let fx = fixture(&pool).await;
        let id = fx.create_ok("Mail", "ok").await;
        attempt(&fx, fx.ws, &id, Delivered, Some(202), 5).await;
        attempt(&fx, fx.ws, &id, Delivered, Some(200), 60).await;
        attempt(&fx, fx.ws, &id, Retryable, Some(503), 90).await;
        attempt(&fx, fx.ws, &id, Terminal, Some(404), 100).await;
        attempt(&fx, fx.ws, &id, Retryable, None, 110).await;
        // Inside the day but outside two hours, and outside the day.
        attempt(&fx, fx.ws, &id, Delivered, Some(301), 121).await;
        attempt(&fx, fx.ws, &id, Delivered, Some(200), 25 * 60).await;

        let day = fx
            .as_admin("GET", &format!("/destinations/{id}/stats"), None)
            .await;
        let two_hours = fx
            .as_admin("GET", &format!("/destinations/{id}/stats?hours=2"), None)
            .await;
        let week = fx
            .as_admin("GET", &format!("/destinations/{id}/stats?hours=168"), None)
            .await;

        assert_eq!(day.status, StatusCode::OK, "{:?}", day.body);
        assert_eq!(day.body["window_hours"], 24);
        assert_eq!(day.body["attempts"], 6);
        assert_eq!(
            day.body["outcomes"],
            json!({"delivered": 3, "retryable": 2, "terminal": 1, "throttled": 0})
        );
        assert_eq!(
            day.body["status_classes"],
            json!({"none": 1, "2xx": 2, "3xx": 1, "4xx": 1, "5xx": 1})
        );
        assert_eq!(two_hours.body["window_hours"], 2);
        assert_eq!(two_hours.body["attempts"], 5);
        assert_eq!(two_hours.body["status_classes"]["3xx"], 0);
        assert_eq!(week.body["attempts"], 7);
        // Only counts: no subject id, no person.
        assert!(!day.body.to_string().contains("subject"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_stats_refuse_a_bad_window_other_workspaces_and_non_admins(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.create_ok("Mail", "ok").await;
        let empty = fx
            .as_admin("GET", &format!("/destinations/{id}/stats"), None)
            .await;
        assert_eq!(empty.body["attempts"], 0);
        assert_eq!(empty.body["outcomes"]["delivered"], 0);

        for bad in ["0", "169", "-1", "x", ""] {
            let reply = fx
                .as_admin(
                    "GET",
                    &format!("/destinations/{id}/stats?hours={bad}"),
                    None,
                )
                .await;
            assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        }
        let tail = format!("/destinations/{id}/stats");
        let anonymous = fx.send("GET", &tail, None, Auth::Anonymous).await;
        let read_key = fx.send("GET", &tail, None, Auth::Key(&fx.read)).await;
        let member = fx.send("GET", &tail, None, Auth::Session(&fx.member)).await;
        let stranger = fx.send("GET", &tail, None, Auth::Key(&fx.stranger)).await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(read_key.status, StatusCode::FORBIDDEN);
        assert_eq!(member.status, StatusCode::FORBIDDEN);
        assert_eq!(stranger.status, StatusCode::NOT_FOUND);
        // A destination of another workspace is not found through ours.
        let theirs = call(
            &fx.router,
            "POST",
            &format!("/api/v1/workspaces/{}/destinations", fx.other.as_uuid()),
            Some(json!({"kind": "fake", "name": "Theirs", "config": {},
                        "credential": {"token": "ok"}})),
            Auth::Key(&fx.stranger),
        )
        .await;
        let theirs = theirs.body["id"].as_str().unwrap().to_owned();
        let crossed = fx
            .as_admin("GET", &format!("/destinations/{theirs}/stats"), None)
            .await;
        assert_eq!(crossed.status, StatusCode::NOT_FOUND);
        let missing = fx
            .as_admin(
                "GET",
                &format!("/destinations/{}/stats", uuid::Uuid::now_v7()),
                None,
            )
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_kinds_endpoint_needs_a_sign_in_and_lists_every_kind_with_secrets_marked(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;

        let anonymous = call(
            &fx.router,
            "GET",
            "/api/v1/destination-kinds",
            None,
            Auth::Anonymous,
        )
        .await;
        let member = call(
            &fx.router,
            "GET",
            "/api/v1/destination-kinds",
            None,
            Auth::Session(&fx.member),
        )
        .await;

        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(member.status, StatusCode::OK, "{:?}", member.body);
        let kinds: Vec<_> = member
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|kind| kind["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["webhook", "smtp", "provider", "fake"]);
        let smtp = &member.body[1];
        let password = smtp["credential_fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|field| field["key"] == "password")
            .unwrap();
        assert_eq!(password["secret"], true);
        assert_eq!(password["required"], true);
    }
}
