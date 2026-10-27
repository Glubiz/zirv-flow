//! The workspace credentials API end to end, driven through the router against the fake
//! destination and the fake source.

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
    use zirv_flow_core::{
        HmacRoot, Keyring, MasterKey, Role, Scope, Slug, WorkspaceId, WorkspaceName,
        deployment_key_aad, seal,
    };
    use zirv_flow_store::secrets;

    use crate::admin::{self, Context};
    use crate::test_support::{tables_hold_no, the_scan_finds_a_planted};
    use crate::{AppState, router};

    const PASSWORD: &str = "correct horse battery";
    /// Longer than 16 characters, and distinct from anything else a response or a table may hold.
    const MARKER: &str = "zz-credential-marker-4711";
    const ROTATED_MARKER: &str = "zz-credential-rotated-0815";

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
        keyring: Arc<Keyring>,
        ws: WorkspaceId,
        other: WorkspaceId,
        read: String,
        admin: String,
        /// An admin key of another workspace.
        stranger: String,
        member: Login,
        /// Every response body seen, as text, to scan for secret values.
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
        let held =
            keyring.unwrap_or_else(|| Arc::new(Keyring::new(MasterKey::generate(), None).unwrap()));
        let root = HmacRoot::bootstrap_from(held.current());
        let sealed = seal(
            &held,
            root.to_secret_bytes().expose(),
            &deployment_key_aad(secrets::HMAC_ROOT),
        );
        secrets::insert_hmac_root_if_absent(pool, &sealed)
            .await
            .unwrap();
        let state = AppState::new(pool.clone(), Arc::new(String::new))
            .with_keyring(held.clone())
            .with_hmac_root(&root);
        let router = router(state);
        let key = |workspace: WorkspaceId, scopes: &'static [Scope]| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), workspace, "credentials", scopes)
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
            keyring: held,
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
        fixture_with(pool, None).await
    }

    fn token_schema() -> Value {
        json!({"fields": [
            {"key": "token", "label": "Token", "type": "string", "required": true, "secret": true},
            {"key": "region", "label": "Region", "type": "string"}
        ]})
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
                "/credentials",
                Some(
                    json!({"kind": "custom", "name": name, "schema": token_schema(),
                            "fields": {"region": "eu"}, "secret": {"token": token}}),
                ),
            )
            .await
        }

        async fn create_ok(&self, name: &str, token: &str) -> String {
            let reply = self.create(name, token).await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().to_owned()
        }

        async fn destination(&self, name: &str, credential: &str, config: Value) -> String {
            let reply = self
                .as_admin(
                    "POST",
                    "/destinations",
                    Some(json!({"kind": "fake", "name": name, "config": config,
                                "credential_id": credential})),
                )
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().to_owned()
        }

        async fn rotate(&self, id: &str, token: &str) -> Reply {
            self.as_admin(
                "PUT",
                &format!("/credentials/{id}/secret"),
                Some(json!({"secret": {"token": token}})),
            )
            .await
        }

        async fn check(&self, destination: &str) -> Value {
            self.as_admin("POST", &format!("/destinations/{destination}/check"), None)
                .await
                .body
        }

        async fn audit_actions(&self) -> Vec<String> {
            let mut entries = zirv_flow_store::list_audit(&self.pool, 300).await.unwrap();
            entries.reverse();
            entries
                .into_iter()
                .map(|entry| entry.action)
                .filter(|action| action.starts_with("credential.") || action.starts_with("secret."))
                .collect()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_credentials_endpoint_is_admin_only_and_no_access_is_404(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.create_ok("Mail", "ok").await;
        let destination = fx.destination("Mail", &id, json!({})).await;
        let body = json!({"kind": "custom", "name": "X", "schema": token_schema(),
                          "secret": {"token": "ok"}});
        let endpoints: Vec<(&str, String, Option<Value>)> = vec![
            ("GET", "/credential-kinds".into(), None),
            ("GET", "/credentials".into(), None),
            ("POST", "/credentials".into(), Some(body)),
            ("GET", format!("/credentials/{id}"), None),
            (
                "PATCH",
                format!("/credentials/{id}"),
                Some(json!({"name": "Y"})),
            ),
            (
                "PUT",
                format!("/credentials/{id}/secret"),
                Some(json!({"secret": {"token": "new"}})),
            ),
            (
                "POST",
                format!("/credentials/{id}/test"),
                Some(json!({"destination_id": destination})),
            ),
            ("DELETE", format!("/credentials/{id}"), None),
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
        let listed = fx.as_admin("GET", "/credentials", None).await;
        assert_eq!(listed.body.as_array().unwrap().len(), 1);
        assert_eq!(listed.body[0]["name"], "Mail");
        let read = fx
            .as_admin("GET", &format!("/credentials/{id}"), None)
            .await;
        assert_eq!(read.status, StatusCode::OK);
        assert!(read.body["last_tested_at"].is_null());
        assert_eq!(read.body["hints"], json!({"token": ""}));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_admin_of_one_workspace_cannot_reach_a_credential_of_another(pool: PgPool) {
        let fx = fixture(&pool).await;
        let created = call(
            &fx.router,
            "POST",
            &format!("/api/v1/workspaces/{}/credentials", fx.other.as_uuid()),
            Some(
                json!({"kind": "custom", "name": "Theirs", "schema": token_schema(),
                        "secret": {"token": "theirs-token"}}),
            ),
            Auth::Key(&fx.stranger),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let theirs = created.body["id"].as_str().unwrap().to_owned();
        let snapshot = || async {
            let row: String =
                sqlx::query_scalar("SELECT c::text FROM credentials c WHERE c.id = $1::uuid")
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

        let attempts = [
            ("GET", format!("/credentials/{theirs}"), None),
            (
                "PATCH",
                format!("/credentials/{theirs}"),
                Some(json!({"name": "Taken"})),
            ),
            (
                "PUT",
                format!("/credentials/{theirs}/secret"),
                Some(json!({"secret": {"token": "stolen"}})),
            ),
            (
                "POST",
                format!("/credentials/{theirs}/test"),
                Some(json!({"source_id": Uuid::now_v7()})),
            ),
            ("DELETE", format!("/credentials/{theirs}"), None),
        ];
        for (method, tail, body) in attempts {
            let reply = fx.as_admin(method, &tail, body).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {tail}");
        }
        // Naming their credential from our destination is a refused field, not a link.
        let linked = fx
            .as_admin(
                "POST",
                "/destinations",
                Some(json!({"kind": "fake", "name": "Mine", "config": {},
                            "credential_id": theirs})),
            )
            .await;

        assert_eq!(
            linked.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            linked.body
        );
        assert!(
            linked.body["detail"]
                .as_str()
                .unwrap()
                .contains("credential_id")
        );
        assert_eq!(
            fx.as_admin("GET", "/credentials", None).await.body,
            json!([])
        );
        assert_eq!(snapshot().await, before, "their credential is unchanged");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_kinds_endpoint_lists_every_schema_and_no_value(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.create_ok("Mail", MARKER).await;

        let kinds = fx.as_admin("GET", "/credential-kinds", None).await;

        assert_eq!(kinds.status, StatusCode::OK);
        let ids: Vec<_> = kinds
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|kind| kind["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "api_key",
                "basic",
                "smtp",
                "oauth2_client",
                "signing_secret",
                "bearer_token",
                "tls_client"
            ]
        );
        let smtp = kinds.body[2]["schema"]["fields"].as_array().unwrap();
        assert_eq!(smtp.len(), 5);
        assert!(smtp.iter().all(|field| field.get("value").is_none()));
        let text = kinds.body.to_string();
        assert!(
            !text.contains(MARKER),
            "no stored value is listed with a kind"
        );
        assert_eq!(
            smtp.iter().filter(|field| field["secret"] == true).count(),
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_admin_creates_reads_lists_changes_rotates_tests_and_archives(pool: PgPool) {
        let fx = fixture(&pool).await;

        let created = fx.create("Mail account", MARKER).await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let id = created.body["id"].as_str().unwrap().to_owned();
        assert_eq!(created.body["key"], "mail-account");
        assert_eq!(created.body["kind"], "custom");
        assert_eq!(created.body["fields"], json!({"region": "eu"}));
        assert_eq!(created.body["hints"], json!({"token": "4711"}));
        assert_eq!(created.body["used_by"], json!([]));
        assert_eq!(created.body["archived"], false);
        assert_eq!(created.body["fingerprint"].as_str().unwrap().len(), 64);
        assert_eq!(created.body["schema"]["fields"][0]["secret"], true);

        let read = fx
            .as_admin("GET", &format!("/credentials/{id}"), None)
            .await;
        let listed = fx.as_admin("GET", "/credentials", None).await;
        assert_eq!(read.body, created.body);
        assert_eq!(listed.body[0]["id"], id.as_str());

        let destination = fx.destination("Mail", &id, json!({})).await;
        let used = fx
            .as_admin("GET", &format!("/credentials/{id}"), None)
            .await;
        assert_eq!(
            used.body["used_by"],
            json!([{"kind": "destination", "id": destination, "name": "Mail"}])
        );

        let changed = fx
            .as_admin(
                "PATCH",
                &format!("/credentials/{id}"),
                Some(json!({"name": "Mail account 2", "fields": {"region": "us"}})),
            )
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.body);
        assert_eq!(changed.body["name"], "Mail account 2");
        assert_eq!(changed.body["fields"], json!({"region": "us"}));
        assert_eq!(
            changed.body["hints"], created.body["hints"],
            "a patch leaves the secret alone"
        );

        let failing = fx
            .as_admin(
                "POST",
                &format!("/credentials/{id}/test"),
                Some(json!({"destination_id": destination})),
            )
            .await;
        assert_eq!(failing.status, StatusCode::OK, "{:?}", failing.body);
        assert_eq!(failing.body["ok"], false);
        assert_eq!(failing.body["code"], "credential_refused");
        let after_fail = fx
            .as_admin("GET", &format!("/credentials/{id}"), None)
            .await;
        assert_eq!(after_fail.body["last_test_outcome"], "failed");
        assert!(after_fail.body["last_tested_at"].is_string());

        let rotated = fx.rotate(&id, "ok").await;
        assert_eq!(rotated.status, StatusCode::OK, "{:?}", rotated.body);
        assert_eq!(rotated.body["hints"], json!({"token": ""}));
        assert_ne!(rotated.body["fingerprint"], created.body["fingerprint"]);
        let passing = fx
            .as_admin(
                "POST",
                &format!("/credentials/{id}/test"),
                Some(json!({"destination_id": destination})),
            )
            .await;
        assert_eq!(
            passing.body,
            json!({"ok": true, "message": null, "code": null})
        );
        let after_pass = fx
            .as_admin("GET", &format!("/credentials/{id}"), None)
            .await;
        assert_eq!(after_pass.body["last_test_outcome"], "ok");

        let refused = fx
            .as_admin("DELETE", &format!("/credentials/{id}"), None)
            .await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert_eq!(refused.body["code"], "credential_in_use");
        let released = fx
            .as_admin("DELETE", &format!("/destinations/{destination}"), None)
            .await;
        assert_eq!(released.status, StatusCode::NO_CONTENT);
        let archived = fx
            .as_admin("DELETE", &format!("/credentials/{id}"), None)
            .await;
        assert_eq!(archived.status, StatusCode::NO_CONTENT);
        assert_eq!(
            fx.as_admin("GET", "/credentials", None).await.body,
            json!([])
        );
        let read = fx
            .as_admin("GET", &format!("/credentials/{id}"), None)
            .await;
        assert_eq!(read.body["archived"], true);
        for (method, tail, body) in [
            (
                "PATCH",
                format!("/credentials/{id}"),
                Some(json!({"name": "Z"})),
            ),
            (
                "PUT",
                format!("/credentials/{id}/secret"),
                Some(json!({"secret": {"token": "n"}})),
            ),
            (
                "POST",
                format!("/credentials/{id}/test"),
                Some(json!({"destination_id": destination})),
            ),
            ("DELETE", format!("/credentials/{id}"), None),
        ] {
            let reply = fx.as_admin(method, &tail, body).await;
            assert_eq!(reply.status, StatusCode::CONFLICT, "{method}");
            assert_eq!(reply.body["code"], "credential_archived", "{method}");
        }
        // An archived credential cannot be referenced, and its name is free again.
        let reuse = fx
            .as_admin(
                "POST",
                "/destinations",
                Some(json!({"kind": "fake", "name": "Again", "config": {}, "credential_id": id})),
            )
            .await;
        assert_eq!(reuse.status, StatusCode::CONFLICT);
        assert_eq!(reuse.body["code"], "credential_archived");
        fx.create_ok("Mail account 2", "ok").await;

        assert_eq!(
            fx.audit_actions().await,
            [
                "secret.created",
                "credential.created",
                "credential.updated",
                "credential.tested",
                "secret.created",
                "secret.deleted",
                "credential.rotated",
                "credential.tested",
                "credential.archived",
                "secret.created",
                "credential.created",
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_destinations_share_one_credential_and_a_rotation_reaches_both_without_editing_either(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.create_ok("Shared", "not-yet-the-token").await;
        let first = fx.destination("First", &id, json!({})).await;
        let second = fx.destination("Second", &id, json!({})).await;
        let before: Vec<String> =
            sqlx::query_scalar("SELECT d::text FROM destinations d ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();

        assert_eq!(fx.check(&first).await["code"], "credential_refused");
        assert_eq!(fx.check(&second).await["code"], "credential_refused");
        let rotated = fx.rotate(&id, "ok").await;
        assert_eq!(rotated.status, StatusCode::OK, "{:?}", rotated.body);

        assert_eq!(fx.check(&first).await["ok"], true);
        assert_eq!(fx.check(&second).await["ok"], true);
        let again = fx.rotate(&id, "bad-again").await;
        assert_eq!(again.status, StatusCode::OK);
        assert_eq!(fx.check(&first).await["code"], "credential_refused");
        assert_eq!(fx.check(&second).await["code"], "credential_refused");
        let after: Vec<String> =
            sqlx::query_scalar("SELECT d::text FROM destinations d ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(after, before, "no destination was edited");
        let users = fx
            .as_admin("GET", &format!("/credentials/{id}"), None)
            .await;
        assert_eq!(users.body["used_by"].as_array().unwrap().len(), 2);
        let secrets_left: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM encrypted_secrets WHERE purpose = 'workspace_credential'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            secrets_left, 1,
            "each rotation deleted the secret it replaced"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_source_uses_a_workspace_credential_and_follows_its_rotation(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.create_ok("Source token", "ok").await;
        let created = fx
            .as_admin(
                "POST",
                "/sources",
                Some(json!({"kind": "fake", "name": "CRM", "config": {}, "credential_id": id})),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let source = created.body["id"].as_str().unwrap().to_owned();
        assert_eq!(created.body["credential_id"], id.as_str());
        assert_eq!(
            created.body["credential_fields_set"],
            json!(["region", "token"])
        );

        let tested = fx
            .as_admin(
                "POST",
                &format!("/credentials/{id}/test"),
                Some(json!({"source_id": source})),
            )
            .await;
        assert_eq!(tested.body["ok"], true, "{:?}", tested.body);
        fx.rotate(&id, "wrong").await;
        let refused = fx
            .as_admin(
                "POST",
                &format!("/credentials/{id}/test"),
                Some(json!({"source_id": source})),
            )
            .await;
        assert_eq!(refused.body["code"], "credential_refused");
        assert_eq!(refused.body["message"], "the source refused the credential");
        let used = fx
            .as_admin("GET", &format!("/credentials/{id}"), None)
            .await;
        assert_eq!(used.body["used_by"][0]["kind"], "source");
        let in_use = fx
            .as_admin("DELETE", &format!("/credentials/{id}"), None)
            .await;
        assert_eq!(in_use.body["code"], "credential_in_use");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_test_names_exactly_one_user_of_the_credential_and_no_host(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.create_ok("Mail", "ok").await;
        let other = fx.create_ok("Other", "ok").await;
        let private = fx
            .destination("Private", &id, json!({"host": "10.0.0.5"}))
            .await;
        let elsewhere = fx.destination("Elsewhere", &other, json!({})).await;
        let tail = format!("/credentials/{id}/test");
        let test = |body: Value| fx.as_admin("POST", &tail, Some(body));

        let denied = test(json!({"destination_id": private})).await;
        let none = test(json!({})).await;
        let both = test(json!({"destination_id": private, "source_id": Uuid::now_v7()})).await;
        let not_ours = test(json!({"destination_id": elsewhere})).await;
        let unknown = test(json!({"destination_id": Uuid::now_v7()})).await;

        assert_eq!(
            denied.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            denied.body
        );
        assert_eq!(
            denied.headers[header::CONTENT_TYPE],
            "application/problem+json"
        );
        assert_eq!(denied.body["status"], 422);
        assert_eq!(denied.body["code"], "egress_denied");
        assert!(denied.body.get("ok").is_none());
        for reply in [&denied, &none, &both, &not_ours, &unknown] {
            let text = format!("{} {:?}", reply.body, reply.headers);
            assert!(!text.contains("10.0.0.5"), "the host is not named: {text}");
            assert!(!text.contains("http"), "no URL: {text}");
        }
        for refused in [&none, &both, &not_ours, &unknown] {
            assert_eq!(
                refused.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{:?}",
                refused.body
            );
        }
        let read = fx
            .as_admin("GET", &format!("/credentials/{id}"), None)
            .await;
        assert_eq!(read.status, StatusCode::OK, "{:?}", read.body);
        assert_eq!(
            read.body["last_test_outcome"],
            Value::Null,
            "an egress refusal returns before recording a credential test"
        );
        assert_eq!(read.body["last_tested_at"], Value::Null);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_secret_is_in_no_response_audit_entry_problem_or_table_and_only_sealed_at_rest(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.create_ok("Mail", MARKER).await;
        let destination = fx.destination("Mail", &id, json!({})).await;
        fx.as_admin("GET", &format!("/credentials/{id}"), None)
            .await;
        fx.as_admin("GET", "/credentials", None).await;
        fx.as_admin("GET", "/credential-kinds", None).await;
        fx.as_admin(
            "POST",
            &format!("/credentials/{id}/test"),
            Some(json!({"destination_id": destination})),
        )
        .await;
        fx.rotate(&id, ROTATED_MARKER).await;
        // Refusals that carry the secret in the request must not echo it either.
        let wrong_group = fx
            .as_admin(
                "POST",
                "/credentials",
                Some(json!({"kind": "custom", "name": "Bad", "schema": token_schema(),
                            "fields": {"token": MARKER}, "secret": {"token": MARKER, "extra": MARKER}})),
            )
            .await;
        let wrong_rotation = fx
            .as_admin(
                "PUT",
                &format!("/credentials/{id}/secret"),
                Some(json!({"secret": {"token": MARKER, "other": MARKER}})),
            )
            .await;
        let malformed = fx
            .as_admin(
                "PUT",
                &format!("/credentials/{id}/secret"),
                Some(json!({"secret": {"token": MARKER}, "unknown": MARKER})),
            )
            .await;
        for refused in [&wrong_group, &wrong_rotation, &malformed] {
            assert!(
                refused.status.is_client_error(),
                "{:?} {:?}",
                refused.status,
                refused.body
            );
        }

        let purposes: Vec<String> = sqlx::query_scalar("SELECT purpose FROM encrypted_secrets")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(purposes, ["workspace_credential"]);
        let seen = fx.seen.lock().unwrap().join("\n");
        assert!(
            !seen.contains(MARKER) && !seen.contains(ROTATED_MARKER),
            "{seen}"
        );
        let audit = zirv_flow_store::list_audit(&pool, 300)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| format!("{entry:?}"))
            .collect::<String>();
        assert!(!audit.contains(MARKER) && !audit.contains(ROTATED_MARKER));
        let row: String = sqlx::query_scalar("SELECT c::text FROM credentials c")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(row.contains("token") && !row.contains(ROTATED_MARKER));
        tables_hold_no(&pool, &[MARKER, ROTATED_MARKER]).await;
        the_scan_finds_a_planted(&pool, &[MARKER]).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_hint_shows_the_last_four_characters_only_from_sixteen_characters_on(pool: PgPool) {
        let fx = fixture(&pool).await;
        let short = "a".repeat(15);
        let long = format!("{}wxyz", "b".repeat(12));

        let a = fx.create("Short", &short).await;
        let b = fx.create("Long", &long).await;

        assert_eq!(a.body["hints"], json!({"token": ""}));
        assert_eq!(b.body["hints"], json!({"token": "wxyz"}));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_same_secret_entered_twice_is_flagged_by_its_fingerprint(pool: PgPool) {
        let fx = fixture(&pool).await;
        let one = fx.create_ok("One", "the-same-secret").await;
        let two = fx.create_ok("Two", "the-same-secret").await;
        let three = fx.create_ok("Three", "a-different-secret").await;

        let listed = fx.as_admin("GET", "/credentials", None).await;

        let by_id = |id: &str| {
            listed
                .body
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(by_id(&one)["same_secret_as"], json!([two]));
        assert_eq!(by_id(&two)["same_secret_as"], json!([one]));
        assert_eq!(by_id(&three)["same_secret_as"], json!([]));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn invalid_requests_are_refused_with_the_field_and_a_taken_name_is_a_conflict(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        fx.create_ok("Mail", "ok").await;
        let post = |body: Value| fx.as_admin("POST", "/credentials", Some(body));
        let named = |name: &str| {
            json!({"kind": "custom", "name": name, "schema": token_schema(),
                                        "secret": {"token": "ok"}})
        };

        let unknown_kind = post(json!({"kind": "nope", "name": "A", "secret": {}})).await;
        let no_schema =
            post(json!({"kind": "custom", "name": "A", "secret": {"token": "x"}})).await;
        let schema_on_builtin = post(json!({"kind": "basic", "name": "A", "schema": token_schema(),
                                            "fields": {"username": "u"}, "secret": {"password": "p"}}))
        .await;
        let bad_schema = post(json!({"kind": "custom", "name": "A",
                                     "schema": {"fields": [{"key": "Bad Key", "label": "L", "type": "string"}]},
                                     "secret": {}}))
        .await;
        let secret_in_fields = post(json!({"kind": "basic", "name": "A",
                                           "fields": {"username": "u", "password": "p"},
                                           "secret": {"password": "p"}}))
        .await;
        let field_in_secret = post(json!({"kind": "basic", "name": "A",
                                          "fields": {"username": "u"},
                                          "secret": {"password": "p", "username": "u"}}))
        .await;
        let missing = post(json!({"kind": "smtp", "name": "A", "fields": {}, "secret": {}})).await;
        let wrong_type = post(json!({"kind": "smtp", "name": "A",
                                     "fields": {"host": "h", "port": "587", "tls": "plain", "username": "u"},
                                     "secret": {"password": "p"}}))
        .await;
        let unknown_field = post(json!({"kind": "basic", "name": "A",
                                        "fields": {"username": "u", "extra": 1},
                                        "secret": {"password": "p"}}))
        .await;
        let bad_key = post(json!({"kind": "custom", "name": "A", "key": "Not Valid",
                                  "schema": token_schema(), "secret": {"token": "x"}}))
        .await;
        let bad_name = post(named("  ")).await;
        let taken = post(named("MAIL")).await;
        let extra_key = post(json!({"kind": "custom", "name": "A", "surprise": 1,
                                    "schema": token_schema(), "secret": {"token": "x"}}))
        .await;
        let detail = |reply: &Reply| reply.body["detail"].as_str().unwrap_or_default().to_owned();

        for reply in [
            &unknown_kind,
            &no_schema,
            &schema_on_builtin,
            &bad_schema,
            &secret_in_fields,
            &field_in_secret,
            &missing,
            &wrong_type,
            &unknown_field,
            &bad_key,
            &bad_name,
        ] {
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{:?}",
                reply.body
            );
        }
        assert!(detail(&unknown_kind).contains("kind"));
        assert!(detail(&no_schema).contains("schema"));
        assert!(detail(&schema_on_builtin).contains("schema"));
        assert!(detail(&bad_schema).contains("schema.fields[0].key"));
        assert!(detail(&secret_in_fields).contains("fields.password"));
        assert!(detail(&field_in_secret).contains("secret.username"));
        for expected in [
            "fields.host",
            "fields.port",
            "fields.tls",
            "fields.username",
            "secret.password",
        ] {
            assert!(
                detail(&missing).contains(expected),
                "{expected} in {}",
                detail(&missing)
            );
        }
        assert!(detail(&wrong_type).contains("fields.port"));
        assert!(detail(&wrong_type).contains("fields.tls"));
        assert!(detail(&unknown_field).contains("fields.extra"));
        assert!(detail(&bad_key).contains("key"));
        assert_eq!(taken.status, StatusCode::CONFLICT);
        assert!(extra_key.status.is_client_error());
        let empty_patch = fx
            .as_admin(
                "PATCH",
                &format!(
                    "/credentials/{}",
                    fx.as_admin("GET", "/credentials", None).await.body[0]["id"]
                        .as_str()
                        .unwrap()
                ),
                Some(json!({})),
            )
            .await;
        assert_eq!(empty_patch.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            fx.as_admin("GET", "/credentials", None)
                .await
                .body
                .as_array()
                .unwrap()
                .len(),
            1,
            "a refused create stores nothing"
        );
        let secrets: i64 = sqlx::query_scalar("SELECT count(*) FROM encrypted_secrets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(secrets, 1, "a refused create seals nothing");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_built_in_kind_stores_its_non_secret_fields_and_seals_only_the_secret(pool: PgPool) {
        let fx = fixture(&pool).await;

        let created = fx
            .as_admin(
                "POST",
                "/credentials",
                Some(json!({"kind": "smtp", "name": "Relay",
                            "fields": {"host": "mail.internal.example", "port": 587,
                                       "tls": "starttls", "username": "relay"},
                            "secret": {"password": MARKER}})),
            )
            .await;

        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        assert_eq!(created.body["fields"]["port"], 587);
        assert_eq!(created.body["hints"], json!({"password": "4711"}));
        let id: Uuid = created.body["id"].as_str().unwrap().parse().unwrap();
        let opened = zirv_flow_store::open_credential(&pool, &fx.keyring, fx.ws, id)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(opened.secret.expose()).unwrap(),
            json!({"password": MARKER}),
            "the sealed object holds the secret fields only"
        );
        assert_eq!(opened.fields["host"], "mail.internal.example");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_master_key_rotation_rewraps_credential_secrets_and_they_still_open(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.create_ok("Mail", MARKER).await;
        let uuid: Uuid = id.parse().unwrap();
        let rotated = Keyring::new(
            MasterKey::generate(),
            Some(MasterKey::parse(&fx.keyring.current().to_base64()).unwrap()),
        )
        .unwrap();

        let report = admin::rotate_master_key(&pool, &Context::system(), &rotated)
            .await
            .unwrap();

        assert_eq!(report.rows, 2, "the credential secret and the HMAC root");
        let new_only = Keyring::new(
            MasterKey::parse(&rotated.current().to_base64()).unwrap(),
            None,
        )
        .unwrap();
        let opened = zirv_flow_store::open_credential(&pool, &new_only, fx.ws, uuid)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(opened.secret.expose()).unwrap(),
            json!({"token": MARKER})
        );
        let old_only = zirv_flow_store::open_credential(&pool, &fx.keyring, fx.ws, uuid).await;
        assert!(old_only.is_err(), "the old key no longer opens it");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_imported_credential_keeps_working_and_rotates_with_the_same_field_names(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let secret = zirv_flow_core::SecretId::new();
        let purpose = zirv_flow_core::SecretPurpose::DestinationCredential;
        let sealed = seal(
            &fx.keyring,
            br#"{"token":"ok"}"#,
            &zirv_flow_core::secret_aad(fx.ws, secret, purpose),
        );
        secrets::insert_unchecked(&pool, fx.ws, purpose, secret, &sealed)
            .await
            .unwrap();
        let imported = zirv_flow_store::import_secret_as_credential(
            &pool,
            fx.ws,
            "Old destination credential",
            secret,
            &["token"],
        )
        .await
        .unwrap();
        let id = imported.id.to_string();
        let destination = fx.destination("Legacy", &id, json!({})).await;

        let before = fx.check(&destination).await;
        let read = fx
            .as_admin("GET", &format!("/credentials/{id}"), None)
            .await;
        let extra = fx
            .as_admin(
                "PUT",
                &format!("/credentials/{id}/secret"),
                Some(json!({"secret": {"token": "x", "other": "y"}})),
            )
            .await;
        let rotated = fx.rotate(&id, "not-ok").await;
        let after = fx.check(&destination).await;

        assert_eq!(before["ok"], true, "{before}");
        assert_eq!(read.body["kind"], "imported");
        assert_eq!(read.body["hints"], json!({"token": ""}));
        assert!(read.body["fingerprint"].is_null());
        assert_eq!(extra.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(rotated.status, StatusCode::OK, "{:?}", rotated.body);
        assert!(rotated.body["fingerprint"].is_string());
        assert_eq!(after["code"], "credential_refused");
        let purposes: Vec<String> = sqlx::query_scalar(
            "SELECT purpose FROM encrypted_secrets WHERE id IN (SELECT secret_id FROM credentials)",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            purposes,
            ["workspace_credential"],
            "re-sealed under the new purpose on rotation"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_credential_must_hold_the_fields_the_destination_type_requires(pool: PgPool) {
        let fx = fixture(&pool).await;
        let basic = fx
            .as_admin(
                "POST",
                "/credentials",
                Some(
                    json!({"kind": "basic", "name": "Basic", "fields": {"username": "u"},
                            "secret": {"password": "p"}}),
                ),
            )
            .await;
        assert_eq!(basic.status, StatusCode::CREATED);

        let reply = fx
            .as_admin(
                "POST",
                "/destinations",
                Some(json!({"kind": "fake", "name": "X", "config": {},
                            "credential_id": basic.body["id"]})),
            )
            .await;

        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            reply.body
        );
        assert!(
            reply.body["detail"]
                .as_str()
                .unwrap()
                .contains("credential.token")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_manifest_that_declares_a_credential_schema_gets_the_same_form_and_storage(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let manifest = json!({
            "version": "v1",
            "base_url": "https://api.example.test/v1",
            "auth": {"type": "bearer", "credential": {"fields": [
                {"key": "token", "label": "Access token", "type": "string",
                 "required": true, "secret": true},
                {"key": "account", "label": "Account", "type": "string", "required": true}
            ]}},
            "streams": [{"name": "people", "path": "/people", "records_path": "/data",
                         "fields": [{"name": "id", "type": "string"}], "primary_key": ["id"]}]
        });

        let validated = fx
            .as_admin(
                "POST",
                "/source-manifests/validate",
                Some(json!({"manifest": manifest})),
            )
            .await;

        assert_eq!(validated.status, StatusCode::OK, "{:?}", validated.body);
        assert_eq!(validated.body["valid"], true, "{:?}", validated.body);
        let schema = validated.body["credential_schema"].clone();
        assert_eq!(schema["fields"][0]["secret"], true);
        let created = fx
            .as_admin(
                "POST",
                "/credentials",
                Some(
                    json!({"kind": "custom", "name": "Manifest account", "schema": schema,
                            "fields": {"account": "acme"}, "secret": {"token": MARKER}}),
                ),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        assert_eq!(created.body["schema"], schema, "the form round-trips");
        assert_eq!(created.body["fields"], json!({"account": "acme"}));
    }

    fn declared_manifest() -> Value {
        json!({
            "version": "v1",
            "base_url": "https://api.example.test/v1",
            "auth": {"type": "bearer", "credential": {"fields": [
                {"key": "token", "label": "Access token", "type": "string",
                 "required": true, "secret": true},
                {"key": "account", "label": "Account", "type": "string", "required": true}
            ]}},
            "streams": [{"name": "people", "path": "/people", "records_path": "/data",
                         "fields": [{"name": "id", "type": "string"}], "primary_key": ["id"]}]
        })
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_http_source_must_use_a_credential_that_fits_the_schema_its_manifest_declares(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let body = |credential: Option<&str>| {
            let mut body = json!({"kind": "http", "name": format!("API {}", Uuid::new_v4()),
                                  "config": {"manifest": declared_manifest()}});
            if let Some(id) = credential {
                body["credential_id"] = json!(id);
            }
            body
        };
        let complete = fx
            .as_admin(
                "POST",
                "/credentials",
                Some(json!({"kind": "custom", "name": "Complete", "fields": {"account": "acme"},
                            "schema": {"fields": [
                                {"key": "token", "label": "T", "type": "string", "required": true, "secret": true},
                                {"key": "account", "label": "A", "type": "string", "required": true}]},
                            "secret": {"token": "tok"}})),
            )
            .await;
        let partial = fx.create_ok("Token only", "tok").await;

        let none = fx.as_admin("POST", "/sources", Some(body(None))).await;
        let wrong = fx
            .as_admin("POST", "/sources", Some(body(Some(&partial))))
            .await;
        let fitting = fx
            .as_admin(
                "POST",
                "/sources",
                Some(body(Some(complete.body["id"].as_str().unwrap()))),
            )
            .await;

        assert_eq!(
            none.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            none.body
        );
        assert!(
            none.body["detail"]
                .as_str()
                .unwrap()
                .contains("credential.token")
        );
        assert_eq!(
            wrong.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            wrong.body
        );
        assert!(
            wrong.body["detail"]
                .as_str()
                .unwrap()
                .contains("credential.account")
        );
        assert_eq!(fitting.status, StatusCode::CREATED, "{:?}", fitting.body);
        // An update that moves the source to the incomplete credential is refused too.
        let id = fitting.body["id"].as_str().unwrap();
        let moved = fx
            .as_admin(
                "PATCH",
                &format!("/sources/{id}"),
                Some(json!({"credential_id": partial})),
            )
            .await;
        assert_eq!(
            moved.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            moved.body
        );
        let kept = fx.as_admin("GET", &format!("/sources/{id}"), None).await;
        assert_eq!(kept.body["credential_id"], complete.body["id"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_source_update_share_locks_the_credential_it_keeps_so_a_concurrent_change_is_seen(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let credential = fx
            .as_admin(
                "POST",
                "/credentials",
                Some(json!({"kind": "custom", "name": "Complete", "fields": {"account": "acme"},
                            "schema": {"fields": [
                                {"key": "token", "label": "T", "type": "string", "required": true, "secret": true},
                                {"key": "account", "label": "A", "type": "string", "required": true}]},
                            "secret": {"token": "tok"}})),
            )
            .await;
        let credential_id = credential.body["id"].as_str().unwrap().to_owned();
        let created = fx
            .as_admin(
                "POST",
                "/sources",
                Some(json!({"kind": "http", "name": "API", "config": {"manifest": declared_manifest()},
                            "credential_id": credential_id})),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let source = created.body["id"].as_str().unwrap().to_owned();

        // Another transaction holds the credential's write lock and has dropped the token.
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM credentials WHERE id = $1::uuid FOR UPDATE")
            .bind(&credential_id)
            .execute(&mut *holder)
            .await
            .unwrap();
        sqlx::query("UPDATE credentials SET hints = '{}' WHERE id = $1::uuid")
            .bind(&credential_id)
            .execute(&mut *holder)
            .await
            .unwrap();
        // A save of the source's manifest, which keeps the credential, must wait for it.
        let mut manifest = declared_manifest();
        manifest["rate_limit"] = json!({"requests": 10, "per_seconds": 1});
        let router = fx.router.clone();
        let uri = fx.path(&format!("/sources/{source}"));
        let key = fx.admin.clone();
        let update = tokio::spawn(async move {
            call(
                &router,
                "PATCH",
                &uri,
                Some(json!({"config": {"manifest": manifest}})),
                Auth::Key(&key),
            )
            .await
        });
        crate::test_support::wait_until_a_backend_waits_on_a_lock(&pool).await;
        assert!(
            !update.is_finished(),
            "the save waits for the credential's writer"
        );
        holder.commit().await.unwrap();

        // It then sees the credential without its token and refuses the manifest that needs it.
        let refused = update.await.unwrap();
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            refused.body
        );
        assert!(
            refused.body["detail"]
                .as_str()
                .unwrap()
                .contains("credential.token")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rotation_or_a_patch_cannot_drop_a_field_a_live_owner_requires(pool: PgPool) {
        let fx = fixture(&pool).await;
        // `token` is optional in both credentials, but the fake destination requires it.
        let secret_token = fx
            .as_admin(
                "POST",
                "/credentials",
                Some(json!({"kind": "custom", "name": "Optional secret", "schema": {"fields": [
                    {"key": "token", "label": "T", "type": "string", "secret": true},
                    {"key": "extra", "label": "E", "type": "string", "required": true, "secret": true}]},
                    "secret": {"token": "ok", "extra": "e"}})),
            )
            .await;
        let plain_token = fx
            .as_admin(
                "POST",
                "/credentials",
                Some(json!({"kind": "custom", "name": "Optional plain", "schema": {"fields": [
                    {"key": "token", "label": "T", "type": "string"},
                    {"key": "extra", "label": "E", "type": "string", "required": true, "secret": true}]},
                    "fields": {"token": "ok"}, "secret": {"extra": "e"}})),
            )
            .await;
        let (secret_id, plain_id) = (
            secret_token.body["id"].as_str().unwrap().to_owned(),
            plain_token.body["id"].as_str().unwrap().to_owned(),
        );
        fx.destination("One", &secret_id, json!({})).await;
        fx.destination("Two", &plain_id, json!({})).await;

        let rotate = fx
            .as_admin(
                "PUT",
                &format!("/credentials/{secret_id}/secret"),
                Some(json!({"secret": {"extra": "new"}})),
            )
            .await;
        let patch = fx
            .as_admin(
                "PATCH",
                &format!("/credentials/{plain_id}"),
                Some(json!({"fields": {}})),
            )
            .await;
        let fine = fx
            .as_admin(
                "PUT",
                &format!("/credentials/{secret_id}/secret"),
                Some(json!({"secret": {"token": "ok", "extra": "new"}})),
            )
            .await;

        assert_eq!(
            rotate.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            rotate.body
        );
        assert!(
            rotate.body["detail"]
                .as_str()
                .unwrap()
                .contains("credential.token")
        );
        assert_eq!(
            patch.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            patch.body
        );
        assert!(
            patch.body["detail"]
                .as_str()
                .unwrap()
                .contains("credential.token")
        );
        assert_eq!(fine.status, StatusCode::OK, "{:?}", fine.body);
        let kept = fx
            .as_admin("GET", &format!("/credentials/{plain_id}"), None)
            .await;
        assert_eq!(
            kept.body["fields"],
            json!({"token": "ok"}),
            "a refused change stores nothing"
        );
        let secrets: i64 = sqlx::query_scalar("SELECT count(*) FROM encrypted_secrets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(secrets, 2, "a refused rotation sealed nothing");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_built_in_username_and_password_credential_can_be_shared_with_database_sources(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let basic = fx
            .as_admin(
                "POST",
                "/credentials",
                Some(json!({"kind": "basic", "name": "Warehouse login",
                            "fields": {"username": "reader"}, "secret": {"password": "pw"}})),
            )
            .await;
        assert_eq!(basic.status, StatusCode::CREATED, "{:?}", basic.body);
        let basic_id = basic.body["id"].as_str().unwrap().to_owned();
        let config = json!({"host": "db.example.com", "database": "shop"});

        for kind in ["postgres", "mysql"] {
            let created = fx
                .as_admin(
                    "POST",
                    "/sources",
                    Some(
                        json!({"kind": kind, "name": format!("{kind} one"), "config": config,
                                "credential_id": basic_id}),
                    ),
                )
                .await;
            assert_eq!(
                created.status,
                StatusCode::CREATED,
                "{kind}: {:?}",
                created.body
            );
            assert_eq!(
                created.body["credential_fields_set"],
                json!(["password", "username"])
            );

            // An update that moves a source onto the credential is accepted too.
            let bare = fx
                .as_admin(
                    "POST",
                    "/credentials",
                    Some(json!({"kind": "custom", "name": format!("{kind} sealed"),
                                "schema": {"fields": [
                                    {"key": "username", "label": "U", "type": "string", "required": true, "secret": true},
                                    {"key": "password", "label": "P", "type": "string", "required": true, "secret": true}]},
                                "secret": {"username": "other", "password": "pw"}})),
                )
                .await;
            let other = fx
                .as_admin(
                    "POST",
                    "/sources",
                    Some(
                        json!({"kind": kind, "name": format!("{kind} two"), "config": config,
                                "credential_id": bare.body["id"]}),
                    ),
                )
                .await;
            assert_eq!(
                other.status,
                StatusCode::CREATED,
                "{kind}: {:?}",
                other.body
            );
            let moved = fx
                .as_admin(
                    "PATCH",
                    &format!("/sources/{}", other.body["id"].as_str().unwrap()),
                    Some(json!({"credential_id": basic_id})),
                )
                .await;
            assert_eq!(moved.status, StatusCode::OK, "{kind}: {:?}", moved.body);
        }

        // What the source opens carries the user under the key the source reads.
        let opened =
            zirv_flow_store::open_credential(&pool, &fx.keyring, fx.ws, basic_id.parse().unwrap())
                .await
                .unwrap();
        let credential =
            zirv_flow_connectors::sources::Credential::from_parts(&opened.secret, &opened.fields)
                .unwrap();
        assert_eq!(credential.get("username").unwrap().expose(), "reader");
        assert_eq!(credential.get("password").unwrap().expose(), "pw");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_new_built_in_kinds_fit_the_owner_types_that_read_their_fields(pool: PgPool) {
        let fx = fixture(&pool).await;
        let make = |body: Value| {
            let fx = &fx;
            async move {
                let reply = fx.as_admin("POST", "/credentials", Some(body)).await;
                assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
                reply.body["id"].as_str().unwrap().to_owned()
            }
        };
        let source = |kind: &str, config: Value, credential: &str| {
            let fx = &fx;
            let body = json!({"kind": kind, "name": format!("{kind} {credential}"),
                              "config": config, "credential_id": credential});
            async move { fx.as_admin("POST", "/sources", Some(body)).await }
        };
        let bearer = make(json!({"kind": "bearer_token", "name": "Feed token",
                                 "fields": {}, "secret": {"token": "tok"}}))
        .await;
        let tls = make(json!({"kind": "tls_client", "name": "Broker identity",
                              "fields": {"tls_client_cert": "-----BEGIN CERTIFICATE-----"},
                              "secret": {"tls_client_key": "-----BEGIN PRIVATE KEY-----"}}))
        .await;
        let oauth = make(json!({"kind": "oauth2_client", "name": "App client",
                                "fields": {"client_id": "app"},
                                "secret": {"client_secret": "shh"}}))
        .await;
        let signing = make(json!({"kind": "signing_secret", "name": "Hook signing",
                                  "fields": {},
                                  "secret": {"signing_secret": "now", "previous_signing_secret": "old"}}))
        .await;
        let manifest = |auth: Value| {
            json!({"version": "v1", "base_url": "https://api.example.test/v1", "auth": auth,
                   "streams": [{"name": "people", "path": "/people", "records_path": "/data",
                                "fields": [{"name": "id", "type": "string"}],
                                "primary_key": ["id"]}]})
        };

        let file = source(
            "file",
            json!({"url": "https://files.example.com/people.csv", "format": "csv"}),
            &bearer,
        )
        .await;
        let kafka = source(
            "kafka",
            json!({"brokers": "kafka-1.example.com:9093", "topics": "people"}),
            &tls,
        )
        .await;
        let http_bearer = source(
            "http",
            json!({"manifest": manifest(json!({"type": "bearer"}))}),
            &bearer,
        )
        .await;
        let http_oauth = source(
            "http",
            json!({"manifest": manifest(json!({
                "type": "oauth2_client_credentials",
                "token_url": "https://auth.example.test/token"}))}),
            &oauth,
        )
        .await;
        let hook = fx
            .as_admin(
                "POST",
                "/destinations",
                Some(json!({"kind": "webhook", "name": "Hook",
                            "config": {"url": "https://hooks.example.com/in"},
                            "credential_id": signing})),
            )
            .await;

        for (name, reply) in [
            ("file", &file),
            ("kafka", &kafka),
            ("http bearer", &http_bearer),
            ("http oauth", &http_oauth),
            ("webhook", &hook),
        ] {
            assert_eq!(
                reply.status,
                StatusCode::CREATED,
                "{name}: {:?}",
                reply.body
            );
        }
        assert_eq!(
            hook.body["credential_fields_set"],
            json!(["previous_signing_secret", "signing_secret"])
        );
    }

    const PROVIDER: &str = include_str!(
        "../../../zirv-flow-connectors/tests/fixtures/providers/api_key_body_only.json"
    );

    /// A provider manifest with the given auth and credential fields (key, required), whose
    /// feedback is verified under `secret_field`.
    fn provider_manifest(auth: &str, fields: &[(&str, bool)], secret_field: &str) -> Value {
        let mut manifest: Value = serde_json::from_str(PROVIDER).unwrap();
        let fields: Vec<Value> = fields
            .iter()
            .map(|(key, required)| {
                json!({"key": key, "label": key, "type": "string",
                       "required": required, "secret": true})
            })
            .collect();
        manifest["auth"] = json!({"type": auth, "credential": {"fields": fields}});
        manifest["destination"]["feedback"]["verification"]["secret_field"] = json!(secret_field);
        manifest
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_credential_with_only_the_fields_a_provider_manifest_declares_fits_the_destination(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let manifest = provider_manifest("none", &[("signing_secret", true)], "signing_secret");
        let credential = fx
            .as_admin(
                "POST",
                "/credentials",
                Some(json!({"kind": "custom", "name": "Feedback only",
                            "schema": {"fields": [
                                {"key": "signing_secret", "label": "S", "type": "string",
                                 "required": true, "secret": true}]},
                            "secret": {"signing_secret": "sig"}})),
            )
            .await;
        assert_eq!(
            credential.status,
            StatusCode::CREATED,
            "{:?}",
            credential.body
        );

        let created = fx
            .as_admin(
                "POST",
                "/destinations",
                Some(json!({"kind": "provider", "name": "Mail",
                            "config": {"manifest": manifest},
                            "credential_id": credential.body["id"]})),
            )
            .await;

        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rotation_that_drops_a_field_a_provider_manifest_requires_is_refused(pool: PgPool) {
        let fx = fixture(&pool).await;
        let manifest = provider_manifest(
            "basic",
            &[
                ("username", true),
                ("password", true),
                ("hook_secret", true),
            ],
            "hook_secret",
        );
        let credential = fx
            .as_admin(
                "POST",
                "/credentials",
                Some(json!({"kind": "custom", "name": "Basic mail",
                            "schema": {"fields": [
                                {"key": "username", "label": "U", "type": "string", "secret": true},
                                {"key": "password", "label": "P", "type": "string", "secret": true},
                                {"key": "hook_secret", "label": "H", "type": "string", "secret": true},
                                {"key": "token", "label": "T", "type": "string", "secret": true}]},
                            "secret": {"username": "u", "password": "p", "hook_secret": "h"}})),
            )
            .await;
        assert_eq!(
            credential.status,
            StatusCode::CREATED,
            "{:?}",
            credential.body
        );
        let id = credential.body["id"].as_str().unwrap().to_owned();
        let created = fx
            .as_admin(
                "POST",
                "/destinations",
                Some(json!({"kind": "provider", "name": "Mail",
                            "config": {"manifest": manifest}, "credential_id": id})),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let rotate = |secret: Value| {
            let fx = &fx;
            let path = format!("/credentials/{id}/secret");
            async move {
                fx.as_admin("PUT", &path, Some(json!({"secret": secret})))
                    .await
            }
        };

        let refused = rotate(json!({"token": "t"})).await;
        let kept = rotate(json!({"username": "u2", "password": "p2", "hook_secret": "h2"})).await;

        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            refused.body
        );
        let detail = refused.body["detail"].as_str().unwrap();
        assert!(detail.contains("credential.username"), "{detail}");
        assert!(detail.contains("required by the manifest"), "{detail}");
        assert_eq!(kept.status, StatusCode::OK, "{:?}", kept.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_credential_needs_the_master_key(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let router = router(AppState::new(pool.clone(), Arc::new(String::new)));
        let key = admin::create_api_key(
            &pool,
            &Context::system(),
            ws,
            "k",
            &[Scope::Admin, Scope::Read],
        )
        .await
        .unwrap()
        .plaintext;

        let reply = call(
            &router,
            "POST",
            &format!("/api/v1/workspaces/{}/credentials", ws.as_uuid()),
            Some(
                json!({"kind": "custom", "name": "Mail", "schema": token_schema(),
                        "secret": {"token": "ok"}}),
            ),
            Auth::Key(&key),
        )
        .await;

        assert_eq!(
            reply.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{:?}",
            reply.body
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM credentials")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }
}
