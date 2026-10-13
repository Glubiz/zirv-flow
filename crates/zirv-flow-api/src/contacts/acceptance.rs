//! The contacts API end to end, driven through the router.

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
        HmacPurpose, HmacRoot, MasterKey, Role, Scope, Slug, WorkspaceId, WorkspaceName,
    };

    use crate::admin::{self, Context};
    use crate::{AppState, IngestLimits, router};

    const PASSWORD: &str = "correct horse battery";

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Value,
    }

    #[derive(Clone)]
    struct Login {
        cookie: String,
        csrf: String,
    }

    enum Auth<'a> {
        Key(&'a str),
        Session(&'a Login),
        SessionWithoutCsrf(&'a Login),
    }

    struct Fixture {
        router: Router,
        ws: WorkspaceId,
        read: String,
        write: String,
        admin: String,
    }

    async fn call(
        router: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
        auth: Auth<'_>,
        extra: &[(&str, &str)],
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(uri);
        request = match auth {
            Auth::Key(key) => request.header(header::AUTHORIZATION, format!("Bearer {key}")),
            Auth::Session(login) => request
                .header(header::COOKIE, &login.cookie)
                .header("x-csrf-token", &login.csrf),
            Auth::SessionWithoutCsrf(login) => request.header(header::COOKIE, &login.cookie),
        };
        for (name, value) in extra {
            request = request.header(*name, *value);
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
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    async fn key(pool: &PgPool, ws: WorkspaceId, scopes: &[Scope]) -> String {
        admin::create_api_key(pool, &Context::system(), ws, "contacts", scopes)
            .await
            .unwrap()
            .plaintext
    }

    async fn fixture_with(pool: &PgPool, limits: IngestLimits) -> Fixture {
        zirv_flow_store::migrate(pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let state = AppState::new(pool.clone(), Arc::new(String::new))
            .with_hmac_root(&HmacRoot::bootstrap_from(&MasterKey::generate()))
            .with_ingest_limits(limits);
        Fixture {
            router: router(state),
            ws,
            read: key(pool, ws, &[Scope::Read]).await,
            write: key(pool, ws, &[Scope::Read, Scope::Write]).await,
            admin: key(pool, ws, &[Scope::Admin, Scope::Read, Scope::Write]).await,
        }
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        fixture_with(pool, IngestLimits::default()).await
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
            key: &str,
            extra: &[(&str, &str)],
        ) -> Reply {
            call(
                &self.router,
                method,
                &self.path(tail),
                body,
                Auth::Key(key),
                extra,
            )
            .await
        }

        /// Registers the fields and saves the definition: `email` (required identifier),
        /// `age` and a PII-flagged `ssn`.
        async fn define(&self) {
            for (key, kind, pii) in [
                ("email", "string", false),
                ("age", "number", false),
                ("ssn", "string", true),
            ] {
                let reply = self
                    .send(
                        "POST",
                        "/traits",
                        Some(json!({"key": key, "type": kind, "pii": pii})),
                        &self.admin,
                        &[],
                    )
                    .await;
                assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            }
            let reply = self
                .send(
                    "PUT",
                    "/contact-definition",
                    Some(json!({"fields": [
                        {"key": "email", "required": true, "identifier": "email"},
                        {"key": "age"},
                        {"key": "ssn"}
                    ]})),
                    &self.admin,
                    &[],
                )
                .await;
            assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        }

        async fn create(&self, key: &str, fields: Value) -> Reply {
            self.send(
                "POST",
                "/contacts",
                Some(json!({"key": key, "fields": fields})),
                &self.write,
                &[],
            )
            .await
        }
    }

    /// Every table a contact write may touch, as comparable text.
    async fn dump(pool: &PgPool) -> Vec<Vec<String>> {
        let mut state = Vec::new();
        for table in [
            "people",
            "identities",
            "source_records",
            "person_changes",
            "decision_records",
            "contact_idempotency",
            "audit_log",
            "ingest_inbox",
        ] {
            state.push(
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT to_jsonb(t)::text FROM {table} t ORDER BY 1"
                )))
                .fetch_all(pool)
                .await
                .unwrap(),
            );
        }
        state
    }

    async fn audited(pool: &PgPool) -> Vec<String> {
        sqlx::query_scalar("SELECT action FROM audit_log WHERE action LIKE 'contact%' ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn member_login(pool: &PgPool, router: &Router, ws: WorkspaceId, role: Role) -> Login {
        let email = format!("{}@example.com", uuid::Uuid::now_v7().simple());
        let user = admin::create_user(pool, &Context::system(), &email, PASSWORD)
            .await
            .unwrap();
        admin::set_member(pool, &Context::system(), ws, user.id, role)
            .await
            .unwrap();
        let response = router
            .clone()
            .oneshot(
                Request::post("/api/v1/session")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"email": email, "password": PASSWORD}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
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

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_contact_is_created_read_patched_replaced_and_deleted(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.define().await;

        let created = fx
            .send(
                "POST",
                "/contacts",
                Some(json!({"key": "c-1", "fields": {"email": "A@Example.com", "age": 30}})),
                &fx.admin,
                &[],
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        assert_eq!(created.body["key"], "c-1");
        assert_eq!(
            created.body["fields"],
            json!({"email": "A@Example.com", "age": 30})
        );
        let person = created.body["person_id"].as_str().unwrap().to_owned();

        let read = fx.send("GET", "/contacts/c-1", None, &fx.admin, &[]).await;
        assert_eq!(read.status, StatusCode::OK);
        assert_eq!(read.body["fields"], created.body["fields"]);

        let patched = fx
            .send(
                "PATCH",
                "/contacts/c-1",
                Some(json!({"fields": {"age": 31, "ssn": "123"}})),
                &fx.admin,
                &[],
            )
            .await;
        assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);
        assert_eq!(patched.body["person_id"], person.as_str());
        let removed = fx
            .send(
                "PATCH",
                "/contacts/c-1",
                Some(json!({"fields": {"ssn": null}})),
                &fx.admin,
                &[],
            )
            .await;
        assert_eq!(
            removed.body["fields"],
            json!({"email": "A@Example.com", "age": 31})
        );

        let replaced = fx
            .send(
                "PUT",
                "/contacts/c-1",
                Some(json!({"fields": {"email": "A@Example.com"}})),
                &fx.admin,
                &[],
            )
            .await;
        assert_eq!(replaced.status, StatusCode::OK);
        assert_eq!(replaced.body["fields"], json!({"email": "A@Example.com"}));

        let profile = fx
            .send("GET", &format!("/people/{person}"), None, &fx.admin, &[])
            .await;
        assert_eq!(profile.status, StatusCode::OK);

        let deleted = fx
            .send("DELETE", "/contacts/c-1", None, &fx.admin, &[])
            .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        let gone = fx.send("GET", "/contacts/c-1", None, &fx.admin, &[]).await;
        assert_eq!(gone.status, StatusCode::NOT_FOUND);
        assert_eq!(
            audited(&pool).await,
            [
                "contact_definition.updated",
                "contact.created",
                "contact.updated",
                "contact.updated",
                "contact.updated",
                "contact.deleted"
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_missing_required_field_is_a_422_naming_it_and_writes_nothing(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.define().await;
        let before = dump(&pool).await;

        let missing = fx.create("c-1", json!({"age": 3})).await;
        let wrong = fx
            .create(
                "c-2",
                json!({"email": "a@example.com", "age": "secret-age", "extra": 1}),
            )
            .await;

        assert_eq!(missing.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(missing.body["detail"], "invalid fields: email");
        assert_eq!(
            missing.body["errors"],
            json!([{"index": 0, "field": "email", "reason": "missing_required"}])
        );
        assert_eq!(
            wrong.body["errors"],
            json!([
                {"index": 0, "field": "age", "reason": "wrong_type"},
                {"index": 0, "field": "extra", "reason": "unplanned_property"}
            ])
        );
        assert!(!wrong.body.to_string().contains("secret-age"));
        assert_eq!(dump(&pool).await, before);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_duplicate_create_is_a_409_and_a_missing_contact_is_a_404(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.create("c-1", json!({"plan": "pro"})).await;

        let again = fx.create("c-1", json!({"plan": "pro"})).await;
        let put = fx
            .send(
                "PUT",
                "/contacts/nope",
                Some(json!({"fields": {}})),
                &fx.write,
                &[],
            )
            .await;
        let reserved = fx.create("search", json!({})).await;

        assert_eq!(again.status, StatusCode::CONFLICT);
        assert_eq!(put.status, StatusCode::NOT_FOUND);
        assert_eq!(reserved.status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_same_idempotency_key_replays_the_response_and_changes_no_row(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.define().await;
        let body = json!({"key": "c-1", "fields": {"email": "a@example.com", "age": 4}});
        let headers = [("idempotency-key", "req-1")];
        let first = fx
            .send("POST", "/contacts", Some(body.clone()), &fx.write, &headers)
            .await;
        assert_eq!(first.status, StatusCode::CREATED);
        assert!(first.headers.get("idempotent-replayed").is_none());
        let before = dump(&pool).await;

        let replay = fx
            .send("POST", "/contacts", Some(body.clone()), &fx.write, &headers)
            .await;

        assert_eq!(replay.status, StatusCode::CREATED);
        assert_eq!(replay.headers["idempotent-replayed"], "true");
        assert_eq!(replay.body, first.body);
        assert_eq!(dump(&pool).await, before);

        // The same request in another key order is the same request.
        let reordered = json!({"fields": {"age": 4, "email": "a@example.com"}, "key": "c-1"});
        let again = fx
            .send("POST", "/contacts", Some(reordered), &fx.write, &headers)
            .await;
        assert_eq!(again.status, StatusCode::CREATED);
        assert_eq!(dump(&pool).await, before);

        // Another request under the same key is refused, and changes nothing.
        let reused = fx
            .send(
                "POST",
                "/contacts",
                Some(json!({"key": "c-2", "fields": {"email": "b@example.com"}})),
                &fx.write,
                &headers,
            )
            .await;
        assert_eq!(reused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(reused.body["code"], "idempotency_key_reused");
        assert_eq!(dump(&pool).await, before);

        // Once the contact is gone the replay is a 404.
        fx.send("DELETE", "/contacts/c-1", None, &fx.write, &[])
            .await;
        let gone = fx
            .send("POST", "/contacts", Some(body), &fx.write, &headers)
            .await;
        assert_eq!(gone.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_delete_repeated_with_its_key_answers_204_again(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.create("c-1", json!({})).await;
        let headers = [("idempotency-key", "del-1")];
        let first = fx
            .send("DELETE", "/contacts/c-1", None, &fx.write, &headers)
            .await;
        let before = dump(&pool).await;

        let replay = fx
            .send("DELETE", "/contacts/c-1", None, &fx.write, &headers)
            .await;
        let without = fx
            .send("DELETE", "/contacts/c-1", None, &fx.write, &[])
            .await;

        assert_eq!(first.status, StatusCode::NO_CONTENT);
        assert_eq!(replay.status, StatusCode::NO_CONTENT);
        assert_eq!(replay.headers["idempotent-replayed"], "true");
        assert_eq!(without.status, StatusCode::NOT_FOUND);
        assert_eq!(dump(&pool).await, before);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_invalid_idempotency_key_is_a_422(pool: PgPool) {
        let fx = fixture(&pool).await;

        let long = "k".repeat(256);
        let reply = fx
            .send(
                "POST",
                "/contacts",
                Some(json!({"key": "c-1"})),
                &fx.write,
                &[("idempotency-key", long.as_str())],
            )
            .await;

        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(count(&pool, "source_records").await, 0);
    }

    async fn count(pool: &PgPool, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn identifier_and_pii_fields_are_masked_for_members_and_read_keys(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.define().await;
        fx.create(
            "c-1",
            json!({"email": "a@example.com", "age": 5, "ssn": "123"}),
        )
        .await;
        let member = member_login(&pool, &fx.router, fx.ws, Role::Member).await;
        let admin_login = member_login(&pool, &fx.router, fx.ws, Role::Admin).await;
        let path = fx.path("/contacts/c-1");

        let by_read = fx.send("GET", "/contacts/c-1", None, &fx.read, &[]).await;
        let by_admin = fx.send("GET", "/contacts/c-1", None, &fx.admin, &[]).await;
        let by_member = call(&fx.router, "GET", &path, None, Auth::Session(&member), &[]).await;
        let by_workspace_admin = call(
            &fx.router,
            "GET",
            &path,
            None,
            Auth::Session(&admin_login),
            &[],
        )
        .await;
        let listed = fx.send("GET", "/contacts", None, &fx.read, &[]).await;

        for masked in [&by_read, &by_member] {
            assert_eq!(
                masked.body["fields"],
                json!({"email": null, "age": 5, "ssn": null})
            );
            assert_eq!(masked.body["masked_fields"], json!(["email", "ssn"]));
        }
        for shown in [&by_admin, &by_workspace_admin] {
            assert_eq!(shown.body["fields"]["email"], "a@example.com");
            assert_eq!(shown.body["masked_fields"], json!([]));
        }
        assert_eq!(listed.body["contacts"][0]["fields"]["email"], Value::Null);
        assert!(!listed.body.to_string().contains("a@example.com"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn scopes_roles_and_csrf_gate_the_endpoints(pool: PgPool) {
        let fx = fixture(&pool).await;
        let ingest = key(&pool, fx.ws, &[Scope::Ingest]).await;
        let member = member_login(&pool, &fx.router, fx.ws, Role::Member).await;
        let path = fx.path("/contacts");
        let post = |auth| {
            call(
                &fx.router,
                "POST",
                &path,
                Some(json!({"key": "m-1"})),
                auth,
                &[],
            )
        };

        let read_key = fx
            .send(
                "POST",
                "/contacts",
                Some(json!({"key": "x"})),
                &fx.read,
                &[],
            )
            .await;
        let ingest_key = fx
            .send("POST", "/contacts", Some(json!({"key": "x"})), &ingest, &[])
            .await;
        let no_csrf = post(Auth::SessionWithoutCsrf(&member)).await;
        let member_ok = post(Auth::Session(&member)).await;
        let member_definition = call(
            &fx.router,
            "PUT",
            &fx.path("/contact-definition"),
            Some(json!({"fields": []})),
            Auth::Session(&member),
            &[],
        )
        .await;
        let write_definition = fx
            .send(
                "PUT",
                "/contact-definition",
                Some(json!({"fields": []})),
                &fx.write,
                &[],
            )
            .await;

        assert_eq!(read_key.status, StatusCode::FORBIDDEN);
        assert_eq!(ingest_key.status, StatusCode::FORBIDDEN);
        assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
        assert_eq!(member_ok.status, StatusCode::CREATED);
        assert_eq!(member_definition.status, StatusCode::FORBIDDEN);
        assert_eq!(write_definition.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_definition_is_admin_only_audited_and_checked_against_the_registry(pool: PgPool) {
        let fx = fixture(&pool).await;
        let undefined = fx
            .send("GET", "/contact-definition", None, &fx.read, &[])
            .await;
        assert_eq!(
            undefined.body,
            json!({"defined": false, "fields": [], "updated_at": null, "updated_by": null})
        );
        fx.send(
            "POST",
            "/traits",
            Some(json!({"key": "age", "type": "number"})),
            &fx.admin,
            &[],
        )
        .await;

        let unknown = fx
            .send(
                "PUT",
                "/contact-definition",
                Some(json!({"fields": [{"key": "age"}, {"key": "nope"}]})),
                &fx.admin,
                &[],
            )
            .await;
        let numeric_identifier = fx
            .send(
                "PUT",
                "/contact-definition",
                Some(json!({"fields": [{"key": "age", "identifier": "external:age"}]})),
                &fx.admin,
                &[],
            )
            .await;
        let duplicate = fx
            .send(
                "PUT",
                "/contact-definition",
                Some(json!({"fields": [{"key": "age"}, {"key": "age"}]})),
                &fx.admin,
                &[],
            )
            .await;
        let saved = fx
            .send(
                "PUT",
                "/contact-definition",
                Some(json!({"fields": [{"key": "age", "required": true}]})),
                &fx.admin,
                &[],
            )
            .await;
        let read = fx
            .send("GET", "/contact-definition", None, &fx.read, &[])
            .await;

        assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            unknown.body["errors"],
            json!([{"index": 1, "field": "nope", "reason": "unknown_key"}])
        );
        assert_eq!(
            numeric_identifier.body["errors"][0]["reason"],
            "identifier_needs_string"
        );
        assert_eq!(duplicate.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(saved.status, StatusCode::OK);
        assert_eq!(read.body["defined"], true);
        assert_eq!(
            read.body["fields"],
            json!([{"key": "age", "required": true, "identifier": null}])
        );
        assert_eq!(audited(&pool).await, ["contact_definition.updated"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_bulk_upsert_is_a_202_that_drains_to_the_state_single_writes_make(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.define().await;
        let contacts = json!([
            {"key": "b-1", "fields": {"email": "a@example.com", "age": 1}},
            {"key": "b-2", "fields": {"email": "b@example.com"}}
        ]);

        let reply = fx
            .send(
                "POST",
                "/contacts/bulk",
                Some(json!({"contacts": contacts})),
                &fx.write,
                &[],
            )
            .await;

        assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.body);
        assert_eq!(reply.body, json!({"accepted": 2}));
        assert_eq!(count(&pool, "ingest_inbox").await, 2);
        let kinds: Vec<String> = sqlx::query_scalar("SELECT DISTINCT kind FROM ingest_inbox")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(kinds, ["contact"]);
        let not_yet = fx.send("GET", "/contacts/b-1", None, &fx.admin, &[]).await;
        assert_eq!(not_yet.status, StatusCode::NOT_FOUND);

        let hmac =
            HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        zirv_flow_store::drain_batch(&pool, fx.ws, zirv_flow_store::Shard::ALL, &hmac, 100)
            .await
            .unwrap();

        let drained = fx.send("GET", "/contacts/b-1", None, &fx.admin, &[]).await;
        assert_eq!(
            drained.body["fields"],
            json!({"email": "a@example.com", "age": 1})
        );
        let single = fx
            .create("s-1", json!({"email": "c@example.com", "age": 1}))
            .await;
        assert_eq!(
            single.body["fields"].as_object().unwrap().len(),
            drained.body["fields"].as_object().unwrap().len()
        );
        assert_eq!(count(&pool, "ingest_inbox").await, 0);
        assert_eq!(audited(&pool).await.last().unwrap(), "contact.created");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_invalid_bulk_is_a_422_listing_every_item_and_appends_nothing(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.define().await;
        let before = count(&pool, "ingest_inbox").await;

        let reply = fx
            .send(
                "POST",
                "/contacts/bulk",
                Some(json!({"contacts": [
                    {"key": "ok", "fields": {"email": "a@example.com"}},
                    {"key": "bad", "fields": {"age": "secret-age"}}
                ]})),
                &fx.write,
                &[],
            )
            .await;
        let empty = fx
            .send(
                "POST",
                "/contacts/bulk",
                Some(json!({"contacts": []})),
                &fx.write,
                &[],
            )
            .await;
        let by_read = fx
            .send(
                "POST",
                "/contacts/bulk",
                Some(json!({"contacts": [{"key": "ok"}]})),
                &fx.read,
                &[],
            )
            .await;

        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            reply.body["errors"],
            json!([
                {"index": 1, "field": "email", "reason": "missing_required"},
                {"index": 1, "field": "age", "reason": "wrong_type"}
            ])
        );
        assert!(!reply.body.to_string().contains("secret-age"));
        assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(by_read.status, StatusCode::FORBIDDEN);
        assert_eq!(count(&pool, "ingest_inbox").await, before);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn bulk_shares_the_ingest_rate_limit_and_size_limits(pool: PgPool) {
        let limits = IngestLimits {
            max_request_bytes: 400,
            max_batch_items: 2,
            rate_per_key: 1,
            burst_per_key: 2,
            ..IngestLimits::default()
        };
        let fx = fixture_with(&pool, limits).await;
        let two = json!({"contacts": [{"key": "a"}, {"key": "b"}]});

        let first = fx
            .send("POST", "/contacts/bulk", Some(two.clone()), &fx.write, &[])
            .await;
        let limited = fx
            .send("POST", "/contacts/bulk", Some(two), &fx.write, &[])
            .await;
        let too_many = fx
            .send(
                "POST",
                "/contacts/bulk",
                Some(json!({"contacts": [{"key": "a"}, {"key": "b"}, {"key": "c"}]})),
                &fx.admin,
                &[],
            )
            .await;
        let big = fx
            .send(
                "POST",
                "/contacts/bulk",
                Some(json!({"contacts": [{"key": "x".repeat(500)}]})),
                &fx.admin,
                &[],
            )
            .await;

        assert_eq!(first.status, StatusCode::ACCEPTED);
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.headers.contains_key(header::RETRY_AFTER));
        assert_eq!(too_many.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(big.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_session_may_bulk_upsert_with_csrf(pool: PgPool) {
        let fx = fixture(&pool).await;
        let member = member_login(&pool, &fx.router, fx.ws, Role::Member).await;
        let body = json!({"contacts": [{"key": "a"}]});

        let accepted = call(
            &fx.router,
            "POST",
            &fx.path("/contacts/bulk"),
            Some(body.clone()),
            Auth::Session(&member),
            &[],
        )
        .await;
        let no_csrf = call(
            &fx.router,
            "POST",
            &fx.path("/contacts/bulk"),
            Some(body),
            Auth::SessionWithoutCsrf(&member),
            &[],
        )
        .await;

        assert_eq!(accepted.status, StatusCode::ACCEPTED);
        assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn contacts_are_paged_by_key_and_found_by_key_prefix_email_and_phone(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.define().await;
        for (key, email) in [
            ("b", "b@example.com"),
            ("a2", "a2@example.com"),
            ("a1", "a1@example.com"),
        ] {
            fx.create(key, json!({"email": email})).await;
        }
        fx.send(
            "POST",
            "/traits",
            Some(json!({"key": "phone_number", "type": "string"})),
            &fx.admin,
            &[],
        )
        .await;

        let first = fx
            .send("GET", "/contacts?limit=2", None, &fx.read, &[])
            .await;
        let cursor = first.body["next_after"].as_str().unwrap().to_owned();
        let second = fx
            .send(
                "GET",
                &format!("/contacts?limit=2&after={cursor}"),
                None,
                &fx.read,
                &[],
            )
            .await;
        let by_prefix = fx
            .send("GET", "/contacts/search?q=a", None, &fx.read, &[])
            .await;
        let by_email_get = fx
            .send(
                "GET",
                "/contacts/search?q=B%40Example.com",
                None,
                &fx.admin,
                &[],
            )
            .await;
        let by_identifier = fx
            .send(
                "POST",
                "/contacts/search",
                Some(json!({"identifier": {"kind": "email", "value": "A1@example.com"}})),
                &fx.admin,
                &[],
            )
            .await;
        let by_q = fx
            .send(
                "POST",
                "/contacts/search",
                Some(json!({"q": "b@example.com"})),
                &fx.read,
                &[],
            )
            .await;
        let both = fx
            .send(
                "POST",
                "/contacts/search",
                Some(json!({"q": "a", "identifier": {"kind": "email", "value": "a@b.co"}})),
                &fx.read,
                &[],
            )
            .await;
        let invalid = fx
            .send(
                "POST",
                "/contacts/search",
                Some(json!({"identifier": {"kind": "email", "value": "not-an-address"}})),
                &fx.read,
                &[],
            )
            .await;
        let bad_limit = fx
            .send("GET", "/contacts?limit=500", None, &fx.read, &[])
            .await;

        let keys = |reply: &Reply, field: &str| -> Vec<String> {
            reply.body[field]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["key"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(keys(&first, "contacts"), ["a1", "a2"]);
        assert_eq!(keys(&second, "contacts"), ["b"]);
        assert_eq!(second.body["next_after"], Value::Null);
        assert_eq!(keys(&by_prefix, "contacts"), ["a1", "a2"]);
        assert_eq!(keys(&by_email_get, "contacts"), ["b"]);
        assert_eq!(keys(&by_identifier, "contacts"), ["a1"]);
        assert_eq!(keys(&by_q, "contacts"), ["b"]);
        assert_eq!(both.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(invalid.body["detail"], "identifier: invalid_email");
        assert_eq!(bad_limit.status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_contact_of_another_workspace_is_a_404(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.create("c-1", json!({})).await;
        let name = WorkspaceName::parse("theirs").unwrap();
        let slug = Slug::parse("theirs").unwrap();
        let other = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let their_key = key(&pool, other, &[Scope::Read, Scope::Write]).await;

        let read = call(
            &fx.router,
            "GET",
            &format!("/api/v1/workspaces/{}/contacts/c-1", other.as_uuid()),
            None,
            Auth::Key(&their_key),
            &[],
        )
        .await;
        let cross = call(
            &fx.router,
            "GET",
            &fx.path("/contacts/c-1"),
            None,
            Auth::Key(&their_key),
            &[],
        )
        .await;

        assert_eq!(read.status, StatusCode::NOT_FOUND);
        assert_eq!(cross.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_bulk_without_a_definition_checks_the_registry_like_a_single_write(pool: PgPool) {
        let fx = fixture(&pool).await;
        let registered = fx
            .send(
                "POST",
                "/traits",
                Some(json!({"key": "age", "type": "number"})),
                &fx.admin,
                &[],
            )
            .await;
        assert_eq!(registered.status, StatusCode::CREATED);
        let before = count(&pool, "ingest_inbox").await;

        let bulk = fx
            .send(
                "POST",
                "/contacts/bulk",
                Some(json!({"contacts": [{"key": "c", "fields": {"age": "bad"}}]})),
                &fx.write,
                &[],
            )
            .await;
        let single = fx.create("c", json!({"age": "bad"})).await;

        assert_eq!(
            bulk.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            bulk.body
        );
        assert_eq!(
            bulk.body["errors"],
            json!([{"index": 0, "field": "age", "reason": "wrong_type"}])
        );
        assert_eq!(single.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(count(&pool, "ingest_inbox").await, before);
    }
}
