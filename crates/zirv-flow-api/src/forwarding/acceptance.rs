//! The forwarding API end to end, driven through the router against the fake destination.

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use std::sync::Arc;
    use tower::ServiceExt;
    use uuid::Uuid;
    use zirv_flow_core::{Role, Scope, Slug, WorkspaceId, WorkspaceName};

    use crate::admin::{self, Context};
    use crate::{AppState, router};

    const PASSWORD: &str = "correct horse battery";
    /// Distinct from anything else a response or an audit entry may hold.
    const MARKER: &str = "zz-forwarded-marker-4711";

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
        read: String,
        admin: String,
        /// An admin key of another workspace.
        stranger: String,
        member: Login,
        destination: Uuid,
        person: Uuid,
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

    async fn fixture(pool: &PgPool) -> Fixture {
        zirv_flow_store::migrate(pool).await.unwrap();
        let create = |slug: &'static str| async move {
            zirv_flow_store::create_workspace(
                pool,
                &WorkspaceName::parse(slug).unwrap(),
                &Slug::parse(slug).unwrap(),
            )
            .await
            .unwrap()
            .id
        };
        let ws = create("ours").await;
        let other = create("theirs").await;
        let router = router(AppState::new(pool.clone(), Arc::new(String::new)));
        let key = |workspace: WorkspaceId, scopes: &'static [Scope]| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), workspace, "forwarding", scopes)
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
        let destination = zirv_flow_store::create_destination(
            pool,
            ws,
            &zirv_flow_store::NewDestination {
                id: Uuid::now_v7(),
                kind: "fake",
                name: "Sink",
                config: &json!({"mode": "ok"}),
                credential_id: None,
                rate: 10,
                burst: 10,
                created_by: None,
            },
        )
        .await
        .unwrap()
        .id;
        let person = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(pool)
            .await
            .unwrap();
        Fixture {
            pool: pool.clone(),
            router,
            ws,
            read,
            admin: admin_key,
            stranger,
            member: Login {
                cookie,
                csrf: reply.body["csrf_token"].as_str().unwrap().to_owned(),
            },
            destination,
            person,
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
            call(&self.router, method, &self.path(tail), body, auth).await
        }

        async fn as_admin(&self, method: &str, tail: &str, body: Option<Value>) -> Reply {
            self.send(method, tail, body, Auth::Key(&self.admin)).await
        }

        fn rule(&self, name: &str) -> Value {
            json!({
                "destination_id": self.destination,
                "name": name,
                "event_name": "order.completed",
                "mapping": {"id": {"from": "event.id"},
                            "plan": {"from": "event.properties", "pointer": "/plan"}},
            })
        }

        async fn create_rule(&self, name: &str) -> String {
            let reply = self
                .as_admin("POST", "/forwarding/rules", Some(self.rule(name)))
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().to_owned()
        }

        /// An outbox row of the fixture's person in `state`, carrying the marker in its payload.
        async fn row(&self, state: &str, error: Option<&str>, age_minutes: i32) -> Uuid {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO forwarding_outbox (id, workspace_id, destination_id, rule_id, \
                    event_id, person_id, idempotency_key, payload, state, attempts, last_error, \
                    updated_at) \
                 VALUES ($1, $2, $3, gen_random_uuid(), gen_random_uuid(), $4, 'k', $5, $6, 3, $7, \
                         now() - make_interval(mins => $8))",
            )
            .bind(id)
            .bind(self.ws.as_uuid())
            .bind(self.destination)
            .bind(self.person)
            .bind(json!({"secret": MARKER}))
            .bind(state)
            .bind(error)
            .bind(age_minutes)
            .execute(&self.pool)
            .await
            .unwrap();
            id
        }

        async fn audit_actions(&self) -> Vec<String> {
            let mut entries = zirv_flow_store::list_audit(&self.pool, 200).await.unwrap();
            entries.reverse();
            entries
                .into_iter()
                .map(|entry| entry.action)
                .filter(|action| action.starts_with("forwarding."))
                .collect()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rule_cannot_be_created_for_a_private_webhook(pool: PgPool) {
        use zirv_flow_core::{HmacRoot, Keyring, MasterKey, deployment_key_aad, seal};
        use zirv_flow_store::secrets;

        let mut fx = fixture(&pool).await;
        let keys = Arc::new(Keyring::new(MasterKey::generate(), None).unwrap());
        let root = HmacRoot::bootstrap_from(keys.current()).to_secret_bytes();
        let sealed = seal(
            &keys,
            root.expose(),
            &deployment_key_aad(secrets::HMAC_ROOT),
        );
        secrets::insert_hmac_root_if_absent(&pool, &sealed)
            .await
            .unwrap();
        fx.router = router(AppState::new(pool.clone(), Arc::new(String::new)).with_keyring(keys));
        let credential = fx.as_admin("POST", "/credentials", Some(json!({
            "kind": "custom", "name": "Signing", "schema": {"fields": [
                {"key": "signing_secret", "label": "Signing secret", "type": "string", "required": true, "secret": true}
            ]}, "secret": {"signing_secret": "secret"}
        }))).await;
        assert_eq!(
            credential.status,
            StatusCode::CREATED,
            "{}",
            credential.body
        );
        let destination = fx.as_admin("POST", "/destinations", Some(json!({
            "kind": "webhook", "name": "Private", "config": {"url": "http://169.254.169.254/secret-path?token=secret-query"},
            "credential_id": credential.body["id"],
        }))).await;
        assert_eq!(
            destination.status,
            StatusCode::CREATED,
            "{}",
            destination.body
        );
        let mut rule = fx.rule("Private");
        rule["destination_id"] = destination.body["id"].clone();
        let refused = fx.as_admin("POST", "/forwarding/rules", Some(rule)).await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            refused.body
        );
        assert_eq!(
            refused.headers[header::CONTENT_TYPE],
            "application/problem+json"
        );
        assert_eq!(refused.body["code"], "egress_denied");
        assert!(!refused.body.to_string().contains("169.254.169.254"));
        assert!(fx.audit_actions().await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_forwarding_endpoint_is_admin_only_and_no_access_is_404(pool: PgPool) {
        let fx = fixture(&pool).await;
        let rule = fx.create_rule("Orders").await;
        let endpoints: Vec<(&str, String, Option<Value>)> = vec![
            ("GET", "/forwarding/rules".into(), None),
            ("POST", "/forwarding/rules".into(), Some(fx.rule("Other"))),
            ("GET", format!("/forwarding/rules/{rule}"), None),
            (
                "PATCH",
                format!("/forwarding/rules/{rule}"),
                Some(json!({"active": false})),
            ),
            ("DELETE", format!("/forwarding/rules/{rule}"), None),
            ("GET", "/forwarding/dead-letters".into(), None),
            ("GET", "/forwarding/status".into(), None),
            ("POST", "/forwarding/re-drive".into(), Some(json!({}))),
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
        let listed = fx.as_admin("GET", "/forwarding/rules", None).await;
        assert_eq!(listed.body.as_array().unwrap().len(), 1);
        assert_eq!(listed.body[0]["active"], true);
        assert_eq!(fx.audit_actions().await, ["forwarding.rule_created"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_admin_creates_reads_changes_and_deletes_a_rule_with_an_audit_entry_each(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.create_rule("Orders").await;

        let read = fx
            .as_admin("GET", &format!("/forwarding/rules/{id}"), None)
            .await;
        assert_eq!(read.status, StatusCode::OK);
        assert_eq!(read.body["event_name"], "order.completed");
        assert_eq!(read.body["filter"], json!([]));
        assert_eq!(read.body["active"], true);
        assert_eq!(read.body["mapping"]["plan"]["pointer"], "/plan");

        let patched = fx
            .as_admin(
                "PATCH",
                &format!("/forwarding/rules/{id}"),
                Some(json!({"name": "Paid orders", "active": false,
                            "mapping": {"id": {"from": "event.id"}}})),
            )
            .await;
        assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);
        assert_eq!(patched.body["name"], "Paid orders");
        assert_eq!(patched.body["active"], false);
        assert_eq!(patched.body["mapping"], json!({"id": {"from": "event.id"}}));

        let empty = fx
            .as_admin("PATCH", &format!("/forwarding/rules/{id}"), Some(json!({})))
            .await;
        assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);

        let removed = fx
            .as_admin("DELETE", &format!("/forwarding/rules/{id}"), None)
            .await;
        assert_eq!(removed.status, StatusCode::NO_CONTENT);
        let gone = fx
            .as_admin("GET", &format!("/forwarding/rules/{id}"), None)
            .await;
        assert_eq!(gone.status, StatusCode::NOT_FOUND);
        assert_eq!(
            fx.audit_actions().await,
            [
                "forwarding.rule_created",
                "forwarding.rule_updated",
                "forwarding.rule_deleted"
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn invalid_rules_are_refused_with_where_and_why_and_the_cap_and_names_hold(pool: PgPool) {
        let fx = fixture(&pool).await;
        let refused = |mut body: Value, field: &str, value: Value| {
            body[field] = value;
            body
        };
        let cases = [
            (refused(fx.rule("a"), "event_name", json!("")), "event_name"),
            (
                refused(
                    fx.rule("b"),
                    "mapping",
                    json!({"x": {"from": "event.nope"}}),
                ),
                "mapping/x unknown_source",
            ),
            (refused(fx.rule("c"), "mapping", json!({})), "mapping empty"),
            (
                refused(
                    fx.rule("d"),
                    "mapping",
                    json!({"x": {"from": "event.properties", "pointer": "plan"}}),
                ),
                "mapping/x/pointer invalid_pointer",
            ),
            (
                refused(
                    fx.rule("e"),
                    "filter",
                    json!([{"prop": "at", "op": "within_last", "value": "P1D"}]),
                ),
                "filter/0/op op_not_supported",
            ),
            (
                refused(
                    fx.rule("f"),
                    "filter",
                    json!([{"prop": "plan", "op": "eq", "value": "pro"}]),
                ),
                "filter",
            ),
            (
                refused(fx.rule("g"), "destination_id", json!(Uuid::now_v7())),
                "destination_id",
            ),
            (refused(fx.rule("   "), "name", json!("   ")), "name"),
        ];
        for (body, want) in cases {
            let reply = fx.as_admin("POST", "/forwarding/rules", Some(body)).await;
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{want}: {:?}",
                reply.body
            );
            assert!(
                reply.body["detail"].as_str().unwrap().contains(want),
                "{want}: {:?}",
                reply.body
            );
        }

        // A destination type that cannot carry events.
        let mail = zirv_flow_store::create_destination(
            &pool,
            fx.ws,
            &zirv_flow_store::NewDestination {
                id: Uuid::now_v7(),
                kind: "smtp",
                name: "Mail",
                config: &json!({}),
                credential_id: None,
                rate: 10,
                burst: 10,
                created_by: None,
            },
        )
        .await
        .unwrap()
        .id;
        let unsupported = fx
            .as_admin(
                "POST",
                "/forwarding/rules",
                Some(refused(fx.rule("h"), "destination_id", json!(mail))),
            )
            .await;
        assert_eq!(unsupported.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(unsupported.body["code"], "destination_unsupported");

        let first = fx.create_rule("Same").await;
        let taken = fx
            .as_admin("POST", "/forwarding/rules", Some(fx.rule("SAME")))
            .await;
        assert_eq!(taken.status, StatusCode::CONFLICT);
        for n in 1..20 {
            fx.create_rule(&format!("rule {n}")).await;
        }
        let over = fx
            .as_admin("POST", "/forwarding/rules", Some(fx.rule("one too many")))
            .await;
        assert_eq!(over.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(over.body["code"], "too_many_rules");
        // An inactive rule is allowed, and turning it on is refused at the cap.
        let mut dormant = fx.rule("dormant");
        dormant["active"] = json!(false);
        let created = fx
            .as_admin("POST", "/forwarding/rules", Some(dormant))
            .await;
        assert_eq!(created.status, StatusCode::CREATED);
        let id = created.body["id"].as_str().unwrap().to_owned();
        let on = fx
            .as_admin(
                "PATCH",
                &format!("/forwarding/rules/{id}"),
                Some(json!({"active": true})),
            )
            .await;
        assert_eq!(on.body["code"], "too_many_rules");
        let off = fx
            .as_admin(
                "PATCH",
                &format!("/forwarding/rules/{first}"),
                Some(json!({"active": false})),
            )
            .await;
        assert_eq!(off.status, StatusCode::OK);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_filter_on_a_property_is_checked_against_the_published_event_schema(pool: PgPool) {
        let fx = fixture(&pool).await;
        sqlx::query(
            "INSERT INTO event_schemas (workspace_id, name, mode, current_version) \
             VALUES ($1, 'order.completed', 'warn', 1)",
        )
        .bind(fx.ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO event_schema_versions (workspace_id, name, version, properties) \
             VALUES ($1, 'order.completed', 1, $2)",
        )
        .bind(fx.ws.as_uuid())
        .bind(json!([{"name": "total", "type": "number", "required": false, "pii": false}]))
        .execute(&pool)
        .await
        .unwrap();
        let with_filter = |filter: Value| {
            let mut body = fx.rule(&format!("r {}", Uuid::now_v7()));
            body["filter"] = filter;
            body
        };

        let good = fx
            .as_admin(
                "POST",
                "/forwarding/rules",
                Some(with_filter(
                    json!([{"prop": "total", "op": "gt", "value": 10}]),
                )),
            )
            .await;
        let wrong_type = fx
            .as_admin(
                "POST",
                "/forwarding/rules",
                Some(with_filter(
                    json!([{"prop": "total", "op": "contains", "value": "x"}]),
                )),
            )
            .await;
        let unknown = fx
            .as_admin(
                "POST",
                "/forwarding/rules",
                Some(with_filter(json!([{"prop": "nope", "op": "exists"}]))),
            )
            .await;

        assert_eq!(good.status, StatusCode::CREATED, "{:?}", good.body);
        assert_eq!(good.body["filter"][0]["prop"], "total");
        assert_eq!(wrong_type.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            wrong_type.body["detail"]
                .as_str()
                .unwrap()
                .contains("operator_not_allowed")
        );
        assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            unknown.body["detail"]
                .as_str()
                .unwrap()
                .contains("unknown_property")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn concurrent_changes_never_save_a_filter_invalid_for_the_final_event(pool: PgPool) {
        let fx = fixture(&pool).await;
        sqlx::query(
            "INSERT INTO event_schemas (workspace_id, name, mode, current_version) \
             VALUES ($1, 'order.completed', 'warn', 1)",
        )
        .bind(fx.ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO event_schema_versions (workspace_id, name, version, properties) \
             VALUES ($1, 'order.completed', 1, $2)",
        )
        .bind(fx.ws.as_uuid())
        .bind(json!([{"name": "total", "type": "number", "required": false, "pii": false}]))
        .execute(&pool)
        .await
        .unwrap();
        let id = fx.create_rule("racing").await;
        let rule: Uuid = id.parse().unwrap();

        // Both changes arrive while another writer holds the rule: each is checked against the
        // rule as it is once it gets the lock, never against the state it was read in.
        let mut holder = zirv_flow_store::begin(&pool).await.unwrap();
        zirv_flow_store::lock_rule(&mut holder, fx.ws, rule)
            .await
            .unwrap()
            .unwrap();
        let patch = |body: Value| {
            let router = fx.router.clone();
            let path = fx.path(&format!("/forwarding/rules/{id}"));
            let key = fx.admin.clone();
            tokio::spawn(
                async move { call(&router, "PATCH", &path, Some(body), Auth::Key(&key)).await },
            )
        };
        let filter = patch(json!({"filter": [{"prop": "total", "op": "gt", "value": 10}]}));
        let event = patch(json!({"event_name": "refund.issued"}));
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        zirv_flow_store::commit(holder).await.unwrap();
        let replies = [filter.await.unwrap(), event.await.unwrap()];

        let (event_name, stored): (String, Value) =
            sqlx::query_as("SELECT event_name, filter FROM forwarding_rules WHERE id = $1")
                .bind(rule)
                .fetch_one(&pool)
                .await
                .unwrap();
        let refused = replies
            .iter()
            .filter(|reply| reply.status == StatusCode::UNPROCESSABLE_ENTITY)
            .count();
        let saved = replies
            .iter()
            .filter(|reply| reply.status == StatusCode::OK)
            .count();
        assert_eq!(
            (saved, refused),
            (1, 1),
            "one change wins, the other is refused"
        );
        // `refund.issued` has no published schema, so it carries no predicate.
        assert!(
            event_name == "order.completed" || stored == json!([]),
            "{event_name} with {stored}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn dead_letters_are_paged_without_payloads_and_the_status_counts_the_queue(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.create_rule("Orders").await;
        let oldest = fx.row("dead", Some("http_4xx"), 30).await;
        let middle = fx.row("dead", Some("egress_denied"), 20).await;
        let newest = fx.row("dead", Some("http_5xx"), 10).await;
        fx.row("pending", None, 0).await;
        fx.row("delivered", None, 0).await;

        let page = fx
            .as_admin("GET", "/forwarding/dead-letters?limit=2", None)
            .await;
        assert_eq!(page.status, StatusCode::OK, "{:?}", page.body);
        let letters = page.body["letters"].as_array().unwrap();
        assert_eq!(letters.len(), 2);
        assert_eq!(letters[0]["id"], newest.to_string());
        assert_eq!(letters[1]["id"], middle.to_string());
        assert_eq!(letters[0]["error"], "http_5xx");
        assert_eq!(letters[0]["attempts"], 3);
        assert_eq!(letters[0]["destination_id"], fx.destination.to_string());
        assert!(letters[0].get("payload").is_none());
        let before = page.body["next_before"].as_str().unwrap().to_owned();

        let rest = fx
            .as_admin(
                "GET",
                &format!("/forwarding/dead-letters?limit=2&before={before}"),
                None,
            )
            .await;
        let letters = rest.body["letters"].as_array().unwrap();
        assert_eq!(letters.len(), 1);
        assert_eq!(letters[0]["id"], oldest.to_string());
        assert!(rest.body["next_before"].is_null());
        let bad = fx
            .as_admin("GET", "/forwarding/dead-letters?limit=0", None)
            .await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST);

        let status = fx.as_admin("GET", "/forwarding/status", None).await;
        assert_eq!(status.status, StatusCode::OK, "{:?}", status.body);
        let destinations = status.body["destinations"].as_array().unwrap();
        assert_eq!(destinations.len(), 1);
        assert_eq!(
            destinations[0]["destination_id"],
            fx.destination.to_string()
        );
        assert_eq!(destinations[0]["depth"], 1);
        assert_eq!(destinations[0]["dead"], 3);
        assert!(destinations[0]["oldest_lag_seconds"].as_i64().unwrap() >= 0);
        for reply in [&page, &rest, &status] {
            assert!(!reply.body.to_string().contains(MARKER), "no payload value");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_redrive_puts_dead_rows_back_audited_and_starts_no_flow(pool: PgPool) {
        let fx = fixture(&pool).await;
        let one = fx.row("dead", Some("http_4xx"), 5).await;
        fx.row("dead", Some("http_4xx"), 4).await;
        fx.row("dead", Some("http_4xx"), 3).await;

        let both = fx
            .as_admin(
                "POST",
                "/forwarding/re-drive",
                Some(json!({"id": one, "destination_id": fx.destination})),
            )
            .await;
        assert_eq!(both.status, StatusCode::UNPROCESSABLE_ENTITY);
        let single = fx
            .as_admin("POST", "/forwarding/re-drive", Some(json!({"id": one})))
            .await;
        assert_eq!(single.body["redriven"], 1, "{:?}", single.body);
        let remaining = fx
            .as_admin(
                "POST",
                "/forwarding/re-drive",
                Some(json!({"destination_id": fx.destination})),
            )
            .await;
        assert_eq!(remaining.body["redriven"], 2);
        let nothing = fx
            .as_admin("POST", "/forwarding/re-drive", Some(json!({})))
            .await;
        assert_eq!(nothing.body["redriven"], 0);

        let states: Vec<(String, i32, i32)> = sqlx::query_as(
            "SELECT state, attempts, attempt_offset FROM forwarding_outbox ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(states, vec![("pending".to_owned(), 0, 3); 3]);
        let kinds: Vec<String> = sqlx::query_scalar("SELECT DISTINCT kind FROM jobs ORDER BY kind")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(kinds, ["event.forward"], "no flow job");
        let enrollments: i64 = sqlx::query_scalar("SELECT count(*) FROM enrollments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(enrollments, 0);
        assert_eq!(
            fx.audit_actions().await,
            [
                "forwarding.redriven",
                "forwarding.redriven",
                "forwarding.redriven"
            ]
        );
        let audit = zirv_flow_store::list_audit(&pool, 50).await.unwrap();
        assert!(
            !format!("{audit:?}").contains(MARKER),
            "no payload in the audit"
        );
    }
}
