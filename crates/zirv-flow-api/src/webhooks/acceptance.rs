//! The webhooks API end to end, driven through the router: the public receive endpoint, the
//! admin operations, and the guards on the generic source routes.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::http::{HeaderMap, Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;
    use zirv_flow_core::people::{Precedence, TraitType};
    use zirv_flow_core::signature::sign;
    use zirv_flow_core::{
        HmacPurpose, HmacRoot, Keyring, MasterKey, Role, Scope, Slug, WorkspaceId, WorkspaceName,
        deployment_key_aad, seal,
    };
    use zirv_flow_store::{NewTraitDefinition, Shard, drain_batch, secrets};

    use crate::admin::{self, Context};
    use crate::ingest::IngestLimits;
    use crate::test_support::{tables_hold_no, the_scan_finds_a_planted};
    use crate::{AppState, router};

    const PASSWORD: &str = "correct horse battery";

    /// A label, the headers sent and the body sent.
    type Case = (&'static str, Vec<(String, String)>, Vec<u8>);

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Value,
        text: String,
    }

    struct Login {
        cookie: String,
        csrf: String,
    }

    enum Auth<'a> {
        Anonymous,
        Key(&'a str),
        Session(&'a Login),
        /// A session cookie without the CSRF header.
        NoCsrf(&'a Login),
    }

    struct Fixture {
        pool: PgPool,
        router: Router,
        ws: WorkspaceId,
        admin: String,
        read: String,
        member: Login,
        /// An admin key of another workspace.
        other_admin: String,
        other_ws: WorkspaceId,
    }

    /// A hook as created: its id, its secret and its source.
    struct Hook {
        id: String,
        source: String,
        secret: String,
    }

    async fn send(
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
            Auth::NoCsrf(login) => request.header(header::COOKIE, &login.cookie),
        };
        let body = match body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        reply(
            router
                .clone()
                .oneshot(request.body(body).unwrap())
                .await
                .unwrap(),
        )
        .await
    }

    async fn reply(response: axum::response::Response) -> Reply {
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            text: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    fn keyring() -> Arc<Keyring> {
        Arc::new(Keyring::new(MasterKey::generate(), None).unwrap())
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        fixture_with(pool, Some(keyring()), IngestLimits::default()).await
    }

    async fn fixture_with(
        pool: &PgPool,
        keyring: Option<Arc<Keyring>>,
        limits: IngestLimits,
    ) -> Fixture {
        zirv_flow_store::migrate(pool).await.unwrap();
        let mut workspaces = Vec::new();
        for slug in ["ours", "theirs"] {
            let name = WorkspaceName::parse(slug).unwrap();
            let slug = Slug::parse(slug).unwrap();
            workspaces.push(
                zirv_flow_store::create_workspace(pool, &name, &slug)
                    .await
                    .unwrap()
                    .id,
            );
        }
        let (ws, other_ws) = (workspaces[0], workspaces[1]);
        plan_trait(pool, ws).await;
        let mut state =
            AppState::new(pool.clone(), Arc::new(String::new)).with_ingest_limits(limits);
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
                admin::create_api_key(&pool, &Context::system(), workspace, "hooks", scopes)
                    .await
                    .unwrap()
                    .plaintext
            }
        };
        let admin_key = key(ws, &[Scope::Admin, Scope::Read]).await;
        let read = key(ws, &[Scope::Read]).await;
        let other_admin = key(other_ws, &[Scope::Admin, Scope::Read]).await;
        let user = admin::create_user(pool, &Context::system(), "member@example.com", PASSWORD)
            .await
            .unwrap();
        admin::set_member(pool, &Context::system(), ws, user.id, Role::Member)
            .await
            .unwrap();
        let login = send(
            &router,
            "POST",
            "/api/v1/session",
            Some(json!({"email": "member@example.com", "password": PASSWORD})),
            Auth::Anonymous,
        )
        .await;
        let cookie = login.headers[header::SET_COOKIE]
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
            admin: admin_key,
            read,
            member: Login {
                cookie,
                csrf: login.body["csrf_token"].as_str().unwrap().to_owned(),
            },
            other_admin,
            other_ws,
        }
    }

    impl Fixture {
        fn path(&self, tail: &str) -> String {
            format!("/api/v1/workspaces/{}{tail}", self.ws.as_uuid())
        }

        async fn as_admin(&self, method: &str, tail: &str, body: Option<Value>) -> Reply {
            send(
                &self.router,
                method,
                &self.path(tail),
                body,
                Auth::Key(&self.admin),
            )
            .await
        }

        async fn hooks(
            &self,
            method: &str,
            tail: &str,
            body: Option<Value>,
            auth: Auth<'_>,
        ) -> Reply {
            send(
                &self.router,
                method,
                &self.path(&format!("/webhooks{tail}")),
                body,
                auth,
            )
            .await
        }

        /// A hook with the default scheme, mapped and enabled.
        async fn hook(&self, name: &str) -> Hook {
            self.hook_with(name, json!({})).await
        }

        async fn hook_with(&self, name: &str, extra: Value) -> Hook {
            let mut body =
                json!({"name": name, "dedupe_field": "id", "staleness_threshold_seconds": 3600});
            for (key, value) in extra.as_object().unwrap() {
                body[key] = value.clone();
            }
            let created = self
                .hooks("POST", "", Some(body), Auth::Key(&self.admin))
                .await;
            assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
            let hook = Hook {
                id: created.body["id"].as_str().unwrap().to_owned(),
                source: created.body["source_id"].as_str().unwrap().to_owned(),
                secret: created.body["secret"].as_str().unwrap().to_owned(),
            };
            let mapped = self.map(&hook, true).await;
            assert_eq!(mapped.status, StatusCode::OK, "{:?}", mapped.body);
            hook
        }

        async fn map(&self, hook: &Hook, enabled: bool) -> Reply {
            self.as_admin(
                "PUT",
                &format!("/sources/{}/streams/payload", hook.source),
                Some(json!({
                    "enabled": enabled,
                    "sync_mode": "full_refresh",
                    "staleness_threshold_seconds": 3600,
                    "mapping": mapping(),
                })),
            )
            .await
        }

        async fn put_mapping(&self, hook: &Hook, mapping: Value) -> Reply {
            self.as_admin(
                "PUT",
                &format!("/sources/{}/streams/payload", hook.source),
                Some(json!({"enabled": true, "sync_mode": "full_refresh", "mapping": mapping})),
            )
            .await
        }

        async fn count(&self, sql: &'static str) -> i64 {
            sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
        }

        async fn last_success(&self, hook: &Hook) -> Option<time::OffsetDateTime> {
            sqlx::query_scalar(
                "SELECT t.last_success_at FROM streams t JOIN webhooks w ON w.stream_id = t.id \
                 WHERE w.id = $1",
            )
            .bind(Uuid::parse_str(&hook.id).unwrap())
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }

        async fn drain(&self) {
            let key =
                HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
            drain_batch(&self.pool, self.ws, Shard::ALL, &key, 500)
                .await
                .unwrap();
        }
    }

    fn mapping() -> Value {
        json!({
            "columns": [
                {"column": "email", "target": {"type": "identifier", "kind": "email"}},
                {"column": "plan", "target": {"type": "trait", "key": "plan"}}
            ],
            "event": {"name": "order_placed", "properties": [{"property": "total", "column": "order.total"}]}
        })
    }

    async fn plan_trait(pool: &PgPool, ws: WorkspaceId) {
        zirv_flow_store::create_trait_definition(
            pool,
            ws,
            &NewTraitDefinition {
                key: "plan".into(),
                trait_type: TraitType::String,
                precedence: Precedence::Latest,
                source_order: Vec::new(),
                pii: false,
                channel_address: false,
                inferred: false,
                description: String::new(),
            },
        )
        .await
        .unwrap();
    }

    fn now() -> i64 {
        time::OffsetDateTime::now_utc().unix_timestamp()
    }

    fn order(id: &str, plan: &str) -> Vec<u8> {
        json!({"id": id, "email": "ada@example.com", "plan": plan, "order": {"total": 12}})
            .to_string()
            .into_bytes()
    }

    /// The headers of a request signed as the README documents: `X-Zirv-Timestamp` and
    /// `X-Zirv-Signature: sha256=<hmac of "{timestamp}.{body}">`.
    fn signed(secret: &str, timestamp: i64, body: &[u8]) -> Vec<(String, String)> {
        vec![
            ("x-zirv-timestamp".into(), timestamp.to_string()),
            (
                "x-zirv-signature".into(),
                format!("sha256={}", sign(secret.as_bytes(), Some(timestamp), body)),
            ),
        ]
    }

    async fn post_hook(
        router: &Router,
        id: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Reply {
        let mut request = Request::builder()
            .method("POST")
            .uri(format!("/hooks/{id}"))
            .header(header::CONTENT_TYPE, "application/json");
        for (name, value) in headers {
            request = request.header(name, value);
        }
        reply(
            router
                .clone()
                .oneshot(request.body(Body::from(body)).unwrap())
                .await
                .unwrap(),
        )
        .await
    }

    async fn deliver(fx: &Fixture, hook: &Hook, body: Vec<u8>) -> Reply {
        let headers = signed(&hook.secret, now(), &body);
        post_hook(&fx.router, &hook.id, &headers, body).await
    }

    fn is_problem(reply: &Reply) -> bool {
        reply
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/problem+json"))
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_signed_payload_is_accepted_queued_and_applied_exactly_once(pool: PgPool) {
        let fx = fixture(&pool).await;
        let hook = fx.hook("orders").await;

        let accepted = deliver(&fx, &hook, order("o-1", "pro")).await;

        assert_eq!(accepted.status, StatusCode::ACCEPTED, "{}", accepted.text);
        assert_eq!(accepted.body, json!({"accepted": 1}));
        let kinds: Vec<(String, Option<Uuid>)> =
            sqlx::query_as("SELECT kind, api_key_id FROM ingest_inbox")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(kinds, [("webhook".to_owned(), None)]);
        assert_eq!(
            fx.count("SELECT count(*) FROM jobs WHERE kind = 'ingest.drain'")
                .await,
            1
        );
        fx.drain().await;
        assert_eq!(fx.count("SELECT count(*) FROM people").await, 1);
        assert_eq!(fx.count("SELECT count(*) FROM events").await, 1);
        assert_eq!(fx.count("SELECT count(*) FROM ingest_inbox").await, 0);
        assert!(fx.last_success(&hook).await.is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replay_leaves_one_event_and_the_trait_of_the_newest_payload(pool: PgPool) {
        let fx = fixture(&pool).await;
        let hook = fx.hook("orders").await;
        let now = now();
        let newer = order("o-2", "pro");
        let older = order("o-1", "free");

        for (body, at) in [(&newer, now), (&older, now - 100)] {
            let headers = signed(&hook.secret, at, body);
            assert_eq!(
                post_hook(&fx.router, &hook.id, &headers, body.clone())
                    .await
                    .status,
                StatusCode::ACCEPTED
            );
        }
        fx.drain().await;
        // The older payload arrived last but must not have overwritten the newer one.
        let after_older: Value =
            sqlx::query_scalar("SELECT traits->'plan'->'v' FROM source_records")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(after_older, json!("pro"));
        // The same signed payload again, with a fresh signature and without one.
        let replay = signed(&hook.secret, now, &newer);
        let again = post_hook(&fx.router, &hook.id, &replay, newer.clone()).await;
        let fresh = deliver(&fx, &hook, newer.clone()).await;
        assert_eq!(again.status, StatusCode::ACCEPTED);
        assert_eq!(fresh.status, StatusCode::ACCEPTED);
        fx.drain().await;

        assert_eq!(fx.count("SELECT count(*) FROM events").await, 2);
        assert_eq!(fx.count("SELECT count(*) FROM source_records").await, 1);
        let plan: Value = sqlx::query_scalar("SELECT traits->'plan'->'v' FROM source_records")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(plan, json!("pro"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_signature_fault_is_a_401_problem_that_writes_nothing_and_leaks_nothing(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let hook = fx.hook("orders").await;
        let body = order("o-1", "pro");
        let at = now();
        let good = signed(&hook.secret, at, &body);
        let signature = good[1].1.clone();
        let digest = signature.trim_start_matches("sha256=").to_owned();
        let header = |name: &str, value: String| (name.to_owned(), value);
        let without = |name: &str| -> Vec<(String, String)> {
            good.iter()
                .filter(|(key, _)| key != name)
                .cloned()
                .collect()
        };
        let replaced = |name: &str, value: String| -> Vec<(String, String)> {
            good.iter()
                .map(|(key, old)| {
                    (
                        key.clone(),
                        if key == name {
                            value.clone()
                        } else {
                            old.clone()
                        },
                    )
                })
                .collect()
        };
        let cases: Vec<Case> = vec![
            (
                "no signature header",
                without("x-zirv-signature"),
                body.clone(),
            ),
            (
                "malformed value",
                replaced("x-zirv-signature", "sha256=zz".into()),
                body.clone(),
            ),
            (
                "wrong secret",
                signed("zfwh_wrong", at, &body),
                body.clone(),
            ),
            ("altered body", good.clone(), order("o-1", "other")),
            (
                "wrong prefix",
                replaced("x-zirv-signature", format!("sha1={digest}")),
                body.clone(),
            ),
            (
                "no prefix",
                replaced("x-zirv-signature", digest.clone()),
                body.clone(),
            ),
            ("no timestamp", without("x-zirv-timestamp"), body.clone()),
            (
                "bad timestamp",
                replaced("x-zirv-timestamp", "yesterday".into()),
                body.clone(),
            ),
            (
                "stale timestamp",
                signed(&hook.secret, at - 301, &body),
                body.clone(),
            ),
            (
                "future timestamp",
                signed(&hook.secret, at + 301, &body),
                body.clone(),
            ),
            (
                "altered timestamp",
                vec![
                    header("x-zirv-timestamp", (at + 1).to_string()),
                    good[1].clone(),
                ],
                body.clone(),
            ),
        ];

        for (label, headers, payload) in cases {
            let rejected = post_hook(&fx.router, &hook.id, &headers, payload).await;

            assert_eq!(
                rejected.status,
                StatusCode::UNAUTHORIZED,
                "{label}: {}",
                rejected.text
            );
            assert!(is_problem(&rejected), "{label}");
            assert_eq!(rejected.body["code"], "invalid_signature", "{label}");
            for secret in [&hook.secret, &digest] {
                assert!(!rejected.text.contains(secret.as_str()), "{label}");
            }
            assert!(!rejected.text.contains("ada@example.com"), "{label}");
        }
        assert_eq!(fx.count("SELECT count(*) FROM ingest_inbox").await, 0);
        assert!(fx.last_success(&hook).await.is_none());
        // The edge of the window passes.
        let edge = signed(&hook.secret, at - 300, &body);
        assert_eq!(
            post_hook(&fx.router, &hook.id, &edge, body).await.status,
            StatusCode::ACCEPTED
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_body_only_hook_signs_the_raw_body_and_needs_no_timestamp(pool: PgPool) {
        let fx = fixture(&pool).await;
        let hook = fx
            .hook_with(
                "forms",
                json!({"timestamp_header": null, "signature_header": "X-Sig", "signature_prefix": ""}),
            )
            .await;
        let body = order("o-1", "pro");
        let signature = sign(hook.secret.as_bytes(), None, &body);

        let accepted = post_hook(
            &fx.router,
            &hook.id,
            &[("x-sig".into(), signature)],
            body.clone(),
        )
        .await;
        let wrong = post_hook(
            &fx.router,
            &hook.id,
            &[("x-sig".into(), "0".repeat(64))],
            body,
        )
        .await;

        assert_eq!(accepted.status, StatusCode::ACCEPTED, "{}", accepted.text);
        assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn limits_unknown_hooks_and_bad_payloads_answer_their_codes_and_write_nothing(
        pool: PgPool,
    ) {
        let limits = IngestLimits {
            max_webhook_bytes: 2048,
            ..IngestLimits::default()
        };
        let fx = fixture_with(&pool, Some(keyring()), limits).await;
        let hook = fx.hook("orders").await;

        // Declared too large, before any signature work: even an unsigned body is a 413.
        let big = vec![b'x'; 4096];
        let declared = post_hook(&fx.router, &hook.id, &[], big.clone()).await;
        assert_eq!(declared.status, StatusCode::PAYLOAD_TOO_LARGE);
        // Streamed without a declared length.
        let chunks = futures_util::stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 1500])),
            Ok(Bytes::from(vec![b'x'; 1500])),
        ]);
        let request = Request::builder()
            .method("POST")
            .uri(format!("/hooks/{}", hook.id))
            .body(Body::from_stream(chunks))
            .unwrap();
        let streamed = reply(fx.router.clone().oneshot(request).await.unwrap()).await;
        assert_eq!(streamed.status, StatusCode::PAYLOAD_TOO_LARGE);

        // An unknown id, a malformed id, and a hook that is not receiving.
        let body = order("o-1", "pro");
        let unknown = post_hook(
            &fx.router,
            &Uuid::new_v4().to_string(),
            &signed("x", now(), &body),
            body.clone(),
        )
        .await;
        let malformed = post_hook(&fx.router, "not-a-uuid", &[], body.clone()).await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        assert_eq!(malformed.status, StatusCode::NOT_FOUND);
        assert_eq!(fx.map(&hook, false).await.status, StatusCode::OK);
        assert_eq!(
            deliver(&fx, &hook, body.clone()).await.status,
            StatusCode::NOT_FOUND
        );
        sqlx::query("UPDATE streams SET enabled = true, mapping = NULL")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            deliver(&fx, &hook, body.clone()).await.status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(fx.map(&hook, true).await.status, StatusCode::OK);

        // Not a JSON object, signed correctly.
        for payload in [b"[1,2]".to_vec(), b"not json".to_vec(), b"7".to_vec()] {
            let rejected = deliver(&fx, &hook, payload).await;
            assert_eq!(
                rejected.status,
                StatusCode::BAD_REQUEST,
                "{}",
                rejected.text
            );
            assert_eq!(rejected.body["code"], "invalid_payload");
        }
        // No usable dedupe value.
        for payload in [
            json!({"email": "a@example.com"}),
            json!({"id": ""}),
            json!({"id": true}),
            json!({"id": {"a": 1}}),
            json!({"id": "x".repeat(129)}),
            json!({"id": "a\nb"}),
        ] {
            let rejected = deliver(&fx, &hook, payload.to_string().into_bytes()).await;
            assert_eq!(
                rejected.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{}",
                rejected.text
            );
            assert_eq!(rejected.body["code"], "dedupe_missing");
        }
        assert_eq!(fx.count("SELECT count(*) FROM ingest_inbox").await, 0);
        // A number is a usable value.
        let number = deliver(
            &fx,
            &hook,
            json!({"id": 42, "email": "a@example.com"})
                .to_string()
                .into_bytes(),
        )
        .await;
        assert_eq!(number.status, StatusCode::ACCEPTED);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_hook_over_its_rate_answers_429_with_retry_after_and_another_hook_is_unaffected(
        pool: PgPool,
    ) {
        let limits = IngestLimits {
            rate_per_hook: 1,
            burst_per_hook: 2,
            ..IngestLimits::default()
        };
        let fx = fixture_with(&pool, Some(keyring()), limits).await;
        let busy = fx.hook("busy").await;
        let quiet = fx.hook("quiet").await;

        let first = deliver(&fx, &busy, order("1", "a")).await;
        let second = deliver(&fx, &busy, order("2", "a")).await;
        let third = deliver(&fx, &busy, order("3", "a")).await;
        let other = deliver(&fx, &quiet, order("1", "a")).await;

        assert_eq!(first.status, StatusCode::ACCEPTED);
        assert_eq!(second.status, StatusCode::ACCEPTED);
        assert_eq!(third.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(third.headers.contains_key(header::RETRY_AFTER));
        assert_eq!(other.status, StatusCode::ACCEPTED);
        assert_eq!(fx.count("SELECT count(*) FROM ingest_inbox").await, 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn unknown_ids_never_get_a_bucket_so_a_flood_of_them_cannot_grow_the_limiter(
        pool: PgPool,
    ) {
        let state = AppState::new(pool.clone(), Arc::new(String::new));
        zirv_flow_store::migrate(&pool).await.unwrap();
        let router = router(state.clone());

        for _ in 0..20 {
            let missing =
                post_hook(&router, &Uuid::new_v4().to_string(), &[], b"{}".to_vec()).await;
            assert_eq!(missing.status, StatusCode::NOT_FOUND);
        }

        assert_eq!(state.ingest.hook_buckets(), 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_a_keyring_a_receive_is_a_503_and_writes_nothing(pool: PgPool) {
        let fx = fixture(&pool).await;
        let hook = fx.hook("orders").await;
        let bare = router(AppState::new(pool.clone(), Arc::new(String::new)));
        let body = order("o-1", "pro");

        let unavailable =
            post_hook(&bare, &hook.id, &signed(&hook.secret, now(), &body), body).await;

        assert_eq!(unavailable.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(fx.count("SELECT count(*) FROM ingest_inbox").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_database_failure_is_a_503_and_nothing_is_stored(pool: PgPool) {
        let fx = fixture(&pool).await;
        let hook = fx.hook("orders").await;
        let body = order("o-1", "pro");
        sqlx::query("ALTER TABLE ingest_inbox RENAME TO ingest_inbox_away")
            .execute(&pool)
            .await
            .unwrap();

        let failed = deliver(&fx, &hook, body).await;

        assert_eq!(
            failed.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            failed.text
        );
        sqlx::query("ALTER TABLE ingest_inbox_away RENAME TO ingest_inbox")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(fx.count("SELECT count(*) FROM ingest_inbox").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn freshness_is_reported_at_most_once_per_window_and_only_for_accepted_payloads(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let hook = fx.hook("orders").await;

        let first = deliver(&fx, &hook, order("1", "a")).await;
        let reported = fx.last_success(&hook).await;
        sqlx::query("UPDATE streams SET last_success_at = NULL")
            .execute(&pool)
            .await
            .unwrap();
        let second = deliver(&fx, &hook, order("2", "a")).await;

        assert_eq!(first.status, StatusCode::ACCEPTED);
        assert_eq!(second.status, StatusCode::ACCEPTED);
        assert!(reported.is_some());
        assert!(
            fx.last_success(&hook).await.is_none(),
            "the second request wrote the stream"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn create_returns_the_secret_once_and_no_other_response_audit_or_table_row_shows_it(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;

        let created = fx
            .hooks(
                "POST",
                "",
                Some(json!({"name": "orders", "dedupe_field": "id"})),
                Auth::Key(&fx.admin),
            )
            .await;

        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let secret = created.body["secret"].as_str().unwrap().to_owned();
        assert!(secret.starts_with("zfwh_") && secret.len() == 69);
        assert_eq!(created.headers[header::CACHE_CONTROL], "no-store");
        let id = created.body["id"].as_str().unwrap().to_owned();
        assert_eq!(created.body["path"], format!("/hooks/{id}"));
        assert_eq!(created.body["signature_header"], "X-Zirv-Signature");
        assert_eq!(created.body["signature_prefix"], "sha256=");
        assert_eq!(created.body["timestamp_header"], "X-Zirv-Timestamp");
        assert_eq!(created.body["tolerance_seconds"], 300);
        assert_eq!(created.body["receiving"], false);

        let got = fx
            .hooks("GET", &format!("/{id}"), None, Auth::Key(&fx.read))
            .await;
        let listed = fx.hooks("GET", "", None, Auth::Key(&fx.read)).await;
        let rotated = fx
            .hooks(
                "POST",
                &format!("/{id}/rotate-secret"),
                None,
                Auth::Key(&fx.admin),
            )
            .await;
        let new_secret = rotated.body["secret"].as_str().unwrap().to_owned();
        let updated = fx
            .hooks(
                "PATCH",
                &format!("/{id}"),
                Some(json!({"tolerance_seconds": 60})),
                Auth::Key(&fx.admin),
            )
            .await;
        let source = created.body["source_id"].as_str().unwrap().to_owned();
        let mut generic = Vec::new();
        for tail in [
            "/sources".to_owned(),
            format!("/sources/{source}"),
            format!("/sources/{source}/runs"),
        ] {
            generic.push(fx.as_admin("GET", &tail, None).await.text);
        }
        let audit = serde_json::to_string(
            &zirv_flow_store::list_audit(&pool, 200)
                .await
                .unwrap()
                .iter()
                .map(|e| (&e.action, &e.summary))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let actions: Vec<String> = zirv_flow_store::list_audit(&pool, 200)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.action)
            .collect();

        for shown in [&got.text, &listed.text, &updated.text, &audit]
            .into_iter()
            .chain(generic.iter())
        {
            assert!(!shown.contains(&secret));
            assert!(!shown.contains(&new_secret));
        }
        assert!(got.body.get("secret").is_none());
        assert_ne!(secret, new_secret);
        for action in [
            "webhook.created",
            "webhook.secret_rotated",
            "webhook.updated",
            "secret.created",
            "secret.deleted",
        ] {
            assert!(actions.iter().any(|a| a == action), "{action}: {actions:?}");
        }
        // Sealed under the new purpose, and only the new secret is stored.
        let purposes: Vec<String> = sqlx::query_scalar("SELECT purpose FROM encrypted_secrets")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(purposes, ["webhook_secret"]);
        tables_hold_no(&pool, &[&secret, &new_secret]).await;
        the_scan_finds_a_planted(&pool, &[&secret, &new_secret]).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn rotating_makes_the_old_secret_stop_working_at_once(pool: PgPool) {
        let fx = fixture(&pool).await;
        let hook = fx.hook("orders").await;
        assert_eq!(
            deliver(&fx, &hook, order("1", "a")).await.status,
            StatusCode::ACCEPTED
        );

        let rotated = fx
            .hooks(
                "POST",
                &format!("/{}/rotate-secret", hook.id),
                None,
                Auth::Key(&fx.admin),
            )
            .await;
        let fresh = Hook {
            secret: rotated.body["secret"].as_str().unwrap().to_owned(),
            id: hook.id.clone(),
            source: hook.source.clone(),
        };

        assert_eq!(
            deliver(&fx, &hook, order("2", "a")).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            deliver(&fx, &fresh, order("2", "a")).await.status,
            StatusCode::ACCEPTED
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_admins_change_hooks_members_and_readers_read_and_a_session_needs_its_csrf_token(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let hook = fx.hook("orders").await;
        let body = Some(json!({"name": "other", "dedupe_field": "id"}));

        let member_write = fx
            .hooks("POST", "", body.clone(), Auth::Session(&fx.member))
            .await;
        let reader_write = fx
            .hooks("POST", "", body.clone(), Auth::Key(&fx.read))
            .await;
        let member_patch = fx
            .hooks(
                "PATCH",
                &format!("/{}", hook.id),
                Some(json!({"enabled": false})),
                Auth::Session(&fx.member),
            )
            .await;
        let member_rotate = fx
            .hooks(
                "POST",
                &format!("/{}/rotate-secret", hook.id),
                None,
                Auth::Session(&fx.member),
            )
            .await;
        let member_delete = fx
            .hooks(
                "DELETE",
                &format!("/{}", hook.id),
                None,
                Auth::Session(&fx.member),
            )
            .await;
        let member_read = fx
            .hooks(
                "GET",
                &format!("/{}", hook.id),
                None,
                Auth::Session(&fx.member),
            )
            .await;
        let member_list = fx.hooks("GET", "", None, Auth::Session(&fx.member)).await;
        let anonymous = fx.hooks("GET", "", None, Auth::Anonymous).await;
        let no_csrf = fx.hooks("GET", "", None, Auth::NoCsrf(&fx.member)).await;
        let no_csrf_write = fx.hooks("POST", "", body, Auth::NoCsrf(&fx.member)).await;

        for denied in [
            &member_write,
            &reader_write,
            &member_patch,
            &member_rotate,
            &member_delete,
        ] {
            assert_eq!(denied.status, StatusCode::FORBIDDEN, "{:?}", denied.body);
        }
        assert_eq!(member_read.status, StatusCode::OK);
        assert_eq!(member_list.status, StatusCode::OK);
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(no_csrf.status, StatusCode::OK);
        assert_eq!(no_csrf_write.status, StatusCode::FORBIDDEN);
        assert_eq!(fx.count("SELECT count(*) FROM webhooks").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_hook_of_another_workspace_is_not_found_by_any_admin_route(pool: PgPool) {
        let fx = fixture(&pool).await;
        let hook = fx.hook("orders").await;
        let theirs = format!("/api/v1/workspaces/{}/webhooks", fx.other_ws.as_uuid());
        let auth = || Auth::Key(&fx.other_admin);

        let read = send(
            &fx.router,
            "GET",
            &format!("{theirs}/{}", hook.id),
            None,
            auth(),
        )
        .await;
        let patch = send(
            &fx.router,
            "PATCH",
            &format!("{theirs}/{}", hook.id),
            Some(json!({"enabled": false})),
            auth(),
        )
        .await;
        let rotate = send(
            &fx.router,
            "POST",
            &format!("{theirs}/{}/rotate-secret", hook.id),
            None,
            auth(),
        )
        .await;
        let delete = send(
            &fx.router,
            "DELETE",
            &format!("{theirs}/{}", hook.id),
            None,
            auth(),
        )
        .await;
        let list = send(&fx.router, "GET", &theirs, None, auth()).await;
        // Another workspace's key cannot reach this workspace's routes at all.
        let reach = send(&fx.router, "GET", &fx.path("/webhooks"), None, auth()).await;

        for missing in [&read, &patch, &rotate, &delete] {
            assert_eq!(missing.status, StatusCode::NOT_FOUND);
        }
        assert_eq!(list.body, json!([]));
        assert!(matches!(
            reach.status,
            StatusCode::NOT_FOUND | StatusCode::FORBIDDEN
        ));
        assert_eq!(fx.count("SELECT count(*) FROM webhooks").await, 1);
        assert_eq!(
            deliver(&fx, &hook, order("1", "a")).await.status,
            StatusCode::ACCEPTED
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_name_in_use_is_a_409_and_a_bad_scheme_lists_every_broken_field(pool: PgPool) {
        let fx = fixture(&pool).await;
        fx.hook("orders").await;

        let taken = fx
            .hooks(
                "POST",
                "",
                Some(json!({"name": "orders", "dedupe_field": "id"})),
                Auth::Key(&fx.admin),
            )
            .await;
        let bad = fx
            .hooks(
                "POST",
                "",
                Some(json!({
                    "name": "scheme",
                    "dedupe_field": "",
                    "signature_header": "bad header",
                    "timestamp_header": "bad header",
                    "tolerance_seconds": 0,
                })),
                Auth::Key(&fx.admin),
            )
            .await;
        let same = fx
            .hooks(
                "POST",
                "",
                Some(json!({"name": "x", "dedupe_field": "id", "timestamp_header": "x-zirv-signature"})),
                Auth::Key(&fx.admin),
            )
            .await;

        assert_eq!(taken.status, StatusCode::CONFLICT);
        assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(same.status, StatusCode::UNPROCESSABLE_ENTITY);
        let fields: Vec<&str> = bad.body["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["field"].as_str().unwrap())
            .collect();
        assert!(fields.contains(&"tolerance_seconds"), "{fields:?}");
        assert_eq!(fx.count("SELECT count(*) FROM webhooks").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn changing_the_scheme_and_disabling_apply_to_the_next_request(pool: PgPool) {
        let fx = fixture(&pool).await;
        let hook = fx.hook("orders").await;

        let changed = fx
            .hooks(
                "PATCH",
                &format!("/{}", hook.id),
                Some(json!({"name": "renamed", "signature_header": "X-Sig", "timestamp_header": null, "signature_prefix": ""})),
                Auth::Key(&fx.admin),
            )
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.body);
        assert_eq!(changed.body["name"], "renamed");
        assert_eq!(changed.body["timestamp_header"], Value::Null);
        let body = order("1", "a");
        let old_scheme = deliver(&fx, &hook, body.clone()).await;
        let new_scheme = post_hook(
            &fx.router,
            &hook.id,
            &[("x-sig".into(), sign(hook.secret.as_bytes(), None, &body))],
            body.clone(),
        )
        .await;
        let off = fx
            .hooks(
                "PATCH",
                &format!("/{}", hook.id),
                Some(json!({"enabled": false})),
                Auth::Key(&fx.admin),
            )
            .await;
        let after_off = post_hook(
            &fx.router,
            &hook.id,
            &[("x-sig".into(), sign(hook.secret.as_bytes(), None, &body))],
            body,
        )
        .await;
        let empty = fx
            .hooks(
                "PATCH",
                &format!("/{}", hook.id),
                Some(json!({})),
                Auth::Key(&fx.admin),
            )
            .await;

        assert_eq!(old_scheme.status, StatusCode::UNAUTHORIZED);
        assert_eq!(new_scheme.status, StatusCode::ACCEPTED);
        assert_eq!(off.body["receiving"], false);
        assert_eq!(after_off.status, StatusCode::NOT_FOUND);
        assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_a_hook_removes_its_source_stream_secret_and_queued_payloads(pool: PgPool) {
        let fx = fixture(&pool).await;
        let hook = fx.hook("orders").await;
        assert_eq!(
            deliver(&fx, &hook, order("1", "a")).await.status,
            StatusCode::ACCEPTED
        );

        let deleted = fx
            .hooks(
                "DELETE",
                &format!("/{}", hook.id),
                None,
                Auth::Key(&fx.admin),
            )
            .await;

        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        for table in [
            "webhooks",
            "sources",
            "streams",
            "encrypted_secrets",
            "ingest_inbox",
        ] {
            let sql: &'static str = match table {
                "webhooks" => "SELECT count(*) FROM webhooks",
                "sources" => "SELECT count(*) FROM sources",
                "streams" => "SELECT count(*) FROM streams",
                "encrypted_secrets" => {
                    "SELECT count(*) FROM encrypted_secrets WHERE purpose = 'webhook_secret'"
                }
                _ => "SELECT count(*) FROM ingest_inbox",
            };
            assert_eq!(fx.count(sql).await, 0, "{table}");
        }
        assert_eq!(
            deliver(&fx, &hook, order("2", "a")).await.status,
            StatusCode::NOT_FOUND
        );
        let actions: Vec<String> = zirv_flow_store::list_audit(&pool, 200)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.action)
            .collect();
        assert!(actions.contains(&"webhook.deleted".to_owned()));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_generic_source_routes_refuse_a_webhook_source_and_it_never_gets_a_run(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let hook = fx.hook("orders").await;
        let source = format!("/sources/{}", hook.source);

        let create = fx
            .as_admin(
                "POST",
                "/sources",
                Some(json!({"kind": "webhook", "name": "x", "config": {}})),
            )
            .await;
        let patch = fx
            .as_admin("PATCH", &source, Some(json!({"name": "x"})))
            .await;
        let delete = fx.as_admin("DELETE", &source, None).await;
        let run = fx
            .as_admin("POST", &format!("{source}/runs"), Some(json!({})))
            .await;
        let named = fx
            .as_admin(
                "POST",
                &format!("{source}/runs"),
                Some(json!({"stream": "payload"})),
            )
            .await;
        let stream = fx.as_admin("GET", &format!("{source}/streams"), None).await;
        let listed = fx.as_admin("GET", "/sources", None).await;

        for refused in [&create, &patch, &delete] {
            assert_eq!(refused.status, StatusCode::CONFLICT, "{:?}", refused.body);
            assert_eq!(refused.body["code"], "managed_by_webhooks");
        }
        for refused in [&run, &named] {
            assert_eq!(refused.status, StatusCode::CONFLICT, "{:?}", refused.body);
            assert_eq!(refused.body["code"], "push_source");
        }
        assert_eq!(stream.body[0]["name"], "payload");
        assert_eq!(listed.body[0]["kind"], "webhook");
        assert_eq!(fx.count("SELECT count(*) FROM sync_runs").await, 0);
        assert_eq!(fx.count("SELECT count(*) FROM webhooks").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_push_mapping_needs_no_key_and_refuses_a_key_column_and_a_delete_signal(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let hook = fx.hook("orders").await;
        let put = |mapping: Value| fx.put_mapping(&hook, mapping);

        let keyed = put(json!({"key_column": "id", "columns": mapping()["columns"].clone()})).await;
        let deleting = put(
            json!({"columns": mapping()["columns"].clone(), "delete_when": {"column": "gone"}}),
        )
        .await;
        let nested = put(json!({"columns": [
            {"column": "user.email", "target": {"type": "identifier", "kind": "email"}},
            {"column": "no.such.field", "target": {"type": "trait", "key": "plan"}}
        ]}))
        .await;
        let unknown_trait = put(json!({"columns": [
            {"column": "email", "target": {"type": "identifier", "kind": "email"}},
            {"column": "x", "target": {"type": "trait", "key": "nope"}}
        ]}))
        .await;
        let stream = fx
            .as_admin("GET", &format!("/sources/{}/streams", hook.source), None)
            .await;

        assert_eq!(keyed.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(keyed.body["code"], "key_column_not_supported");
        assert_eq!(deleting.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(deleting.body["code"], "delete_signal_unsupported");
        assert_eq!(nested.status, StatusCode::OK, "{:?}", nested.body);
        assert_eq!(unknown_trait.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(stream.body[0]["primary_key"], json!([]));
        // The mapping from the fixture has an event and no key: it saved.
        assert_eq!(fx.map(&hook, true).await.status, StatusCode::OK);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_maps_a_pasted_sample_like_the_drain_and_writes_nothing(pool: PgPool) {
        let fx = fixture(&pool).await;
        let hook = fx.hook("orders").await;
        let tail = format!("/sources/{}/streams/payload/preview", hook.source);
        let before = zirv_flow_store::list_audit(&pool, 200).await.unwrap().len();

        let shown = fx
            .as_admin(
                "POST",
                &tail,
                Some(json!({"sample": [
                    {"id": "1", "email": "ada@example.com", "plan": "pro", "order": {"total": 3}},
                    {"id": "2", "plan": "free"}
                ]})),
            )
            .await;
        let none = fx.as_admin("POST", &tail, Some(json!({}))).await;
        let empty = fx
            .as_admin("POST", &tail, Some(json!({"sample": []})))
            .await;
        let too_many = fx
            .as_admin("POST", &tail, Some(json!({"sample": vec![json!({}); 21]})))
            .await;
        let not_objects = fx
            .as_admin("POST", &tail, Some(json!({"sample": [1]})))
            .await;
        let huge = fx
            .as_admin(
                "POST",
                &tail,
                Some(json!({"sample": [{"x": "y".repeat(40_000)}, {"x": "y".repeat(40_000)}]})),
            )
            .await;
        let audited = zirv_flow_store::list_audit(&pool, 200).await.unwrap().len();
        assert_eq!(audited, before, "a preview is not audited");
        let pull = fx.as_admin("POST", "/sources", Some(json!({"kind": "fake", "name": "crm", "config": {}, "credential": {"token": "ok"}}))).await;
        let pull_id = pull.body["id"].as_str().unwrap().to_owned();
        fx.as_admin("POST", &format!("/sources/{pull_id}/discover"), None)
            .await;
        let refused = fx
            .as_admin(
                "POST",
                &format!("/sources/{pull_id}/streams/stream_1/preview"),
                Some(json!({"sample": [{}]})),
            )
            .await;

        assert_eq!(shown.status, StatusCode::OK, "{:?}", shown.body);
        assert_eq!(shown.body["records"][0]["key"], "1");
        assert_eq!(shown.body["records"][0]["result"], "mapped");
        assert_eq!(shown.body["records"][0]["traits"]["plan"], "pro");
        assert_eq!(
            shown.body["records"][0]["event"]["properties"]["total"],
            "3"
        );
        assert_eq!(shown.body["records"][1]["result"], "rejected");
        assert_eq!(
            shown.body["records"][1]["errors"][0]["reason"],
            "missing_identifier"
        );
        assert_eq!(none.body["code"], "sample_required");
        for refused in [&empty, &too_many, &not_objects, &huge] {
            assert_eq!(
                refused.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{:?}",
                refused.body
            );
        }
        assert_eq!(fx.count("SELECT count(*) FROM people").await, 0);
        assert_eq!(fx.count("SELECT count(*) FROM ingest_inbox").await, 0);
        assert_eq!(fx.count("SELECT count(*) FROM ingest_rejects").await, 0);
        assert_eq!(refused.body["code"], "sample_not_supported");
    }

    #[test]
    fn the_openapi_document_describes_the_public_receive_route_without_a_security_scheme() {
        let document = serde_json::to_value(crate::openapi()).unwrap();

        let operation = &document["paths"]["/hooks/{id}"]["post"];
        assert_eq!(operation["operationId"], "receive_webhook");
        assert_eq!(operation["security"], json!([]));
        assert!(operation["responses"]["401"].is_object());
        assert!(
            document["paths"]["/api/v1/workspaces/{ws}/webhooks/{id}/rotate-secret"].is_object()
        );
    }
}
