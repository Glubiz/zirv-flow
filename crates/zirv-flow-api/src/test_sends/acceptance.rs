//! The test-send API end to end, driven through the router: the permission matrix, the recipient
//! rules, the rate limits (on a manual clock), the allowlist and what the tables hold.

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use time::macros::datetime;
    use tower::ServiceExt;
    use uuid::Uuid;
    use zirv_flow_core::{Cause, HmacRoot, MasterKey, Role, Scope, TraceContext, WorkspaceId};
    use zirv_flow_store::{
        Clock, ManualClock, NewDestination, NewTemplate, create_destination, create_template,
        lock_template, publish_template,
    };

    use crate::admin::{self, Context};
    use crate::{AppState, router};

    const PASSWORD: &str = "correct horse battery";

    struct Reply {
        status: StatusCode,
        body: Value,
        retry_after: Option<String>,
        cookie: Option<String>,
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
        clock: ManualClock,
        ws: WorkspaceId,
        read: String,
        write: String,
        admin: String,
        other_admin: String,
        /// A workspace member: may read and write, not administer.
        member: Login,
        member_id: Uuid,
        /// A user who is not in the workspace.
        stranger_id: Uuid,
        /// A workspace admin.
        lead: Login,
        template: Uuid,
        destination: Uuid,
        sender: Uuid,
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
        let header_text = |name| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let retry_after = header_text(header::RETRY_AFTER);
        let cookie = header_text(header::SET_COOKIE)
            .and_then(|value| value.split(';').next().map(str::to_owned));
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            retry_after,
            cookie,
        }
    }

    async fn login_as(router: &Router, email: &str) -> Login {
        let reply = send(
            router,
            "POST",
            "/api/v1/session",
            Some(json!({"email": email, "password": PASSWORD})),
            Auth::Anonymous,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        Login {
            cookie: reply.cookie.clone().unwrap(),
            csrf: reply.body["csrf_token"].as_str().unwrap().to_owned(),
        }
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = zirv_flow_core::WorkspaceName::parse(slug).unwrap();
        let slug = zirv_flow_core::Slug::parse(slug).unwrap();
        zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        zirv_flow_store::migrate(pool).await.unwrap();
        let ws = workspace(pool, "ours").await;
        let other_ws = workspace(pool, "theirs").await;
        let clock = ManualClock::new(datetime!(2031-05-06 07:00 UTC));
        let master = MasterKey::generate();
        let root = HmacRoot::bootstrap_from(&master);
        let keyring = std::sync::Arc::new(zirv_flow_core::Keyring::new(master, None).unwrap());
        zirv_flow_store::secrets::insert_hmac_root_if_absent(
            pool,
            &zirv_flow_core::seal(&keyring, b"root", b"aad"),
        )
        .await
        .unwrap();
        let state = AppState::new(pool.clone(), std::sync::Arc::new(String::new))
            .with_hmac_root(&root)
            .with_keyring(keyring)
            .with_clock(Clock::Manual(clock.clone()));
        let router = router(state);
        let key = |workspace: WorkspaceId, scope: Scope| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), workspace, "tests", &[scope])
                    .await
                    .unwrap()
                    .plaintext
            }
        };
        let read = key(ws, Scope::Read).await;
        let write = key(ws, Scope::Write).await;
        let admin_key = key(ws, Scope::Admin).await;
        let other_admin = key(other_ws, Scope::Admin).await;
        let mut ids = Vec::new();
        for (email, role) in [
            ("member@example.com", Some(Role::Member)),
            ("lead@example.com", Some(Role::Admin)),
            ("stranger@example.com", None),
        ] {
            let user = admin::create_user(pool, &Context::system(), email, PASSWORD)
                .await
                .unwrap();
            if let Some(role) = role {
                admin::set_member(pool, &Context::system(), ws, user.id, role)
                    .await
                    .unwrap();
            }
            ids.push(user.id.as_uuid());
        }
        let member = login_as(&router, "member@example.com").await;
        let lead = login_as(&router, "lead@example.com").await;
        let sender =
            zirv_flow_store::sending_fixtures::plant_verified_sender(pool, ws, "example.com")
                .await
                .unwrap();
        let destination = create_destination(
            pool,
            ws,
            &NewDestination {
                id: Uuid::now_v7(),
                kind: "fake",
                name: "Mail",
                config: &json!({"mode": "ok"}),
                credential_id: None,
                rate: 100,
                burst: 100,
                created_by: None,
            },
        )
        .await
        .unwrap()
        .id;
        let template = Uuid::now_v7();
        let mut connection = pool.acquire().await.unwrap();
        create_template(
            &mut connection,
            ws,
            &NewTemplate {
                id: template,
                name: "Welcome".into(),
                channel: "email".into(),
                category: "marketing".into(),
                document: json!({"schema": 1, "channel": "email", "mode": "code", "email": {
                    "subject": "Hi", "sender": sender, "lang": "en", "category": "marketing",
                    "html": "<a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a>",
                    "text": "{{ system.unsubscribe_url }}"
                }}),
            },
            None,
        )
        .await
        .unwrap();
        let locked = lock_template(&mut connection, ws, template).await.unwrap();
        publish_template(
            &mut connection,
            ws,
            &locked,
            &json!([]),
            None,
            &TraceContext::root(Cause::api("publish")),
        )
        .await
        .unwrap();
        Fixture {
            pool: pool.clone(),
            router,
            clock,
            ws,
            read,
            write,
            admin: admin_key,
            other_admin,
            member,
            member_id: ids[0],
            stranger_id: ids[2],
            lead,
            template,
            destination,
            sender,
        }
    }

    impl Fixture {
        fn url(&self, tail: &str) -> String {
            format!("/api/v1/workspaces/{}{tail}", self.ws.as_uuid())
        }

        async fn call(
            &self,
            method: &str,
            tail: &str,
            body: Option<Value>,
            auth: Auth<'_>,
        ) -> Reply {
            send(&self.router, method, &self.url(tail), body, auth).await
        }

        fn test_send_url(&self) -> String {
            format!("/templates/{}/test-send", self.template)
        }

        fn request(&self, to: Value) -> Value {
            json!({
                "destination_id": self.destination, "sender_identity_id": self.sender,
                "to": to,
            })
        }

        async fn allow(&self, address: &str) -> Reply {
            self.call(
                "POST",
                "/test-send-allowlist",
                Some(json!({"address": address})),
                Auth::Key(&self.admin),
            )
            .await
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_permission_matrix_covers_every_route(pool: PgPool) {
        let f = fixture(&pool).await;
        let to_member = f.request(json!({"member": f.member_id}));
        let entry = f.allow("qa@example.org").await.body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let allowlist_entry = format!("/test-send-allowlist/{entry}");

        // Test send: write.
        let send_url = f.test_send_url();
        let anonymous = f
            .call("POST", &send_url, Some(to_member.clone()), Auth::Anonymous)
            .await;
        let read = f
            .call(
                "POST",
                &send_url,
                Some(to_member.clone()),
                Auth::Key(&f.read),
            )
            .await;
        let write = f
            .call(
                "POST",
                &send_url,
                Some(to_member.clone()),
                Auth::Key(&f.write),
            )
            .await;
        let admin = f
            .call(
                "POST",
                &send_url,
                Some(to_member.clone()),
                Auth::Session(&f.lead),
            )
            .await;
        let member = f
            .call(
                "POST",
                &send_url,
                Some(to_member.clone()),
                Auth::Session(&f.member),
            )
            .await;
        assert_eq!(
            [
                anonymous.status,
                read.status,
                write.status,
                admin.status,
                member.status
            ],
            [
                StatusCode::UNAUTHORIZED,
                StatusCode::FORBIDDEN,
                StatusCode::ACCEPTED,
                StatusCode::ACCEPTED,
                StatusCode::ACCEPTED
            ]
        );
        let foreign = send(
            &f.router,
            "POST",
            &f.url(&send_url),
            Some(to_member.clone()),
            Auth::Key(&f.other_admin),
        )
        .await;
        assert_eq!(foreign.status, StatusCode::NOT_FOUND);
        let no_csrf = send(
            &f.router,
            "POST",
            &f.url(&send_url),
            Some(to_member.clone()),
            Auth::Session(&Login {
                cookie: f.member.cookie.clone(),
                csrf: "wrong".into(),
            }),
        )
        .await;
        assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);

        // A person's values: admin.
        let person = f.request(json!({"member": f.member_id}));
        let mut with_person = person;
        with_person["person_id"] = json!(Uuid::now_v7());
        let by_writer = f
            .call(
                "POST",
                &send_url,
                Some(with_person.clone()),
                Auth::Key(&f.write),
            )
            .await;
        assert_eq!(by_writer.status, StatusCode::FORBIDDEN);
        assert_eq!(by_writer.body["code"], "template_preview_values_forbidden");
        let by_admin = f
            .call("POST", &send_url, Some(with_person), Auth::Key(&f.admin))
            .await;
        assert_eq!(by_admin.status, StatusCode::NOT_FOUND, "an unknown person");

        // The log: read.
        let log = |auth| f.call("GET", "/test-sends", None, auth);
        assert_eq!(log(Auth::Anonymous).await.status, StatusCode::UNAUTHORIZED);
        let listed = log(Auth::Key(&f.read)).await;
        assert_eq!(listed.status, StatusCode::OK);
        assert_eq!(listed.body["test_sends"].as_array().unwrap().len(), 3);
        assert_eq!(log(Auth::Session(&f.member)).await.status, StatusCode::OK);
        let foreign_log = f
            .call("GET", "/test-sends", None, Auth::Key(&f.other_admin))
            .await;
        assert_eq!(foreign_log.status, StatusCode::NOT_FOUND);

        // The allowlist: admin.
        for (method, tail, body) in [
            ("GET", "/test-send-allowlist", None),
            (
                "POST",
                "/test-send-allowlist",
                Some(json!({"address": "x@example.org"})),
            ),
            ("DELETE", allowlist_entry.as_str(), None),
        ] {
            for auth in [Auth::Anonymous] {
                let reply = f.call(method, tail, body.clone(), auth).await;
                assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{method} {tail}");
            }
            for (name, auth) in [
                ("read key", Auth::Key(&f.read)),
                ("write key", Auth::Key(&f.write)),
                ("member", Auth::Session(&f.member)),
            ] {
                let reply = f.call(method, tail, body.clone(), auth).await;
                assert_eq!(
                    reply.status,
                    StatusCode::FORBIDDEN,
                    "{name}: {method} {tail}"
                );
            }
            let foreign = send(
                &f.router,
                method,
                &f.url(tail),
                body.clone(),
                Auth::Key(&f.other_admin),
            )
            .await;
            assert_eq!(foreign.status, StatusCode::NOT_FOUND, "{method} {tail}");
        }
        let lead_list = f
            .call("GET", "/test-send-allowlist", None, Auth::Session(&f.lead))
            .await;
        assert_eq!(lead_list.status, StatusCode::OK);
        let lead_add = f
            .call(
                "POST",
                "/test-send-allowlist",
                Some(json!({"address": "y@example.org"})),
                Auth::Session(&f.lead),
            )
            .await;
        assert_eq!(lead_add.status, StatusCode::CREATED);
        let removed = f
            .call("DELETE", &allowlist_entry, None, Auth::Key(&f.admin))
            .await;
        assert_eq!(removed.status, StatusCode::NO_CONTENT);
        let again = f
            .call("DELETE", &allowlist_entry, None, Auth::Key(&f.admin))
            .await;
        assert_eq!(again.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_members_and_allowlisted_addresses_are_recipients(pool: PgPool) {
        let f = fixture(&pool).await;
        let url = f.test_send_url();
        let write = || Auth::Key(&f.write);

        let member = f
            .call(
                "POST",
                &url,
                Some(f.request(json!({"member": f.member_id}))),
                write(),
            )
            .await;
        let stranger = f
            .call(
                "POST",
                &url,
                Some(f.request(json!({"member": f.stranger_id}))),
                write(),
            )
            .await;
        let unknown_user = f
            .call(
                "POST",
                &url,
                Some(f.request(json!({"member": Uuid::now_v7()}))),
                write(),
            )
            .await;
        let unlisted = f
            .call(
                "POST",
                &url,
                Some(f.request(json!({"address": "qa@example.org"}))),
                write(),
            )
            .await;
        f.allow("QA@Example.org").await;
        let listed = f
            .call(
                "POST",
                &url,
                Some(f.request(json!({"address": "qa@example.org"}))),
                write(),
            )
            .await;
        let both = f
            .call(
                "POST",
                &url,
                Some(f.request(json!({"member": f.member_id, "address": "qa@example.org"}))),
                write(),
            )
            .await;

        assert_eq!(member.status, StatusCode::ACCEPTED, "{:?}", member.body);
        assert_eq!(member.body["outcome"], "queued");
        assert_eq!(member.body["recipient_member_id"], json!(f.member_id));
        for refused in [&stranger, &unknown_user, &unlisted] {
            assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(refused.body["code"], "recipient_not_allowed");
        }
        assert_eq!(listed.status, StatusCode::ACCEPTED, "{:?}", listed.body);
        assert_eq!(listed.body["recipient_member_id"], Value::Null);
        assert_eq!(both.status, StatusCode::UNPROCESSABLE_ENTITY);
        // A refusal writes no row and no job.
        assert_eq!(f.count("SELECT count(*) FROM test_sends").await, 2);
        assert_eq!(
            f.count("SELECT count(*) FROM jobs WHERE kind = 'template.test_send'")
                .await,
            2
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_log_and_the_allowlist_hold_no_address_and_the_audit_holds_none_either(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let added = f.allow("qa.person@example.org").await;
        assert_eq!(added.status, StatusCode::CREATED, "{:?}", added.body);
        assert_eq!(added.body["display_hint"], "q***@example.org");
        assert_eq!(
            f.allow("qa.person@example.org").await.status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            f.allow("not an address").await.status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let sent = f
            .call(
                "POST",
                &f.test_send_url(),
                Some(f.request(json!({"address": "qa.person@example.org"}))),
                Auth::Key(&f.write),
            )
            .await;
        assert_eq!(sent.status, StatusCode::ACCEPTED);
        let payloads: Vec<String> = sqlx::query_scalar("SELECT payload::text FROM jobs")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(!payloads.is_empty());
        assert!(
            payloads
                .iter()
                .all(|payload| !payload.contains('@') && !payload.contains("qa.person")),
            "no job payload holds an address: {payloads:?}"
        );
        let sealed: Vec<u8> = sqlx::query_scalar(
            "SELECT ciphertext FROM encrypted_secrets WHERE purpose = 'test_send_address'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!sealed.windows(9).any(|window| window == b"qa.person"));

        for table in ["test_sends", "test_send_allowlist", "audit_log"] {
            let text: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT coalesce(string_agg(t::text, E'\\n'), '') FROM {table} t"
            )))
            .fetch_one(&pool)
            .await
            .unwrap();
            for marker in ["qa.person", "example.org"] {
                if table == "test_send_allowlist" && marker == "example.org" {
                    continue; // the masked hint keeps the domain
                }
                assert!(!text.contains(marker), "{marker} in {table}");
            }
        }
        let actions: Vec<String> = sqlx::query_scalar(
            "SELECT action FROM audit_log WHERE action LIKE 'template.test%' \
                OR action LIKE 'test_send.%' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(actions, ["test_send.allowlist_added", "template.test_sent"]);
        let listed = f
            .call("GET", "/test-send-allowlist", None, Auth::Key(&f.admin))
            .await;
        assert_eq!(listed.body["entries"].as_array().unwrap().len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_eleventh_test_of_a_member_is_refused_with_a_retry_after_until_the_hour_passes(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let url = f.test_send_url();
        let body = f.request(json!({"member": f.member_id}));
        let as_member = || Auth::Session(&f.member);

        for minute in 0..10 {
            f.clock
                .set(datetime!(2031-05-06 07:00 UTC) + time::Duration::minutes(minute));
            let reply = f.call("POST", &url, Some(body.clone()), as_member()).await;
            assert_eq!(
                reply.status,
                StatusCode::ACCEPTED,
                "{minute}: {:?}",
                reply.body
            );
        }
        f.clock.set(datetime!(2031-05-06 07:30 UTC));
        let refused = f.call("POST", &url, Some(body.clone()), as_member()).await;

        assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(refused.body["code"], "test_send_rate_limited");
        assert_eq!(refused.retry_after.as_deref(), Some("1800"));
        assert_eq!(f.count("SELECT count(*) FROM test_sends").await, 10);
        // Another member of the workspace is not held back by this member's count.
        let lead = f
            .call("POST", &url, Some(body.clone()), Auth::Session(&f.lead))
            .await;
        assert_eq!(lead.status, StatusCode::ACCEPTED);
        f.clock.set(datetime!(2031-05-06 08:01 UTC));
        let after = f.call("POST", &url, Some(body), as_member()).await;
        assert_eq!(after.status, StatusCode::ACCEPTED, "{:?}", after.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_hundred_and_first_test_of_a_workspace_is_refused_until_the_hour_passes(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let url = f.test_send_url();
        let body = f.request(json!({"member": f.member_id}));
        for _ in 0..100 {
            let reply = f
                .call("POST", &url, Some(body.clone()), Auth::Key(&f.write))
                .await;
            assert_eq!(reply.status, StatusCode::ACCEPTED);
        }

        let refused = f
            .call("POST", &url, Some(body.clone()), Auth::Key(&f.write))
            .await;

        assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(refused.retry_after.as_deref(), Some("3600"));
        f.clock.advance(time::Duration::minutes(61));
        let after = f.call("POST", &url, Some(body), Auth::Key(&f.write)).await;
        assert_eq!(after.status, StatusCode::ACCEPTED);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_concurrent_requests_for_the_last_slot_have_one_winner(pool: PgPool) {
        let f = fixture(&pool).await;
        let url = f.test_send_url();
        let body = f.request(json!({"member": f.member_id}));
        for _ in 0..9 {
            let reply = f
                .call("POST", &url, Some(body.clone()), Auth::Session(&f.member))
                .await;
            assert_eq!(reply.status, StatusCode::ACCEPTED);
        }

        let (one, two) = tokio::join!(
            f.call("POST", &url, Some(body.clone()), Auth::Session(&f.member)),
            f.call("POST", &url, Some(body.clone()), Auth::Session(&f.member)),
        );

        let mut statuses = [one.status, two.status];
        statuses.sort();
        assert_eq!(
            statuses,
            [StatusCode::ACCEPTED, StatusCode::TOO_MANY_REQUESTS]
        );
        assert_eq!(f.count("SELECT count(*) FROM test_sends").await, 10);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_log_is_paged_newest_first_and_filtered_by_template(pool: PgPool) {
        let f = fixture(&pool).await;
        let url = f.test_send_url();
        let body = f.request(json!({"member": f.member_id}));
        let mut ids = Vec::new();
        for minute in 0..3 {
            f.clock
                .set(datetime!(2031-05-06 07:00 UTC) + time::Duration::minutes(minute));
            let reply = f
                .call("POST", &url, Some(body.clone()), Auth::Key(&f.write))
                .await;
            ids.push(reply.body["id"].as_str().unwrap().to_owned());
        }

        let first = f
            .call("GET", "/test-sends?limit=2", None, Auth::Key(&f.read))
            .await;
        let cursor = first.body["next_before"].as_str().unwrap().to_owned();
        let second = f
            .call(
                "GET",
                &format!("/test-sends?limit=2&before={cursor}"),
                None,
                Auth::Key(&f.read),
            )
            .await;
        let other_template = f
            .call(
                "GET",
                &format!("/test-sends?template_id={}", Uuid::now_v7()),
                None,
                Auth::Key(&f.read),
            )
            .await;
        let same_template = f
            .call(
                "GET",
                &format!("/test-sends?template_id={}", f.template),
                None,
                Auth::Key(&f.read),
            )
            .await;
        let bad = f
            .call("GET", "/test-sends?template_id=x", None, Auth::Key(&f.read))
            .await;

        let listed = |reply: &Reply| -> Vec<String> {
            reply.body["test_sends"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["id"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(listed(&first), [ids[2].clone(), ids[1].clone()]);
        assert_eq!(listed(&second), [ids[0].clone()]);
        assert_eq!(second.body["next_before"], Value::Null);
        assert!(listed(&other_template).is_empty());
        assert_eq!(listed(&same_template).len(), 3);
        assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_test_needs_a_live_email_template_a_version_a_destination_and_an_email_driver(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let url = f.test_send_url();
        let to = json!({"member": f.member_id});
        let post = |url: String, body: Value| {
            let f = &f;
            async move { f.call("POST", &url, Some(body), Auth::Key(&f.write)).await }
        };

        let unknown_version = {
            let mut body = f.request(to.clone());
            body["version"] = json!(9);
            post(url.clone(), body).await
        };
        let known_version = {
            let mut body = f.request(to.clone());
            body["version"] = json!(1);
            post(url.clone(), body).await
        };
        let no_template = post(
            format!("/templates/{}/test-send", Uuid::now_v7()),
            f.request(to.clone()),
        )
        .await;
        let mut no_destination = f.request(to.clone());
        no_destination["destination_id"] = json!(Uuid::now_v7());
        let no_destination = post(url.clone(), no_destination).await;
        sqlx::query("UPDATE templates SET archived_at = now()")
            .execute(&pool)
            .await
            .unwrap();
        let archived = post(url.clone(), f.request(to)).await;

        assert_eq!(unknown_version.status, StatusCode::NOT_FOUND);
        assert_eq!(known_version.status, StatusCode::ACCEPTED);
        assert_eq!(known_version.body["template_version"], 1);
        assert_eq!(no_template.status, StatusCode::NOT_FOUND);
        assert_eq!(no_destination.status, StatusCode::NOT_FOUND);
        assert_eq!(archived.status, StatusCode::CONFLICT);
        assert_eq!(archived.body["code"], "template_archived");
    }
}
