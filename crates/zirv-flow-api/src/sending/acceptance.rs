//! The sending-domain and sender-identity API end to end, driven through the router.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode, header};
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use zirv_flow_core::sending::{dkim_public_key_of, dkim_record_public_key};
    use zirv_flow_core::{
        HmacRoot, Keyring, MasterKey, Role, Scope, SecretId, SecretPurpose, Slug, WorkspaceId,
        WorkspaceName, deployment_key_aad, seal,
    };
    use zirv_flow_store::secrets;

    use crate::admin::{self, Context};
    use crate::{AppState, router};

    const PASSWORD: &str = "correct horse battery";

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Value,
        raw: String,
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
        keyring: Arc<Keyring>,
        ws: WorkspaceId,
        other: WorkspaceId,
        read: String,
        admin: String,
        /// An admin key of another workspace.
        stranger: String,
        member: Login,
        /// Every response body seen, as text, to scan for key material.
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
            raw: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn fixture_with(pool: &PgPool, with_keyring: bool) -> Fixture {
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
        let keyring = Arc::new(Keyring::new(MasterKey::generate(), None).unwrap());
        let mut state = AppState::new(pool.clone(), Arc::new(String::new));
        if with_keyring {
            let root = HmacRoot::bootstrap_from(keyring.current()).to_secret_bytes();
            let sealed = seal(
                &keyring,
                root.expose(),
                &deployment_key_aad(secrets::HMAC_ROOT),
            );
            secrets::insert_hmac_root_if_absent(pool, &sealed)
                .await
                .unwrap();
            state = state.with_keyring(Arc::clone(&keyring));
        }
        let router = router(state);
        let key = |workspace: WorkspaceId, scopes: &'static [Scope]| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), workspace, "sending", scopes)
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
        Fixture {
            pool: pool.clone(),
            router,
            keyring,
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
        fixture_with(pool, true).await
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
            self.seen.lock().unwrap().push(reply.raw.clone());
            reply
        }

        async fn as_admin(&self, method: &str, tail: &str, body: Option<Value>) -> Reply {
            self.send(method, tail, body, Auth::Key(&self.admin)).await
        }

        async fn domain(&self, domain: &str) -> String {
            let reply = self
                .as_admin("POST", "/sending-domains", Some(json!({"domain": domain})))
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().to_owned()
        }

        async fn identity(&self, domain: &str, address: &str) -> Reply {
            self.as_admin(
                "POST",
                "/sender-identities",
                Some(json!({"domain_id": domain, "from_name": "News", "from_address": address})),
            )
            .await
        }

        async fn audit_actions_like(&self, prefix: &str) -> Vec<String> {
            let mut entries = zirv_flow_store::list_audit(&self.pool, 200).await.unwrap();
            entries.reverse();
            entries
                .into_iter()
                .map(|entry| entry.action)
                .filter(|action| action.starts_with(prefix))
                .collect()
        }

        async fn audit_actions(&self) -> Vec<String> {
            let mut entries = zirv_flow_store::list_audit(&self.pool, 200).await.unwrap();
            entries.reverse();
            entries
                .into_iter()
                .map(|entry| entry.action)
                .filter(|action| {
                    action.starts_with("sending_domain.")
                        || action.starts_with("sender_identity.")
                        || action.starts_with("secret.")
                })
                .collect()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_sending_endpoint_is_admin_only_and_no_access_is_404(pool: PgPool) {
        let fx = fixture(&pool).await;
        let domain = fx.domain("example.com").await;
        let identity = fx.identity(&domain, "news@example.com").await.body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let endpoints: Vec<(&str, String, Option<Value>)> = vec![
            ("GET", "/sending-domains".into(), None),
            (
                "POST",
                "/sending-domains".into(),
                Some(json!({"domain": "other.example.com"})),
            ),
            ("GET", format!("/sending-domains/{domain}"), None),
            ("POST", format!("/sending-domains/{domain}/verify"), None),
            ("DELETE", format!("/sending-domains/{domain}"), None),
            ("GET", "/sender-identities".into(), None),
            (
                "POST",
                "/sender-identities".into(),
                Some(
                    json!({"domain_id": domain, "from_name": "X", "from_address": "x@example.com"}),
                ),
            ),
            ("GET", format!("/sender-identities/{identity}"), None),
            (
                "PATCH",
                format!("/sender-identities/{identity}"),
                Some(json!({"from_name": "Y"})),
            ),
            ("DELETE", format!("/sender-identities/{identity}"), None),
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
        let domains = fx.as_admin("GET", "/sending-domains", None).await;
        let identities = fx.as_admin("GET", "/sender-identities", None).await;
        assert_eq!(domains.body.as_array().unwrap().len(), 1);
        assert_eq!(identities.body.as_array().unwrap().len(), 1);
        let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(jobs, 1, "only the create queued a check");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_admin_of_another_workspace_cannot_reach_a_domain_or_identity_by_id(pool: PgPool) {
        let fx = fixture(&pool).await;
        let theirs = call(
            &fx.router,
            "POST",
            &format!("/api/v1/workspaces/{}/sending-domains", fx.other.as_uuid()),
            Some(json!({"domain": "theirs.example.com"})),
            Auth::Key(&fx.stranger),
        )
        .await;
        assert_eq!(theirs.status, StatusCode::CREATED, "{:?}", theirs.body);
        let domain = theirs.body["id"].as_str().unwrap().to_owned();
        let ours = fx.domain("example.com").await;

        let attempts = [
            ("GET", format!("/sending-domains/{domain}"), None),
            ("POST", format!("/sending-domains/{domain}/verify"), None),
            ("DELETE", format!("/sending-domains/{domain}"), None),
        ];
        for (method, tail, body) in attempts {
            let reply = fx.as_admin(method, &tail, body).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {tail}");
        }
        // An identity cannot be made on a domain of another workspace either.
        let on_theirs = fx.identity(&domain, "news@theirs.example.com").await;
        assert_eq!(on_theirs.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            on_theirs.body["detail"]
                .as_str()
                .unwrap()
                .contains("domain_id")
        );
        let listed = fx.as_admin("GET", "/sending-domains", None).await;
        assert_eq!(listed.body.as_array().unwrap().len(), 1);
        assert_eq!(listed.body[0]["id"], ours.as_str());
        let their_secrets: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sending_domains WHERE workspace_id = $1")
                .bind(fx.other.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(their_secrets, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn creating_a_domain_returns_the_records_to_publish_and_queues_a_check(pool: PgPool) {
        let fx = fixture(&pool).await;

        let created = fx
            .as_admin(
                "POST",
                "/sending-domains",
                Some(json!({"domain": " Example.COM ", "selector": "Mail1",
                            "spf_mechanism": "include:spf.relay.example"})),
            )
            .await;

        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        assert_eq!(created.body["domain"], "example.com");
        assert_eq!(created.body["selector"], "mail1");
        assert_eq!(created.body["status"], "unverified");
        assert!(created.body["verified_at"].is_null());
        let records = created.body["records"].as_array().unwrap();
        let summary: Vec<_> = records
            .iter()
            .map(|record| {
                (
                    record["kind"].as_str().unwrap(),
                    record["host"].as_str().unwrap(),
                    record["status"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("dkim", "mail1._domainkey.example.com", "pending"),
                ("spf", "example.com", "pending"),
                ("dmarc", "_dmarc.example.com", "pending"),
            ]
        );
        assert!(
            records[0]["expected_text"]
                .as_str()
                .unwrap()
                .starts_with("v=DKIM1; k=rsa; p=")
        );
        assert_eq!(
            records[1]["expected_text"],
            "v=spf1 include:spf.relay.example ~all"
        );
        assert_eq!(records[2]["expected_text"], "v=DMARC1; p=none");
        let queued: Vec<(String, Option<uuid::Uuid>, Value)> =
            sqlx::query_as("SELECT kind, workspace_id, payload FROM jobs")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].0, "sending_domain.verify");
        assert_eq!(queued[0].1, Some(fx.ws.as_uuid()));
        assert_eq!(queued[0].2["domain_id"], created.body["id"]);
        assert_eq!(
            fx.audit_actions().await,
            ["secret.created", "sending_domain.created"]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_published_dkim_record_is_the_public_key_of_the_sealed_private_key_which_is_never_returned(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.domain("example.com").await;
        let read = fx
            .as_admin("GET", &format!("/sending-domains/{id}"), None)
            .await;
        fx.as_admin("GET", "/sending-domains", None).await;
        fx.as_admin("POST", &format!("/sending-domains/{id}/verify"), None)
            .await;

        let (secret, purpose, public): (uuid::Uuid, String, String) = sqlx::query_as(
            "SELECT d.dkim_secret_id, s.purpose, d.dkim_public_key \
             FROM sending_domains d JOIN encrypted_secrets s ON s.id = d.dkim_secret_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(purpose, "dkim_key");
        let private = secrets::open_json(
            &pool,
            &fx.keyring,
            fx.ws,
            SecretId::from_uuid(secret),
            SecretPurpose::DkimKey,
        )
        .await
        .unwrap();
        let from_private = dkim_public_key_of(private.expose()).unwrap();
        let published =
            dkim_record_public_key(read.body["records"][0]["expected_text"].as_str().unwrap())
                .unwrap();
        assert_eq!(published, from_private);
        assert_eq!(published, public);

        // No response, audit entry or table cell holds the private key, in DER or in base64, or
        // names the secret that holds it.
        let der = private.expose().to_vec();
        let encoded = STANDARD.encode(&der);
        let seen = fx.seen.lock().unwrap().join("\n");
        assert!(
            !seen.contains(&encoded[..64]),
            "the private key was returned"
        );
        assert!(
            !seen.contains(&secret.to_string()),
            "the secret id was returned"
        );
        assert!(!seen.contains("dkim_secret_id") && !seen.contains("PRIVATE"));
        let audit = zirv_flow_store::list_audit(&pool, 200)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| format!("{entry:?}"))
            .collect::<String>();
        assert!(!audit.contains(&encoded[..64]));
        let jobs: String =
            sqlx::query_scalar("SELECT coalesce(string_agg(payload::text, ''), '') FROM jobs")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!jobs.contains(&encoded[..64]));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_domain_needs_the_master_key_to_seal_its_key_and_stores_nothing_without_it(
        pool: PgPool,
    ) {
        let fx = fixture_with(&pool, false).await;

        let created = fx
            .as_admin(
                "POST",
                "/sending-domains",
                Some(json!({"domain": "example.com"})),
            )
            .await;

        assert_eq!(
            created.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{:?}",
            created.body
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM sending_domains")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn invalid_requests_are_refused_with_the_field_and_a_taken_domain_is_a_conflict(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        fx.domain("example.com").await;
        let post = |body: Value| fx.as_admin("POST", "/sending-domains", Some(body));

        let bad_domain = post(json!({"domain": "localhost"})).await;
        let bad_selector = post(json!({"domain": "a.example.com", "selector": "a.b"})).await;
        let bad_mechanism =
            post(json!({"domain": "a.example.com", "spf_mechanism": "redirect=x.com"})).await;
        let taken = post(json!({"domain": "EXAMPLE.com."})).await;
        let internal = post(json!({"domain": "vault.corp"})).await;
        let private_ip = post(json!({"domain": "10.0.0.5"})).await;

        for (reply, field) in [
            (&bad_domain, "domain"),
            (&internal, "domain"),
            (&private_ip, "domain"),
            (&bad_selector, "selector"),
            (&bad_mechanism, "spf_mechanism"),
        ] {
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{:?}",
                reply.body
            );
            assert!(reply.body["detail"].as_str().unwrap().contains(field));
        }
        assert_eq!(taken.status, StatusCode::CONFLICT);
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM encrypted_secrets WHERE purpose = 'dkim_key'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rows, 1, "a refused create seals nothing");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_recheck_is_accepted_queues_one_job_and_audits(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.domain("example.com").await;
        sqlx::query("DELETE FROM jobs")
            .execute(&pool)
            .await
            .unwrap();

        let first = fx
            .as_admin("POST", &format!("/sending-domains/{id}/verify"), None)
            .await;
        let second = fx
            .as_admin("POST", &format!("/sending-domains/{id}/verify"), None)
            .await;

        assert_eq!(first.status, StatusCode::ACCEPTED, "{:?}", first.body);
        assert_eq!(second.status, StatusCode::ACCEPTED);
        assert_eq!(first.body["id"], id.as_str());
        let jobs: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'sending_domain.verify'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(jobs, 1, "a waiting check serves the second request");
        let missing = fx
            .as_admin(
                "POST",
                &format!("/sending-domains/{}/verify", uuid::Uuid::now_v7()),
                None,
            )
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        let audit = fx.audit_actions().await;
        assert_eq!(
            audit
                .iter()
                .filter(|a| *a == "sending_domain.verification_requested")
                .count(),
            2
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_identity_is_made_only_on_its_domain_and_changes_stay_on_it(pool: PgPool) {
        let fx = fixture(&pool).await;
        let domain = fx.domain("example.com").await;

        let off = fx.identity(&domain, "news@other.com").await;
        let sub = fx.identity(&domain, "news@mail.example.com").await;
        let bad = fx.identity(&domain, "not an address").await;
        let unknown = fx
            .identity(&uuid::Uuid::now_v7().to_string(), "news@example.com")
            .await;
        let created = fx
            .as_admin(
                "POST",
                "/sender-identities",
                Some(json!({"domain_id": domain, "from_name": " Example News ",
                            "from_address": "News@Example.COM", "reply_to": "help@elsewhere.org"})),
            )
            .await;

        for reply in [&off, &sub, &bad, &unknown] {
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{:?}",
                reply.body
            );
        }
        assert!(
            off.body["detail"]
                .as_str()
                .unwrap()
                .contains("from_address")
        );
        assert!(
            bad.body["detail"]
                .as_str()
                .unwrap()
                .contains("from_address")
        );
        assert!(
            unknown.body["detail"]
                .as_str()
                .unwrap()
                .contains("domain_id")
        );
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        assert_eq!(created.body["from_address"], "news@example.com");
        assert_eq!(created.body["from_name"], "Example News");
        assert_eq!(created.body["reply_to"], "help@elsewhere.org");
        assert_eq!(created.body["archived"], false);
        let id = created.body["id"].as_str().unwrap().to_owned();

        let taken = fx.identity(&domain, "news@example.com").await;
        assert_eq!(taken.status, StatusCode::CONFLICT);
        let moved = fx
            .as_admin(
                "PATCH",
                &format!("/sender-identities/{id}"),
                Some(json!({"from_address": "news@other.com"})),
            )
            .await;
        assert_eq!(moved.status, StatusCode::UNPROCESSABLE_ENTITY);
        let changed = fx
            .as_admin(
                "PATCH",
                &format!("/sender-identities/{id}"),
                Some(
                    json!({"from_name": "Newsroom", "from_address": "press@example.com",
                            "reply_to": null}),
                ),
            )
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.body);
        assert_eq!(changed.body["from_address"], "press@example.com");
        assert!(changed.body["reply_to"].is_null());
        let empty = fx
            .as_admin(
                "PATCH",
                &format!("/sender-identities/{id}"),
                Some(json!({})),
            )
            .await;
        assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);
        let listed = fx
            .as_admin(
                "GET",
                &format!("/sender-identities?domain_id={domain}"),
                None,
            )
            .await;
        assert_eq!(listed.body.as_array().unwrap().len(), 1);
        let other_domain = fx
            .as_admin(
                "GET",
                &format!("/sender-identities?domain_id={}", uuid::Uuid::now_v7()),
                None,
            )
            .await;
        assert_eq!(other_domain.body, json!([]));
        let bad_query = fx
            .as_admin("GET", "/sender-identities?domain_id=nope", None)
            .await;
        assert_eq!(bad_query.status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn archiving_an_identity_frees_its_address_and_freezes_it(pool: PgPool) {
        let fx = fixture(&pool).await;
        let domain = fx.domain("example.com").await;
        let id = fx.identity(&domain, "news@example.com").await.body["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let archived = fx
            .as_admin("DELETE", &format!("/sender-identities/{id}"), None)
            .await;

        assert_eq!(archived.status, StatusCode::NO_CONTENT);
        let listed = fx.as_admin("GET", "/sender-identities", None).await;
        assert_eq!(listed.body, json!([]));
        let with_archived = fx
            .as_admin("GET", "/sender-identities?archived=true", None)
            .await;
        assert_eq!(with_archived.body[0]["archived"], true);
        for (method, body) in [("PATCH", Some(json!({"from_name": "Z"}))), ("DELETE", None)] {
            let reply = fx
                .as_admin(method, &format!("/sender-identities/{id}"), body)
                .await;
            assert_eq!(reply.status, StatusCode::CONFLICT, "{method}");
            assert_eq!(reply.body["code"], "sender_identity_archived", "{method}");
        }
        assert_eq!(
            fx.identity(&domain, "news@example.com").await.status,
            StatusCode::CREATED
        );
        let missing = fx
            .as_admin(
                "GET",
                &format!("/sender-identities/{}", uuid::Uuid::now_v7()),
                None,
            )
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_domain_with_a_live_identity_cannot_be_deleted_and_deleting_it_removes_the_key(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let domain = fx.domain("example.com").await;
        let identity = fx.identity(&domain, "news@example.com").await.body["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let refused = fx
            .as_admin("DELETE", &format!("/sending-domains/{domain}"), None)
            .await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert_eq!(refused.body["code"], "sending_domain_in_use");
        fx.as_admin("DELETE", &format!("/sender-identities/{identity}"), None)
            .await;
        let deleted = fx
            .as_admin("DELETE", &format!("/sending-domains/{domain}"), None)
            .await;

        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        let gone = fx
            .as_admin("GET", &format!("/sending-domains/{domain}"), None)
            .await;
        assert_eq!(gone.status, StatusCode::NOT_FOUND);
        let keys: i64 =
            sqlx::query_scalar("SELECT count(*) FROM encrypted_secrets WHERE purpose = 'dkim_key'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(keys, 0, "the sealed key goes with the domain");
        assert_eq!(
            fx.audit_actions().await,
            [
                "secret.created",
                "sending_domain.created",
                "sender_identity.created",
                "sender_identity.archived",
                "secret.deleted",
                "sending_domain.deleted",
            ]
        );
    }

    fn settings_body() -> Value {
        json!({
            "frequency_caps": [
                {"channel": "email", "window": "P1D", "limit": 3, "mode": "skip"},
                {"channel": "email", "window": "PT6H", "limit": 1, "mode": "defer"},
            ],
            "quiet_hours": {"start": "21:00", "end": "08:00", "days": ["fri", "mon"]},
        })
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn send_settings_are_read_by_readers_and_replaced_only_by_admins(pool: PgPool) {
        let fx = fixture(&pool).await;
        let empty = json!({"frequency_caps": [], "quiet_hours": null});

        let anonymous = fx
            .send("GET", "/sending/settings", None, Auth::Anonymous)
            .await;
        let stranger = fx
            .send("GET", "/sending/settings", None, Auth::Key(&fx.stranger))
            .await;
        let read = fx
            .send("GET", "/sending/settings", None, Auth::Key(&fx.read))
            .await;
        let member = fx
            .send("GET", "/sending/settings", None, Auth::Session(&fx.member))
            .await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(stranger.status, StatusCode::NOT_FOUND);
        assert_eq!((read.status, &read.body), (StatusCode::OK, &empty));
        assert_eq!((member.status, &member.body), (StatusCode::OK, &empty));

        let anonymous = fx
            .send(
                "PUT",
                "/sending/settings",
                Some(settings_body()),
                Auth::Anonymous,
            )
            .await;
        let read = fx
            .send(
                "PUT",
                "/sending/settings",
                Some(settings_body()),
                Auth::Key(&fx.read),
            )
            .await;
        let member = fx
            .send(
                "PUT",
                "/sending/settings",
                Some(settings_body()),
                Auth::Session(&fx.member),
            )
            .await;
        let stranger = fx
            .send(
                "PUT",
                "/sending/settings",
                Some(settings_body()),
                Auth::Key(&fx.stranger),
            )
            .await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(read.status, StatusCode::FORBIDDEN);
        assert_eq!(member.status, StatusCode::FORBIDDEN);
        assert_eq!(stranger.status, StatusCode::NOT_FOUND);
        assert_eq!(
            fx.as_admin("GET", "/sending/settings", None).await.body,
            empty,
            "no refused call changed anything"
        );

        let put = fx
            .as_admin("PUT", "/sending/settings", Some(settings_body()))
            .await;
        assert_eq!(put.status, StatusCode::OK, "{:?}", put.body);
        let expected = json!({
            "frequency_caps": [
                {"channel": "email", "window": "P1D", "limit": 3, "mode": "skip"},
                {"channel": "email", "window": "PT6H", "limit": 1, "mode": "defer"},
            ],
            "quiet_hours": {"start": "21:00", "end": "08:00", "days": ["mon", "fri"]},
        });
        assert_eq!(put.body, expected);
        let read = fx
            .send("GET", "/sending/settings", None, Auth::Key(&fx.read))
            .await;
        assert_eq!(read.body, expected);

        // A second PUT replaces both: nothing of the first is left.
        let replaced = fx
            .as_admin("PUT", "/sending/settings", Some(empty.clone()))
            .await;
        assert_eq!(replaced.body, empty);
        assert_eq!(
            fx.send("GET", "/sending/settings", None, Auth::Key(&fx.read))
                .await
                .body,
            empty
        );
        let audit = fx.audit_actions_like("send_settings.").await;
        assert_eq!(audit, ["send_settings.updated", "send_settings.updated"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn invalid_send_settings_are_refused_at_their_json_pointers_and_change_nothing(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        fx.as_admin("PUT", "/sending/settings", Some(settings_body()))
            .await;
        let cap = |window: &str, limit: i64, mode: &str| json!({"channel": "email", "window": window, "limit": limit, "mode": mode});
        let mut sms = cap("P1D", 1, "skip");
        sms["channel"] = json!("sms");
        let quiet = |start: &str, end: &str, days: Value| json!({"quiet_hours": {"start": start, "end": end, "days": days}});
        let cases = [
            (
                json!({"frequency_caps": [sms]}),
                "/frequency_caps/0/channel",
            ),
            (
                json!({"frequency_caps": [cap("P1D", 0, "skip")]}),
                "/frequency_caps/0/limit",
            ),
            (
                json!({"frequency_caps": [cap("P32D", 1, "skip")]}),
                "/frequency_caps/0/window",
            ),
            (
                json!({"frequency_caps": [cap("soon", 1, "skip")]}),
                "/frequency_caps/0/window",
            ),
            (
                json!({"frequency_caps": [cap("P1D", 1, "skip"), cap("P1D", 1, "later")]}),
                "/frequency_caps/1/mode",
            ),
            (quiet("9pm", "08:00", json!(["mon"])), "/quiet_hours/start"),
            (quiet("21:00", "21:00", json!(["mon"])), "/quiet_hours/end"),
            (quiet("21:00", "08:00", json!([])), "/quiet_hours/days"),
        ];

        for (body, pointer) in cases {
            let reply = fx.as_admin("PUT", "/sending/settings", Some(body)).await;
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{pointer}: {:?}",
                reply.body
            );
            let fields: Vec<&str> = reply.body["errors"]
                .as_array()
                .unwrap()
                .iter()
                .map(|error| error["field"].as_str().unwrap())
                .collect();
            assert_eq!(fields, [pointer]);
        }
        let read = fx.as_admin("GET", "/sending/settings", None).await;
        assert_eq!(read.body["frequency_caps"].as_array().unwrap().len(), 2);
        assert_eq!(read.body["quiet_hours"]["start"], "21:00");
        assert_eq!(fx.audit_actions_like("send_settings.").await.len(), 1);
    }
}
