//! The provider feedback route and the domain-status read end to end, driven through the router
//! against the fake provider (#223).

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use time::OffsetDateTime;
    use tower::ServiceExt;
    use uuid::Uuid;
    use zirv_flow_connectors::destinations::{
        FakeProvider, FakeProviderOptions, provider_feedback_of, provider_feedback_scheme,
        sign_provider_feedback,
    };
    use zirv_flow_core::egress::EgressPolicy;
    use zirv_flow_core::{
        HmacRoot, Keyring, MasterKey, Scope, Slug, WorkspaceId, WorkspaceName, deployment_key_aad,
        seal,
    };
    use zirv_flow_store::{is_suppressed, secrets};

    use crate::admin::{self, Context};
    use crate::test_support::tables_hold_no;
    use crate::{AppState, router};

    const ONE: &str = include_str!(
        "../../../zirv-flow-connectors/tests/fixtures/providers/bearer_timestamped.json"
    );
    const TWO: &str = include_str!(
        "../../../zirv-flow-connectors/tests/fixtures/providers/api_key_body_only.json"
    );
    /// Distinct from anything else a response or a table may hold.
    const SECRET: &str = "zz-feedback-secret-4711";
    const TOKEN: &str = "zz-api-token-0815";
    const ADDRESS: &str = "zz-recipient@example.test";

    struct Reply {
        status: StatusCode,
        body: Value,
    }

    struct Fx {
        pool: PgPool,
        router: Router,
        ws: WorkspaceId,
        other: WorkspaceId,
        admin: String,
        read: String,
        stranger: String,
        fake: FakeProvider,
        /// The destination made from the timestamped fixture.
        destination: Uuid,
        config: Value,
    }

    async fn send(
        router: &Router,
        method: &str,
        uri: &str,
        key: Option<&str>,
        body: Option<Value>,
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(key) = key {
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

    fn manifest_for(fixture: &str, fake: &FakeProvider) -> Value {
        let text = fixture.replace(
            "https://api.fixture-provider.example.test",
            &fake.base_url(),
        );
        serde_json::from_str(&text).unwrap()
    }

    async fn fixture(pool: &PgPool) -> Fx {
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
        let keyring = Arc::new(Keyring::new(MasterKey::generate(), None).unwrap());
        let root = HmacRoot::bootstrap_from(keyring.current()).to_secret_bytes();
        let sealed = seal(
            &keyring,
            root.expose(),
            &deployment_key_aad(secrets::HMAC_ROOT),
        );
        secrets::insert_hmac_root_if_absent(pool, &sealed)
            .await
            .unwrap();
        let policy = EgressPolicy::parse(&["127.0.0.0/8".to_owned()]).unwrap();
        let state = AppState::new(pool.clone(), Arc::new(String::new))
            .with_keyring(keyring)
            .with_egress_policy(Arc::new(policy));
        let router = router(state);
        let key = |workspace: WorkspaceId, scopes: &'static [Scope]| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), workspace, "feedback", scopes)
                    .await
                    .unwrap()
                    .plaintext
            }
        };
        let admin = key(ws, &[Scope::Admin, Scope::Read]).await;
        let read = key(ws, &[Scope::Read]).await;
        let stranger = key(other, &[Scope::Admin, Scope::Read]).await;
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let mut fx = Fx {
            pool: pool.clone(),
            router,
            ws,
            other,
            admin,
            read,
            stranger,
            fake,
            destination: Uuid::nil(),
            config: Value::Null,
        };
        fx.config = json!({"manifest": manifest_for(ONE, &fx.fake)});
        let credential = fx
            .credential(&[("token", TOKEN), ("feedback_secret", SECRET)])
            .await;
        fx.destination = fx
            .destination_of(&fx.config.clone(), &credential, "Mail")
            .await;
        fx
    }

    impl Fx {
        fn path(&self, tail: &str) -> String {
            format!("/api/v1/workspaces/{}{tail}", self.ws.as_uuid())
        }

        async fn as_admin(&self, method: &str, tail: &str, body: Option<Value>) -> Reply {
            send(
                &self.router,
                method,
                &self.path(tail),
                Some(&self.admin),
                body,
            )
            .await
        }

        async fn credential(&self, secrets: &[(&str, &str)]) -> String {
            let fields: Vec<Value> = secrets
                .iter()
                .map(|(key, _)| {
                    json!({"key": key, "label": key, "type": "string",
                           "required": true, "secret": true})
                })
                .collect();
            let secret: serde_json::Map<String, Value> = secrets
                .iter()
                .map(|(key, value)| ((*key).to_owned(), json!(value)))
                .collect();
            let reply = self
                .as_admin(
                    "POST",
                    "/credentials",
                    Some(
                        json!({"kind": "custom", "name": format!("c-{}", Uuid::now_v7()),
                                "schema": {"fields": fields}, "secret": secret}),
                    ),
                )
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().to_owned()
        }

        async fn destination_of(&self, config: &Value, credential: &str, name: &str) -> Uuid {
            let reply = self
                .as_admin(
                    "POST",
                    "/destinations",
                    Some(json!({"kind": "provider", "name": name, "config": config,
                                "credential_id": credential})),
                )
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().parse().unwrap()
        }

        /// A message of a new person sent through `destination` of `workspace`, with `hash` as
        /// the recipient hash it was marked sending with.
        async fn message(
            &self,
            workspace: WorkspaceId,
            destination: Uuid,
            hash: Option<[u8; 32]>,
        ) -> Uuid {
            let person = Uuid::now_v7();
            sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
                .bind(person)
                .bind(workspace.as_uuid())
                .execute(&self.pool)
                .await
                .unwrap();
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, \
                    template_id, destination_id, status, address_hash) \
                 VALUES ($1, $2, $3, gen_random_uuid(), 'n', gen_random_uuid(), $4, 'sending', $5)",
            )
            .bind(id)
            .bind(workspace.as_uuid())
            .bind(person)
            .bind(destination)
            .bind(hash.as_ref().map(|hash| hash.as_slice()))
            .execute(&self.pool)
            .await
            .unwrap();
            id
        }

        /// Posts a feedback event signed under `secret` at `timestamp`.
        async fn feedback(
            &self,
            destination: Uuid,
            event: &Value,
            secret: &str,
            timestamp: i64,
        ) -> Reply {
            let (_, verification) = provider_feedback_of(&self.config).unwrap();
            let body = event.to_string().into_bytes();
            let headers = sign_provider_feedback(
                &provider_feedback_scheme(&verification),
                secret,
                timestamp,
                &body,
            );
            self.post(destination, headers, body).await
        }

        async fn post(
            &self,
            destination: Uuid,
            headers: Vec<(String, String)>,
            body: Vec<u8>,
        ) -> Reply {
            let mut request = Request::builder()
                .method("POST")
                .uri(format!("/api/v1/destinations/{destination}/feedback"));
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let response = self
                .router
                .clone()
                .oneshot(request.body(Body::from(body)).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            Reply {
                status,
                body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            }
        }

        async fn count(&self, table: &str) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        async fn nothing_written(&self) {
            for table in ["events", "suppressions", "decision_records", "event_dedupe"] {
                assert_eq!(self.count(table).await, 0, "{table}");
            }
        }
    }

    fn now() -> i64 {
        OffsetDateTime::now_utc().unix_timestamp()
    }

    fn event(kind: &str, message: Uuid, id: &str, extra: Value) -> Value {
        let mut event = json!({"event": kind, "id": id, "meta": {"message_id": message}});
        if let (Some(target), Some(extra)) = (event.as_object_mut(), extra.as_object()) {
            target.extend(extra.clone());
        }
        event
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_provider_destination_takes_the_manifests_rate_and_needs_its_credential_fields(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;

        let listed = fx
            .as_admin("GET", &format!("/destinations/{}", fx.destination), None)
            .await;
        assert_eq!(listed.body["kind"], "provider");
        assert_eq!(listed.body["rate"], 20, "the manifest's rate limit");
        assert_eq!(listed.body["burst"], 20);

        let without_secret = fx.credential(&[("token", TOKEN)]).await;
        let refused = fx
            .as_admin(
                "POST",
                "/destinations",
                Some(
                    json!({"kind": "provider", "name": "Other", "config": fx.config,
                            "credential_id": without_secret}),
                ),
            )
            .await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            refused.body
        );

        let mut unverified = fx.config.clone();
        unverified["manifest"]["destination"]["feedback"]
            .as_object_mut()
            .unwrap()
            .remove("verification");
        let credential = fx
            .credential(&[("token", TOKEN), ("feedback_secret", SECRET)])
            .await;
        let refused = fx
            .as_admin(
                "POST",
                "/destinations",
                Some(
                    json!({"kind": "provider", "name": "Open", "config": unverified,
                            "credential_id": credential}),
                ),
            )
            .await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            refused.body.to_string().contains("verification"),
            "{:?}",
            refused.body
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_signed_hard_bounce_suppresses_in_every_workspace_and_a_complaint_only_in_its_own(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let bounced_hash = [8_u8; 32];
        let complained_hash = [9_u8; 32];
        let bounced = fx.message(fx.ws, fx.destination, Some(bounced_hash)).await;
        let complained = fx
            .message(fx.ws, fx.destination, Some(complained_hash))
            .await;

        let hard = fx
            .feedback(
                fx.destination,
                &event(
                    "bounced",
                    bounced,
                    "e-1",
                    json!({"bounce": {"kind": "permanent"}}),
                ),
                SECRET,
                now(),
            )
            .await;
        let complaint = fx
            .feedback(
                fx.destination,
                &event("complained", complained, "e-2", json!({})),
                SECRET,
                now(),
            )
            .await;

        assert_eq!(hard.status, StatusCode::ACCEPTED, "{:?}", hard.body);
        assert_eq!(complaint.status, StatusCode::ACCEPTED);
        assert!(is_suppressed(&pool, fx.ws, &bounced_hash).await.unwrap());
        assert!(
            is_suppressed(&pool, fx.other, &bounced_hash).await.unwrap(),
            "a hard bounce suppresses in another workspace too"
        );
        assert!(is_suppressed(&pool, fx.ws, &complained_hash).await.unwrap());
        assert!(
            !is_suppressed(&pool, fx.other, &complained_hash)
                .await
                .unwrap(),
            "a complaint suppresses only its workspace"
        );
        let names: Vec<String> = sqlx::query_scalar("SELECT name FROM events ORDER BY name")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(names, ["email.bounced", "email.complained"]);
        let soft = fx.message(fx.ws, fx.destination, Some([7_u8; 32])).await;
        fx.feedback(
            fx.destination,
            &event(
                "bounced",
                soft,
                "e-3",
                json!({"bounce": {"kind": "transient"}}),
            ),
            SECRET,
            now(),
        )
        .await;
        fx.feedback(
            fx.destination,
            &event("delivered", soft, "e-4", json!({})),
            SECRET,
            now(),
        )
        .await;
        assert!(!is_suppressed(&pool, fx.ws, &[7_u8; 32]).await.unwrap());
        assert_eq!(fx.count("suppressions").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_bad_missing_or_stale_signature_is_401_before_the_body_is_parsed_and_writes_nothing(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let hash = [8_u8; 32];
        let message = fx.message(fx.ws, fx.destination, Some(hash)).await;
        let good = event(
            "bounced",
            message,
            "e-1",
            json!({"bounce": {"kind": "permanent"}}),
        );

        let wrong = fx
            .feedback(fx.destination, &good, "not-the-secret", now())
            .await;
        let stale = fx
            .feedback(fx.destination, &good, SECRET, now() - 3600)
            .await;
        let future = fx
            .feedback(fx.destination, &good, SECRET, now() + 3600)
            .await;
        let unsigned = fx
            .post(fx.destination, Vec::new(), good.to_string().into_bytes())
            .await;
        // A body that is not JSON, wrongly signed: the signature is judged first.
        let junk = fx
            .post(
                fx.destination,
                vec![(
                    "X-Fixture-Signature".into(),
                    format!("sha256={}", "0".repeat(64)),
                )],
                b"not json at all".to_vec(),
            )
            .await;

        for (name, reply) in [
            ("wrong", &wrong),
            ("stale", &stale),
            ("future", &future),
            ("unsigned", &unsigned),
            ("junk", &junk),
        ] {
            assert_eq!(
                reply.status,
                StatusCode::UNAUTHORIZED,
                "{name}: {:?}",
                reply.body
            );
            assert_eq!(reply.body["code"], "invalid_signature", "{name}");
        }
        assert!(stale.body["detail"].as_str().unwrap().contains("tolerance"));
        assert!(!wrong.body.to_string().contains(SECRET));
        assert!(!is_suppressed(&pool, fx.ws, &hash).await.unwrap());
        fx.nothing_written().await;

        // A correctly signed body that is not JSON is a 400, after the signature.
        let (_, verification) = provider_feedback_of(&fx.config).unwrap();
        let body = b"not json at all".to_vec();
        let headers = sign_provider_feedback(
            &provider_feedback_scheme(&verification),
            SECRET,
            now(),
            &body,
        );
        let signed_junk = fx.post(fx.destination, headers, body).await;
        assert_eq!(signed_junk.status, StatusCode::BAD_REQUEST);
        assert_eq!(signed_junk.body["code"], "invalid_payload");
        fx.nothing_written().await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unknown_foreign_or_unmapped_event_is_dropped_with_202_and_writes_nothing(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let hash = [8_u8; 32];
        // A message of the other workspace and one sent through another destination.
        let other_destination = Uuid::now_v7();
        let foreign = fx.message(fx.other, other_destination, Some(hash)).await;
        let elsewhere = fx.message(fx.ws, other_destination, Some(hash)).await;
        let hard = json!({"bounce": {"kind": "permanent"}});

        let cases = [
            event("bounced", Uuid::now_v7(), "a", hard.clone()),
            event("bounced", foreign, "b", hard.clone()),
            event("bounced", elsewhere, "c", hard.clone()),
            json!({"event": "bounced", "id": "d", "meta": {"message_id": "not-a-uuid"}}),
            json!({"event": "opened", "id": "e", "meta": {"message_id": foreign}}),
            json!({"unrelated": true}),
        ];
        for case in &cases {
            let reply = fx.feedback(fx.destination, case, SECRET, now()).await;
            assert_eq!(
                reply.status,
                StatusCode::ACCEPTED,
                "{case}: {:?}",
                reply.body
            );
        }

        assert!(!is_suppressed(&pool, fx.ws, &hash).await.unwrap());
        assert!(!is_suppressed(&pool, fx.other, &hash).await.unwrap());
        fx.nothing_written().await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_provider_retry_of_the_same_event_writes_one_event(pool: PgPool) {
        let fx = fixture(&pool).await;
        let message = fx.message(fx.ws, fx.destination, Some([8_u8; 32])).await;
        let bounce = event(
            "bounced",
            message,
            "e-1",
            json!({"bounce": {"kind": "permanent"}}),
        );

        for _ in 0..3 {
            let reply = fx.feedback(fx.destination, &bounce, SECRET, now()).await;
            assert_eq!(reply.status, StatusCode::ACCEPTED);
        }

        assert_eq!(fx.count("events").await, 1);
        assert_eq!(fx.count("suppressions").await, 1);
        let duplicates: i32 = sqlx::query_scalar("SELECT duplicates FROM event_dedupe")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(duplicates, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_message_with_no_stored_hash_records_the_event_and_suppresses_nobody(pool: PgPool) {
        let fx = fixture(&pool).await;
        let message = fx.message(fx.ws, fx.destination, None).await;

        let reply = fx
            .feedback(
                fx.destination,
                &event(
                    "bounced",
                    message,
                    "e-1",
                    json!({"bounce": {"kind": "permanent"}}),
                ),
                SECRET,
                now(),
            )
            .await;

        assert_eq!(reply.status, StatusCode::ACCEPTED);
        assert_eq!(fx.count("events").await, 1);
        assert_eq!(fx.count("suppressions").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_route_is_404_for_an_unknown_or_non_provider_destination_and_413_over_the_cap(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let credential = fx.credential(&[("token", "ok")]).await;
        let reply = fx
            .as_admin(
                "POST",
                "/destinations",
                Some(
                    json!({"kind": "fake", "name": "Fake", "config": {"mode": "ok"},
                            "credential_id": credential}),
                ),
            )
            .await;
        let fake_id: Uuid = reply.body["id"].as_str().unwrap().parse().unwrap();

        for id in [Uuid::now_v7(), fake_id] {
            let reply = fx.post(id, Vec::new(), b"{}".to_vec()).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{id}");
        }
        let not_an_id = send(
            &fx.router,
            "POST",
            "/api/v1/destinations/not-a-uuid/feedback",
            None,
            Some(json!({})),
        )
        .await;
        assert_eq!(not_an_id.status, StatusCode::NOT_FOUND);
        let huge = fx
            .post(fx.destination, Vec::new(), vec![b'x'; 2 * 1024 * 1024])
            .await;
        assert_eq!(huge.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn no_secret_signature_or_address_reaches_a_row_or_a_reply(pool: PgPool) {
        let fx = fixture(&pool).await;
        let message = fx.message(fx.ws, fx.destination, Some([8_u8; 32])).await;
        let bounce = json!({
            "event": "bounced", "id": "e-1", "meta": {"message_id": message},
            "bounce": {"kind": "permanent"}, "recipient": ADDRESS,
        });
        let (_, verification) = provider_feedback_of(&fx.config).unwrap();
        let body = bounce.to_string().into_bytes();
        let headers = sign_provider_feedback(
            &provider_feedback_scheme(&verification),
            SECRET,
            now(),
            &body,
        );
        let signature = headers
            .iter()
            .find(|(name, _)| name == "X-Fixture-Signature")
            .map(|(_, value)| value.clone())
            .unwrap();
        let digest = signature.trim_start_matches("sha256=").to_owned();

        let reply = fx.post(fx.destination, headers, body).await;

        assert_eq!(reply.status, StatusCode::ACCEPTED);
        let text = reply.body.to_string();
        assert!(!text.contains(SECRET) && !text.contains(ADDRESS) && !text.contains(&digest));
        tables_hold_no(&pool, &[SECRET, TOKEN, ADDRESS, &digest]).await;
    }

    async fn domain(fx: &Fx) -> Uuid {
        let reply = fx
            .as_admin(
                "POST",
                "/sending-domains",
                Some(json!({"domain": "news.example.com"})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
        reply.body["id"].as_str().unwrap().parse().unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_domain_status_shows_the_providers_records_next_to_ours_and_never_verifies(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let domain = domain(&fx).await;
        fx.fake.set_domain_status(Some(json!({"records": [
            {"host": "zirv._domainkey.news.example.com", "kind": "TXT", "state": "verified"},
            {"host": "bounces.news.example.com", "kind": "CNAME", "state": "pending"}
        ]})));
        let tail = format!(
            "/destinations/{}/domain-status?domain_id={domain}",
            fx.destination
        );

        let as_reader = send(&fx.router, "GET", &fx.path(&tail), Some(&fx.read), None).await;

        assert_eq!(as_reader.status, StatusCode::OK, "{:?}", as_reader.body);
        assert_eq!(as_reader.body["status"], "unverified");
        let dkim = as_reader.body["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["kind"] == "dkim")
            .unwrap();
        assert_eq!(dkim["provider"]["status"], "verified");
        assert_ne!(dkim["status"], "ok", "our own check has not run");
        assert_eq!(as_reader.body["provider_only"][0]["status"], "pending");
        assert_eq!(fx.fake.domain_queries(), ["domain=news.example.com"]);
        let after = fx
            .as_admin("GET", &format!("/sending-domains/{domain}"), None)
            .await;
        assert_eq!(
            after.body["status"], "unverified",
            "the provider never verifies a domain"
        );
        assert!(after.body["verified_at"].is_null());
        assert!(!as_reader.body.to_string().contains(TOKEN));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_domain_status_is_404_without_a_declared_request_a_domain_or_access_and_400_without_an_id(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let domain = domain(&fx).await;
        let credential = fx
            .credential(&[("api_key", "key-1"), ("hook_secret", "hook-1")])
            .await;
        let plain = fx
            .destination_of(
                &json!({"manifest": manifest_for(TWO, &fx.fake)}),
                &credential,
                "Plain",
            )
            .await;
        let asks = |destination: Uuid, query: String| {
            let uri = fx.path(&format!("/destinations/{destination}/domain-status{query}"));
            let router = fx.router.clone();
            let key = fx.read.clone();
            async move { send(&router, "GET", &uri, Some(&key), None).await }
        };

        let undeclared = asks(plain, format!("?domain_id={domain}")).await;
        let unknown_domain = asks(fx.destination, format!("?domain_id={}", Uuid::now_v7())).await;
        let no_id = asks(fx.destination, String::new()).await;
        let anonymous = send(
            &fx.router,
            "GET",
            &fx.path(&format!(
                "/destinations/{}/domain-status?domain_id={domain}",
                fx.destination
            )),
            None,
            None,
        )
        .await;
        let stranger = send(
            &fx.router,
            "GET",
            &fx.path(&format!(
                "/destinations/{}/domain-status?domain_id={domain}",
                fx.destination
            )),
            Some(&fx.stranger),
            None,
        )
        .await;

        assert_eq!(undeclared.status, StatusCode::NOT_FOUND);
        assert_eq!(undeclared.body["code"], "domain_status_unsupported");
        assert_eq!(unknown_domain.status, StatusCode::NOT_FOUND);
        assert_eq!(no_id.status, StatusCode::BAD_REQUEST);
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(stranger.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_provider_that_errors_is_a_502_with_a_fixed_code_and_no_value(pool: PgPool) {
        let fx = fixture(&pool).await;
        let domain = domain(&fx).await;
        let tail = format!(
            "/destinations/{}/domain-status?domain_id={domain}",
            fx.destination
        );

        // The fake answers 404 while it has no domain document.
        let reply = send(&fx.router, "GET", &fx.path(&tail), Some(&fx.read), None).await;

        assert_eq!(reply.status, StatusCode::BAD_GATEWAY, "{:?}", reply.body);
        assert_eq!(reply.body["code"], "upstream_error");
        let text = reply.body.to_string();
        assert!(
            !text.contains("127.0.0.1") && !text.contains(TOKEN),
            "{text}"
        );
    }
}
