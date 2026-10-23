//! The HTTP manifest source end to end through the router: the manifest endpoints, the OAuth2
//! consent (start and callback), tokens at the call sites, and the sample. A local server stands
//! in for both the API and its identity provider.

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::Body;
    use axum::extract::{RawQuery, State};
    use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use zirv_flow_core::egress::EgressPolicy;
    use zirv_flow_core::{
        HmacRoot, Keyring, MasterKey, Role, Scope, Slug, WorkspaceId, WorkspaceName,
        deployment_key_aad, seal,
    };
    use zirv_flow_store::secrets;

    use crate::admin::{self, Context};
    use crate::test_support::{tables_hold_no, the_scan_finds_a_planted};
    use crate::{AppState, router};

    const PASSWORD: &str = "correct horse battery";
    const MANIFEST: &str = include_str!(
        "../../../zirv-flow-connectors/tests/fixtures/manifests/cursor_oauth2_code.json"
    );
    const CODE: &str = "zz-authorization-code-4711";
    const CLIENT_SECRET: &str = "zz-client-secret-0815";
    const REPLACED_CLIENT_SECRET: &str = "zz-client-secret-replaced-2648";
    const PUBLIC_URL: &str = "http://flow.test";

    /// What the identity provider does with the next code exchange.
    #[derive(Clone, Copy, PartialEq)]
    enum Exchange {
        Good,
        Refuse,
        NoRefreshToken,
    }

    struct Upstream {
        exchange: Mutex<Exchange>,
        valid_access: Mutex<String>,
        valid_refresh: Mutex<String>,
        refreshes: AtomicUsize,
        data_requests: AtomicUsize,
        /// Whether the data endpoint answers 429.
        rate_limited: Mutex<bool>,
        forms: Mutex<Vec<BTreeMap<String, String>>>,
    }

    fn form(text: &str) -> BTreeMap<String, String> {
        url::form_urlencoded::parse(text.as_bytes())
            .into_owned()
            .collect()
    }

    async fn serve_upstream() -> (u16, Arc<Upstream>) {
        let upstream = Arc::new(Upstream {
            exchange: Mutex::new(Exchange::Good),
            valid_access: Mutex::default(),
            valid_refresh: Mutex::new("rt-0".to_owned()),
            refreshes: AtomicUsize::new(0),
            data_requests: AtomicUsize::new(0),
            rate_limited: Mutex::new(false),
            forms: Mutex::default(),
        });
        let router =
            Router::new()
                .route(
                    "/v1/contacts",
                    get(
                        |State(up): State<Arc<Upstream>>,
                         headers: HeaderMap,
                         RawQuery(q): RawQuery| async move {
                            up.data_requests.fetch_add(1, Ordering::SeqCst);
                            if *up.rate_limited.lock().unwrap() {
                                let mut response = StatusCode::TOO_MANY_REQUESTS.into_response();
                                response
                                    .headers_mut()
                                    .insert("retry-after", HeaderValue::from_static("0"));
                                return response;
                            }
                            let sent = headers
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or_default()
                                .to_owned();
                            if sent != format!("Bearer {}", up.valid_access.lock().unwrap()) {
                                return StatusCode::UNAUTHORIZED.into_response();
                            }
                            let q = form(q.as_deref().unwrap_or_default());
                            let start: usize = q.get("cursor").map_or(0, |c| c.parse().unwrap());
                            let size: usize = q["limit"].parse().unwrap();
                            let rows: Vec<Value> = (1..=5)
                                .map(|id| {
                                    json!({"id": format!("c{id}"),
                                       "email": format!("person{id}@example.com"),
                                       "updated_at": format!("2026-01-0{id}T00:00:00Z"),
                                       "status": "active"})
                                })
                                .collect();
                            let page: Vec<Value> =
                                rows.iter().skip(start).take(size).cloned().collect();
                            let next =
                                (start + size < rows.len()).then(|| (start + size).to_string());
                            axum::Json(json!({"data": page, "next_cursor": next})).into_response()
                        },
                    ),
                )
                .route(
                    "/token",
                    post(|State(up): State<Arc<Upstream>>, body: String| async move {
                        let form = form(&body);
                        up.forms.lock().unwrap().push(form.clone());
                        match form.get("grant_type").map(String::as_str) {
                            Some("authorization_code") => {
                                let mode = *up.exchange.lock().unwrap();
                                if mode == Exchange::Refuse
                                    || form.get("code").map(String::as_str) != Some(CODE)
                                {
                                    return (StatusCode::BAD_REQUEST, "no").into_response();
                                }
                                *up.valid_access.lock().unwrap() = "access-0".to_owned();
                                let mut answer =
                                    json!({"access_token": "access-0", "expires_in": 3600});
                                if mode == Exchange::Good {
                                    answer["refresh_token"] = "rt-0".into();
                                }
                                axum::Json(answer).into_response()
                            }
                            Some("refresh_token") => {
                                let mut valid = up.valid_refresh.lock().unwrap();
                                if form.get("refresh_token") != Some(&*valid) {
                                    *valid = "revoked".to_owned();
                                    return (StatusCode::BAD_REQUEST, "reuse").into_response();
                                }
                                let n = up.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                                *valid = format!("rt-{n}");
                                *up.valid_access.lock().unwrap() = format!("access-{n}");
                                axum::Json(json!({"access_token": format!("access-{n}"),
                                "refresh_token": format!("rt-{n}"), "expires_in": 3600}))
                                .into_response()
                            }
                            _ => StatusCode::BAD_REQUEST.into_response(),
                        }
                    }),
                )
                .with_state(Arc::clone(&upstream));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (port, upstream)
    }

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
        /// A session with its CSRF token.
        Session(&'a Login),
        /// A session cookie only, as a browser sends it on a top-level navigation.
        Cookie(&'a Login),
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
            Auth::Cookie(login) => request.header(header::COOKIE, &login.cookie),
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

    struct Fixture {
        keyring: Arc<Keyring>,
        pool: PgPool,
        router: Router,
        ws: WorkspaceId,
        port: u16,
        upstream: Arc<Upstream>,
        admin_key: String,
        read_key: String,
        admin: Login,
        other_admin: Login,
        member: Login,
    }

    async fn login(router: &Router, email: &str) -> Login {
        let reply = call(
            router,
            "POST",
            "/api/v1/session",
            Some(json!({"email": email, "password": PASSWORD})),
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
        Login {
            cookie,
            csrf: reply.body["csrf_token"].as_str().unwrap().to_owned(),
        }
    }

    async fn fixture_with(pool: &PgPool, public_url: Option<&str>) -> Fixture {
        zirv_flow_store::migrate(pool).await.unwrap();
        let (port, upstream) = serve_upstream().await;
        let ws = zirv_flow_store::create_workspace(
            pool,
            &WorkspaceName::parse("ours").unwrap(),
            &Slug::parse(&format!(
                "ws-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            ))
            .unwrap(),
        )
        .await
        .unwrap()
        .id;
        let keyring = Arc::new(Keyring::new(MasterKey::generate(), None).unwrap());
        let root = HmacRoot::bootstrap_from(keyring.current()).to_secret_bytes();
        secrets::insert_hmac_root_if_absent(
            pool,
            &seal(
                &keyring,
                root.expose(),
                &deployment_key_aad(secrets::HMAC_ROOT),
            ),
        )
        .await
        .unwrap();
        let state = AppState::new(pool.clone(), Arc::new(String::new))
            .with_keyring(keyring.clone())
            .with_cookie_secure(false)
            .with_public_url(public_url.map(str::to_owned))
            .with_egress_policy(Arc::new(
                EgressPolicy::parse(&["127.0.0.1/32".to_owned()]).unwrap(),
            ));
        let router = router(state);
        let key = |scopes: &'static [Scope]| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), ws, "k", scopes)
                    .await
                    .unwrap()
                    .plaintext
            }
        };
        let admin_key = key(&[Scope::Admin, Scope::Read]).await;
        let read_key = key(&[Scope::Read]).await;
        for (email, role) in [
            ("admin@example.com", Role::Admin),
            ("other@example.com", Role::Admin),
            ("member@example.com", Role::Member),
        ] {
            let user = admin::create_user(pool, &Context::system(), email, PASSWORD)
                .await
                .unwrap();
            admin::set_member(pool, &Context::system(), ws, user.id, role)
                .await
                .unwrap();
        }
        Fixture {
            keyring,
            pool: pool.clone(),
            admin: login(&router, "admin@example.com").await,
            other_admin: login(&router, "other@example.com").await,
            member: login(&router, "member@example.com").await,
            router,
            ws,
            port,
            upstream,
            admin_key,
            read_key,
        }
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        fixture_with(pool, Some(PUBLIC_URL)).await
    }

    impl Fixture {
        fn path(&self, tail: &str) -> String {
            format!("/api/v1/workspaces/{}{tail}", self.ws.as_uuid())
        }

        fn manifest(&self) -> Value {
            let base = format!("http://127.0.0.1:{}", self.port);
            let mut manifest: Value = serde_json::from_str(
                &MANIFEST
                    .replace("https://api.example.test", &base)
                    .replace("https://auth.example.test", &base),
            )
            .unwrap();
            manifest["streams"][0]["query"] = json!({"limit": 2});
            manifest["rate_limit"] = json!({"requests": 1000, "per_seconds": 1});
            manifest
        }

        async fn as_key(&self, method: &str, tail: &str, body: Option<Value>) -> Reply {
            call(
                &self.router,
                method,
                &self.path(tail),
                body,
                Auth::Key(&self.admin_key),
            )
            .await
        }

        async fn as_session(
            &self,
            who: &Login,
            method: &str,
            tail: &str,
            body: Option<Value>,
        ) -> Reply {
            call(
                &self.router,
                method,
                &self.path(tail),
                body,
                Auth::Session(who),
            )
            .await
        }

        /// Creates an `http` source on the reference manifest with a client id and secret.
        async fn create_source(&self, manifest: Value) -> String {
            let reply = self
                .as_key(
                    "POST",
                    "/sources",
                    Some(json!({
                        "kind": "http",
                        "name": format!("API {}", uuid::Uuid::new_v4()),
                        "config": {"manifest": manifest},
                        "credential": {"client_id": "client", "client_secret": CLIENT_SECRET},
                    })),
                )
                .await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
            reply.body["id"].as_str().unwrap().to_owned()
        }

        /// Starts a consent as `who` and returns the `state` of the authorization URL.
        async fn start(&self, who: &Login, id: &str) -> Reply {
            self.as_session(who, "POST", &format!("/sources/{id}/oauth/start"), None)
                .await
        }

        async fn state_of(&self, who: &Login, id: &str) -> String {
            let reply = self.start(who, id).await;
            assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
            let url = url::Url::parse(reply.body["authorization_url"].as_str().unwrap()).unwrap();
            url.query_pairs()
                .find(|(key, _)| key == "state")
                .unwrap()
                .1
                .into_owned()
        }

        async fn callback(&self, who: Option<&Login>, query: &str) -> Reply {
            call(
                &self.router,
                "GET",
                &format!("/api/v1/oauth/callback?{query}"),
                None,
                who.map_or(Auth::Anonymous, Auth::Cookie),
            )
            .await
        }

        async fn count(&self, sql: &'static str) -> i64 {
            sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
        }

        async fn actions(&self) -> Vec<String> {
            let mut entries = zirv_flow_store::list_audit(&self.pool, 500).await.unwrap();
            entries.reverse();
            entries.into_iter().map(|entry| entry.action).collect()
        }

        async fn connect(&self, id: &str) {
            let state = self.state_of(&self.admin, id).await;
            let reply = self
                .callback(Some(&self.admin), &format!("code={CODE}&state={state}"))
                .await;
            assert_eq!(reply.status, StatusCode::SEE_OTHER);
            assert!(
                location(&reply).ends_with("?oauth=connected"),
                "{}",
                location(&reply)
            );
        }
    }

    fn location(reply: &Reply) -> String {
        reply.headers[header::LOCATION].to_str().unwrap().to_owned()
    }

    fn codes(reply: &Reply) -> Vec<(String, String)> {
        reply.body["errors"]
            .as_array()
            .map(|errors| {
                errors
                    .iter()
                    .map(|e| {
                        (
                            e["field"].as_str().unwrap().to_owned(),
                            e["reason"].as_str().unwrap().to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    // ---- validate -----------------------------------------------------------------------

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn validate_answers_200_with_issues_for_admins_only_and_writes_nothing(pool: PgPool) {
        let fx = fixture(&pool).await;
        let before = fx.count("SELECT count(*) FROM audit_log").await;
        let good = json!({"manifest": fx.manifest()});
        let mut bad_manifest = fx.manifest();
        bad_manifest["streams"][0]["path"] = json!("SECRET-no-slash");
        bad_manifest["script"] = json!("x");
        let bad = json!({"manifest": bad_manifest});

        let valid = fx
            .as_key("POST", "/source-manifests/validate", Some(good.clone()))
            .await;
        let invalid = fx
            .as_key("POST", "/source-manifests/validate", Some(bad.clone()))
            .await;

        assert_eq!(valid.status, StatusCode::OK, "{:?}", valid.body);
        assert_eq!(valid.body["valid"], true);
        assert_eq!(valid.body["issues"], json!([]));
        assert_eq!(valid.body["streams"][0]["name"], "contacts");
        assert_eq!(
            valid.body["streams"][0]["fields"][0],
            json!({"name": "id", "type": "string"})
        );
        assert_eq!(invalid.status, StatusCode::OK);
        assert_eq!(invalid.body["valid"], false);
        let issues: Vec<(String, String)> = invalid.body["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                (
                    i["code"].as_str().unwrap().into(),
                    i["path"].as_str().unwrap().into(),
                )
            })
            .collect();
        assert!(issues.contains(&("path_invalid".into(), "/streams/0/path".into())));
        assert!(issues.contains(&("expression_not_supported".into(), "/script".into())));
        assert!(!invalid.body.to_string().contains("SECRET"));

        // Admins only: a member, a read-only key and an anonymous caller are refused.
        let member = fx
            .as_session(
                &fx.member,
                "POST",
                "/source-manifests/validate",
                Some(good.clone()),
            )
            .await;
        let reader = call(
            &fx.router,
            "POST",
            &fx.path("/source-manifests/validate"),
            Some(good.clone()),
            Auth::Key(&fx.read_key),
        )
        .await;
        let anonymous = call(
            &fx.router,
            "POST",
            &fx.path("/source-manifests/validate"),
            Some(good.clone()),
            Auth::Anonymous,
        )
        .await;
        assert_eq!(member.status, StatusCode::FORBIDDEN);
        assert_eq!(reader.status, StatusCode::FORBIDDEN);
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        // An admin session without its CSRF token is refused too.
        let no_csrf = call(
            &fx.router,
            "POST",
            &fx.path("/source-manifests/validate"),
            Some(good),
            Auth::Cookie(&fx.admin),
        )
        .await;
        assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
        let admin_session = fx
            .as_session(&fx.admin, "POST", "/source-manifests/validate", Some(bad))
            .await;
        assert_eq!(admin_session.status, StatusCode::OK);

        // Nothing was written: no audit entry, no source.
        assert_eq!(fx.count("SELECT count(*) FROM audit_log").await, before);
        assert_eq!(fx.count("SELECT count(*) FROM sources").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn validate_takes_a_body_up_to_the_cap_and_refuses_a_bigger_one(pool: PgPool) {
        let fx = fixture(&pool).await;
        // Over the manifest cap but inside the body cap: a 200 saying so.
        let mut big = fx.manifest();
        big["padding"] = json!("x".repeat(zirv_flow_connectors::manifest::MAX_MANIFEST_BYTES));
        let reply = fx
            .as_key(
                "POST",
                "/source-manifests/validate",
                Some(json!({"manifest": big})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.body["issues"],
            json!([{"code": "manifest_too_large", "path": ""}])
        );

        // Over the body cap: refused before it is parsed.
        let huge = json!({"manifest": {"padding": "x".repeat(70 * 1024)}});
        let refused = fx
            .as_key("POST", "/source-manifests/validate", Some(huge))
            .await;
        assert_eq!(refused.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    // ---- create and patch ---------------------------------------------------------------

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn create_and_patch_refuse_an_invalid_manifest_with_the_validate_codes_and_store_a_valid_one(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let mut bad = fx.manifest();
        bad["streams"][0]["path"] = json!("no-slash");
        bad["extra"] = json!(1);
        let create = |manifest: Value| {
            json!({"kind": "http", "name": "API", "config": {"manifest": manifest},
                   "credential": {"client_id": "c"}})
        };

        let refused = fx
            .as_key("POST", "/sources", Some(create(bad.clone())))
            .await;

        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        let found = codes(&refused);
        assert!(
            found.contains(&(
                "config.manifest/streams/0/path".into(),
                "path_invalid".into()
            )),
            "{found:?}"
        );
        assert!(found.contains(&("config.manifest/extra".into(), "unknown_field".into())));
        assert_eq!(fx.count("SELECT count(*) FROM sources").await, 0);

        let id = fx.create_source(fx.manifest()).await;
        let stored = fx.as_key("GET", &format!("/sources/{id}"), None).await;
        assert_eq!(stored.body["config"]["manifest"]["version"], "v1");

        let patched = fx
            .as_key(
                "PATCH",
                &format!("/sources/{id}"),
                Some(json!({"config": {"manifest": bad}})),
            )
            .await;
        assert_eq!(patched.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(codes(&patched).contains(&(
            "config.manifest/streams/0/path".into(),
            "path_invalid".into()
        )));
        let unchanged = fx.as_key("GET", &format!("/sources/{id}"), None).await;
        assert_eq!(unchanged.body["config"], stored.body["config"]);

        // The kind lists the manifest as a `json` field.
        let kinds = call(
            &fx.router,
            "GET",
            "/api/v1/source-kinds",
            None,
            Auth::Key(&fx.admin_key),
        )
        .await;
        let http = kinds
            .body
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["kind"] == "http")
            .unwrap();
        assert_eq!(http["config_fields"][0]["key"], "manifest");
        assert_eq!(http["config_fields"][0]["kind"], "json");
        assert!(
            http["credential_fields"]
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["secret"] == true)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rotation_between_the_start_and_the_callback_keeps_the_consent_unless_the_client_changed(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let rotate = |id: &str, client: &'static str| {
            let id = id.to_owned();
            let fx = &fx;
            async move {
                let source = fx.as_key("GET", &format!("/sources/{id}"), None).await;
                let credential = source.body["credential_id"].as_str().unwrap().to_owned();
                let reply = fx
                    .as_key(
                        "PUT",
                        &format!("/credentials/{credential}/secret"),
                        Some(json!({"secret": {"client_id": client, "client_secret": "rotated"}})),
                    )
                    .await;
                assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
            }
        };

        // Same client id, new secret: the callback still stores the tokens.
        let kept = fx.create_source(fx.manifest()).await;
        let state = fx.state_of(&fx.admin, &kept).await;
        rotate(&kept, "client").await;
        let reply = fx
            .callback(Some(&fx.admin), &format!("code={CODE}&state={state}"))
            .await;
        assert!(
            location(&reply).ends_with("?oauth=connected"),
            "{}",
            location(&reply)
        );
        let linked = fx
            .count("SELECT count(*) FROM sources WHERE oauth_secret_id IS NOT NULL")
            .await;
        assert_eq!(linked, 1);

        // Another client id: the tokens were issued to a client the credential no longer names.
        let dropped = fx.create_source(fx.manifest()).await;
        let state = fx.state_of(&fx.admin, &dropped).await;
        rotate(&dropped, "another-client").await;
        let reply = fx
            .callback(Some(&fx.admin), &format!("code={CODE}&state={state}"))
            .await;
        assert!(
            location(&reply).ends_with("&reason=state_mismatch"),
            "{}",
            location(&reply)
        );
        let linked = fx
            .count("SELECT count(*) FROM sources WHERE oauth_secret_id IS NOT NULL")
            .await;
        assert_eq!(linked, 1, "no tokens were stored for the second source");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_token_write_reads_the_client_id_from_the_locked_credential_row(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.create_source(fx.manifest()).await;
        let source = fx.as_key("GET", &format!("/sources/{id}"), None).await;
        let credential = source.body["credential_id"].as_str().unwrap().to_owned();
        let state = fx.state_of(&fx.admin, &id).await;

        // Another transaction holds the credential's write lock and points it at a secret that
        // names another client.
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM credentials WHERE id = $1::uuid FOR UPDATE")
            .bind(&credential)
            .execute(&mut *holder)
            .await
            .unwrap();
        let replacement = zirv_flow_core::SecretId::new();
        let purpose = zirv_flow_core::SecretPurpose::WorkspaceCredential;
        let sealed = seal(
            &fx.keyring,
            br#"{"client_id":"another-client","client_secret":"x"}"#,
            &zirv_flow_core::secret_aad(fx.ws, replacement, purpose),
        );
        secrets::insert_unchecked(&mut *holder, fx.ws, purpose, replacement, &sealed)
            .await
            .unwrap();
        sqlx::query("UPDATE credentials SET secret_id = $2 WHERE id = $1::uuid")
            .bind(&credential)
            .bind(replacement.as_uuid())
            .execute(&mut *holder)
            .await
            .unwrap();
        let router = fx.router.clone();
        let query = format!("code={CODE}&state={state}");
        let cookie = fx.admin.cookie.clone();
        let callback = tokio::spawn(async move {
            let request = Request::builder()
                .method("GET")
                .uri(format!("/api/v1/oauth/callback?{query}"))
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap();
            router.oneshot(request).await.unwrap()
        });
        crate::test_support::wait_until_a_backend_waits_on_a_lock(&pool).await;
        assert!(
            !callback.is_finished(),
            "the token write waits for the credential's writer"
        );
        holder.commit().await.unwrap();

        // The write then reads the committed client id and refuses the tokens.
        let reply = callback.await.unwrap();
        let place = reply.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(place.ends_with("&reason=state_mismatch"), "{place}");
        let linked = fx
            .count("SELECT count(*) FROM sources WHERE oauth_secret_id IS NOT NULL")
            .await;
        assert_eq!(linked, 0);
    }

    // ---- the consent --------------------------------------------------------------------

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn start_returns_the_authorization_url_and_keeps_only_the_hash_of_the_state(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.create_source(fx.manifest()).await;

        let reply = fx.start(&fx.admin, &id).await;

        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        let url = url::Url::parse(reply.body["authorization_url"].as_str().unwrap()).unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.path(), "/authorize");
        let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["client_id"], "client");
        assert_eq!(
            query["redirect_uri"],
            format!("{PUBLIC_URL}/api/v1/oauth/callback")
        );
        assert_eq!(query["scope"], "contacts.read");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["code_challenge"].len(), 43);
        let state = &query["state"];
        // The state is stored hashed, with the verifier, the source and the user, for ten minutes.
        let rows: Vec<(Vec<u8>, String, bool)> = sqlx::query_as(
            "SELECT state_hash, pkce_verifier, expires_at > now() + interval '9 minutes' \
             FROM source_oauth_states",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, zirv_flow_core::token::sha256(state).to_vec());
        assert!(rows[0].2);
        assert!(!rows[0].1.is_empty() && &rows[0].1 != state);
        assert!(
            fx.actions()
                .await
                .contains(&"source.oauth_started".to_owned())
        );
        assert!(
            !serde_json::to_string(&fx.audit_details().await)
                .unwrap()
                .contains(state.as_str())
        );
    }

    impl Fixture {
        async fn audit_details(&self) -> Vec<Value> {
            zirv_flow_store::list_audit(&self.pool, 500)
                .await
                .unwrap()
                .into_iter()
                .map(|entry| entry.summary)
                .collect()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn start_refuses_a_key_a_missing_public_url_a_plain_manifest_and_a_missing_client_id(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.create_source(fx.manifest()).await;

        // An API key, even an admin one, cannot consent: that needs a person.
        let key = fx
            .as_key("POST", &format!("/sources/{id}/oauth/start"), None)
            .await;
        assert_eq!(key.status, StatusCode::FORBIDDEN);
        // A member is not an admin.
        let member = fx.start(&fx.member, &id).await;
        assert_eq!(member.status, StatusCode::FORBIDDEN);

        // A manifest that does not use the authorization-code grant.
        let mut plain = fx.manifest();
        plain["auth"] = json!({"type": "none"});
        let plain_id = fx.create_source(plain).await;
        let reply = fx.start(&fx.admin, &plain_id).await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(reply.body["code"], "oauth_not_configured");

        // No client id in the credential.
        let no_client = fx
            .as_key(
                "POST",
                "/sources",
                Some(json!({"kind": "http", "name": "No client", "config": {"manifest": fx.manifest()},
                            "credential": {"client_secret": "s"}})),
            )
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let reply = fx.start(&fx.admin, &no_client).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            codes(&reply),
            [("credential.client_id".into(), "is required".into())]
        );
        assert_eq!(
            fx.count("SELECT count(*) FROM source_oauth_states").await,
            0
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn start_without_a_public_url_is_a_conflict_because_the_provider_could_not_be_told_where_to_return(
        pool: PgPool,
    ) {
        let bare = fixture_with(&pool, None).await;
        let id = bare.create_source(bare.manifest()).await;

        let reply = bare.start(&bare.admin, &id).await;

        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(reply.body["code"], "public_url_not_configured");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_successful_callback_stores_both_tokens_in_one_secret_and_marks_the_source_connected(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.create_source(fx.manifest()).await;
        let before = fx.as_key("GET", &format!("/sources/{id}"), None).await;
        assert_eq!(
            before.body["oauth"],
            json!({"required": true, "connected": false, "connected_at": null})
        );
        let state = fx.state_of(&fx.admin, &id).await;

        let reply = fx
            .callback(Some(&fx.admin), &format!("code={CODE}&state={state}"))
            .await;

        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        assert_eq!(
            location(&reply),
            format!(
                "{PUBLIC_URL}/w/{}/sources/{id}?oauth=connected",
                fx.ws.as_uuid()
            )
        );
        assert_eq!(
            fx.count("SELECT count(*) FROM encrypted_secrets WHERE purpose = 'oauth_token'")
                .await,
            1
        );
        let after = fx.as_key("GET", &format!("/sources/{id}"), None).await;
        assert_eq!(after.body["oauth"]["connected"], true);
        assert!(after.body["oauth"]["connected_at"].is_string());
        // The exchange carried the PKCE verifier and the registered redirect URI.
        let forms = fx.upstream.forms.lock().unwrap().clone();
        let exchange = forms
            .iter()
            .find(|f| f["grant_type"] == "authorization_code")
            .unwrap();
        assert_eq!(
            exchange["redirect_uri"],
            format!("{PUBLIC_URL}/api/v1/oauth/callback")
        );
        assert!(exchange["code_verifier"].len() >= 43);
        // The state is spent.
        assert_eq!(
            fx.count("SELECT count(*) FROM source_oauth_states").await,
            0
        );
        let actions = fx.actions().await;
        assert!(actions.contains(&"source.oauth_connected".to_owned()));
        // Reconnecting replaces the secret's content, not the row.
        fx.connect(&id).await;
        assert_eq!(
            fx.count("SELECT count(*) FROM encrypted_secrets WHERE purpose = 'oauth_token'")
                .await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_failed_callback_stores_no_token_and_redirects_with_its_reason(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.create_source(fx.manifest()).await;
        let page = format!(
            "{PUBLIC_URL}/w/{}/sources/{id}?oauth=failed",
            fx.ws.as_uuid()
        );
        let assert_nothing_stored = |label: &'static str| {
            let pool = pool.clone();
            async move {
                let tokens: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM encrypted_secrets WHERE purpose = 'oauth_token'",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
                let linked: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM sources WHERE oauth_secret_id IS NOT NULL",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
                assert_eq!((tokens, linked), (0, 0), "{label}");
            }
        };

        // A replayed state: the first use connects nothing (the exchange is refused) and the
        // second finds the state gone.
        *fx.upstream.exchange.lock().unwrap() = Exchange::Refuse;
        let state = fx.state_of(&fx.admin, &id).await;
        let query = format!("code={CODE}&state={state}");
        let first = fx.callback(Some(&fx.admin), &query).await;
        assert_eq!(location(&first), format!("{page}&reason=exchange_failed"));
        assert_nothing_stored("the exchange was refused").await;
        *fx.upstream.exchange.lock().unwrap() = Exchange::Good;
        let replay = fx.callback(Some(&fx.admin), &query).await;
        assert_eq!(
            location(&replay),
            format!("{PUBLIC_URL}/?oauth=failed&reason=unknown_state")
        );
        assert_nothing_stored("a replayed state").await;

        // Another admin's session: the state names the person who started.
        let state = fx.state_of(&fx.admin, &id).await;
        let mismatch = fx
            .callback(Some(&fx.other_admin), &format!("code={CODE}&state={state}"))
            .await;
        assert_eq!(location(&mismatch), format!("{page}&reason=state_mismatch"));
        assert_nothing_stored("another user's session").await;
        // No session at all.
        let state = fx.state_of(&fx.admin, &id).await;
        let anonymous = fx
            .callback(None, &format!("code={CODE}&state={state}"))
            .await;
        assert_eq!(
            location(&anonymous),
            format!("{page}&reason=state_mismatch")
        );
        assert_nothing_stored("no session").await;

        // An unknown state.
        let unknown = fx
            .callback(Some(&fx.admin), &format!("code={CODE}&state=nope"))
            .await;
        assert_eq!(
            location(&unknown),
            format!("{PUBLIC_URL}/?oauth=failed&reason=unknown_state")
        );
        let missing = fx.callback(Some(&fx.admin), "code=x").await;
        assert_eq!(
            location(&missing),
            format!("{PUBLIC_URL}/?oauth=failed&reason=unknown_state")
        );
        assert_nothing_stored("an unknown state").await;

        // The provider refused the consent.
        let state = fx.state_of(&fx.admin, &id).await;
        let denied = fx
            .callback(
                Some(&fx.admin),
                &format!("error=access_denied&state={state}"),
            )
            .await;
        assert_eq!(location(&denied), format!("{page}&reason=provider_error"));
        assert_nothing_stored("a provider error").await;

        // The token endpoint answers 400.
        *fx.upstream.exchange.lock().unwrap() = Exchange::Refuse;
        let state = fx.state_of(&fx.admin, &id).await;
        let refused = fx
            .callback(Some(&fx.admin), &format!("code={CODE}&state={state}"))
            .await;
        assert_eq!(location(&refused), format!("{page}&reason=exchange_failed"));
        assert_nothing_stored("a 400 from the token endpoint").await;

        // The token endpoint gives no refresh token.
        *fx.upstream.exchange.lock().unwrap() = Exchange::NoRefreshToken;
        let state = fx.state_of(&fx.admin, &id).await;
        let none = fx
            .callback(Some(&fx.admin), &format!("code={CODE}&state={state}"))
            .await;
        assert_eq!(location(&none), format!("{page}&reason=no_refresh_token"));
        assert_nothing_stored("no refresh token").await;
        assert_eq!(
            fx.count("SELECT count(*) FROM source_oauth_states").await,
            0
        );

        // Failures with a known state are audited with their reason and no value.
        let details = serde_json::to_string(&fx.audit_details().await).unwrap();
        assert!(details.contains("no_refresh_token") && details.contains("state_mismatch"));
        for secret in [CODE, CLIENT_SECRET, "access-0", "rt-0"] {
            assert!(!details.contains(secret), "{secret} in the audit trail");
        }
        assert!(
            fx.actions()
                .await
                .contains(&"source.oauth_failed".to_owned())
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_callback_after_the_source_was_deleted_stores_nothing(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.create_source(fx.manifest()).await;
        let state = fx.state_of(&fx.admin, &id).await;
        fx.as_key("DELETE", &format!("/sources/{id}"), None).await;

        let gone = fx
            .callback(Some(&fx.admin), &format!("code={CODE}&state={state}"))
            .await;

        // Deleting the source deleted its states with it.
        assert_eq!(
            location(&gone),
            format!("{PUBLIC_URL}/?oauth=failed&reason=unknown_state")
        );
        assert_eq!(
            fx.count("SELECT count(*) FROM encrypted_secrets WHERE purpose = 'oauth_token'")
                .await,
            0
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_callback_after_the_auth_section_changed_sends_no_code_and_stores_nothing(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.create_source(fx.manifest()).await;
        let state = fx.state_of(&fx.admin, &id).await;
        let mut other_scopes = fx.manifest();
        other_scopes["auth"]["scopes"] = json!(["contacts.write"]);
        let edited = fx
            .as_key(
                "PATCH",
                &format!("/sources/{id}"),
                Some(json!({"config": {"manifest": other_scopes}})),
            )
            .await;
        assert_eq!(edited.status, StatusCode::OK, "{:?}", edited.body);

        let reply = fx
            .callback(Some(&fx.admin), &format!("code={CODE}&state={state}"))
            .await;

        assert_eq!(
            location(&reply),
            format!(
                "{PUBLIC_URL}/w/{}/sources/{id}?oauth=failed&reason=state_mismatch",
                fx.ws.as_uuid()
            )
        );
        assert!(fx.upstream.forms.lock().unwrap().is_empty());
        assert_eq!(
            fx.count("SELECT count(*) FROM encrypted_secrets WHERE purpose = 'oauth_token'")
                .await,
            0
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_callback_is_counted_by_outcome_with_no_other_label(pool: PgPool) {
        use metrics_exporter_prometheus::PrometheusBuilder;

        let fx = fixture(&pool).await;
        let id = fx.create_source(fx.manifest()).await;
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = metrics::set_default_local_recorder(&recorder);

        fx.connect(&id).await;
        fx.callback(Some(&fx.admin), "code=x&state=none").await;

        let output = handle.render();
        assert!(
            output.contains(r#"zirv_flow_source_oauth_total{outcome="connected"} 1"#),
            "{output}"
        );
        assert!(
            output.contains(r#"zirv_flow_source_oauth_total{outcome="unknown_state"} 1"#),
            "{output}"
        );
    }

    // ---- tokens at the call sites, the sample, disconnecting ----------------------------

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_connection_test_and_the_sample_use_the_stored_account_and_the_rotated_token_is_kept(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.create_source(fx.manifest()).await;
        fx.connect(&id).await;

        let test = fx
            .as_key("POST", &format!("/sources/{id}/test"), None)
            .await;
        assert_eq!(test.body, json!({"ok": true, "error": null}));
        // The seeded access token was good; the sample needs no refresh either.
        let sample = fx
            .as_key(
                "POST",
                &format!("/sources/{id}/manifest/sample"),
                Some(json!({"stream": "contacts", "limit": 3})),
            )
            .await;
        assert_eq!(sample.status, StatusCode::OK, "{:?}", sample.body);
        assert_eq!(sample.body["read"], 3);
        assert_eq!(sample.body["truncated"], true);
        assert_eq!(sample.body["records"][0]["email"], "person1@example.com");
        assert_eq!(fx.upstream.refreshes.load(Ordering::SeqCst), 0);

        // The provider revokes the access token: the next call refreshes once and the rotated
        // refresh token is the one stored.
        *fx.upstream.valid_access.lock().unwrap() = "revoked".to_owned();
        let again = fx
            .as_key(
                "POST",
                &format!("/sources/{id}/manifest/sample"),
                Some(json!({"stream": "contacts"})),
            )
            .await;
        assert_eq!(again.status, StatusCode::OK, "{:?}", again.body);
        assert_eq!(again.body["read"], 5, "default limit 10 reads all five");
        assert_eq!(again.body["truncated"], false);
        assert_eq!(fx.upstream.refreshes.load(Ordering::SeqCst), 1);
        let stored = zirv_flow_store::load_tokens(
            &pool,
            &Keyring::new(MasterKey::generate(), None).unwrap(),
            fx.ws,
            id.parse().unwrap(),
        )
        .await;
        // A different keyring cannot open it: the secret is sealed.
        assert!(matches!(
            stored,
            Err(zirv_flow_store::TokenError::Unopenable)
        ));
        assert_eq!(*fx.upstream.valid_refresh.lock().unwrap(), "rt-1");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_sample_checks_its_limit_its_draft_and_its_stream_and_maps_failures_like_discovery(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.create_source(fx.manifest()).await;
        fx.connect(&id).await;
        let tail = format!("/sources/{id}/manifest/sample");
        let sample = |body: Value| fx.as_key("POST", &tail, Some(body));

        for limit in [0, 21] {
            let reply = sample(json!({"stream": "contacts", "limit": limit})).await;
            assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{limit}");
        }
        let ghost = sample(json!({"stream": "ghost"})).await;
        assert_eq!(ghost.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            codes(&ghost),
            [("stream".into(), "is not in the manifest".into())]
        );
        let mut draft = fx.manifest();
        draft["streams"][0]["path"] = json!("no-slash");
        let bad = sample(json!({"stream": "contacts", "manifest": draft})).await;
        assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(codes(&bad).contains(&(
            "config.manifest/streams/0/path".into(),
            "path_invalid".into()
        )));
        // A draft that is valid reads with the draft: it asks for pages of one.
        let mut small = fx.manifest();
        small["streams"][0]["query"] = json!({"limit": 1});
        let drafted = sample(json!({"stream": "contacts", "manifest": small, "limit": 2})).await;
        assert_eq!(drafted.status, StatusCode::OK, "{:?}", drafted.body);
        assert_eq!(drafted.body["read"], 2);

        // The API rate-limits: the same status discovery gives a failing source.
        *fx.upstream.rate_limited.lock().unwrap() = true;
        let limited = sample(json!({"stream": "contacts"})).await;
        assert_eq!(limited.status, StatusCode::BAD_GATEWAY);
        *fx.upstream.rate_limited.lock().unwrap() = false;

        // Members and read keys cannot sample.
        let member = fx
            .as_session(
                &fx.member,
                "POST",
                &tail,
                Some(json!({"stream": "contacts"})),
            )
            .await;
        assert_eq!(member.status, StatusCode::FORBIDDEN);
        let reader = call(
            &fx.router,
            "POST",
            &fx.path(&tail),
            Some(json!({"stream": "contacts"})),
            Auth::Key(&fx.read_key),
        )
        .await;
        assert_eq!(reader.status, StatusCode::FORBIDDEN);

        // Nothing was written by any of it: no audit entry for a sample, no stream, no record.
        assert!(!fx.actions().await.iter().any(|a| a.contains("sample")));
        assert_eq!(fx.count("SELECT count(*) FROM streams").await, 0);
        assert_eq!(fx.count("SELECT count(*) FROM source_records").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sample_draft_cannot_move_the_base_url_or_the_auth_and_sends_nothing_when_it_tries(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.create_source(fx.manifest()).await;
        fx.connect(&id).await;
        let tail = format!("/sources/{id}/manifest/sample");
        let before = fx.upstream.refreshes.load(Ordering::SeqCst);
        let refresh_before = fx.upstream.valid_refresh.lock().unwrap().clone();

        let mut moved = fx.manifest();
        moved["base_url"] = json!("https://elsewhere.example.test");
        let mut other_scopes = fx.manifest();
        other_scopes["auth"]["scopes"] = json!(["contacts.write"]);
        let mut other_token_url = fx.manifest();
        other_token_url["auth"]["token_url"] = json!("https://elsewhere.example.test/token");
        for draft in [moved, other_scopes, other_token_url] {
            let reply = fx
                .as_key(
                    "POST",
                    &tail,
                    Some(json!({"stream": "contacts", "manifest": draft})),
                )
                .await;
            assert_eq!(reply.status, StatusCode::CONFLICT, "{:?}", reply.body);
            assert_eq!(reply.body["code"], "manifest_draft_auth_changed");
        }

        assert_eq!(fx.upstream.refreshes.load(Ordering::SeqCst), before);
        assert_eq!(*fx.upstream.valid_refresh.lock().unwrap(), refresh_before);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn changing_how_the_manifest_authenticates_disconnects_the_account_but_a_rate_edit_does_not(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.create_source(fx.manifest()).await;
        fx.connect(&id).await;

        let mut faster = fx.manifest();
        faster["rate_limit"] = json!({"requests": 5, "per_seconds": 1});
        let edited = fx
            .as_key(
                "PATCH",
                &format!("/sources/{id}"),
                Some(json!({"config": {"manifest": faster}})),
            )
            .await;
        assert_eq!(edited.status, StatusCode::OK, "{:?}", edited.body);
        assert_eq!(
            edited.body["oauth"]["connected"], true,
            "a rate edit keeps the account"
        );
        assert_eq!(
            edited.body["config"]["manifest"]["rate_limit"]["requests"],
            5
        );

        let mut other_scopes = fx.manifest();
        other_scopes["auth"]["scopes"] = json!(["contacts.write"]);
        let changed = fx
            .as_key(
                "PATCH",
                &format!("/sources/{id}"),
                Some(json!({"config": {"manifest": other_scopes}})),
            )
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.body);
        assert_eq!(changed.body["oauth"]["connected"], false);
        assert_eq!(changed.body["oauth"]["required"], true);
        assert_eq!(
            fx.count("SELECT count(*) FROM encrypted_secrets WHERE purpose = 'oauth_token'")
                .await,
            0
        );
        assert!(
            fx.actions()
                .await
                .contains(&"source.oauth_disconnected".to_owned())
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_a_source_deletes_its_account_tokens_and_audits_it(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.create_source(fx.manifest()).await;
        fx.connect(&id).await;

        let deleted = fx.as_key("DELETE", &format!("/sources/{id}"), None).await;

        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        // The workspace credential stays; the account tokens go with the source.
        assert_eq!(fx.count("SELECT count(*) FROM encrypted_secrets").await, 1);
        let deletions = fx
            .actions()
            .await
            .iter()
            .filter(|a| *a == "secret.deleted")
            .count();
        assert_eq!(deletions, 1, "the account tokens");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn discovery_seeds_a_new_streams_mapping_once_when_it_passes_the_checks(pool: PgPool) {
        let fx = fixture(&pool).await;
        let id = fx.create_source(fx.manifest()).await;
        fx.connect(&id).await;

        // The suggested mapping names the trait `crm_status`, which the workspace does not have:
        // the seed is dropped and the stream is left unmapped.
        let first = fx
            .as_key("POST", &format!("/sources/{id}/discover"), None)
            .await;
        assert_eq!(first.status, StatusCode::OK, "{:?}", first.body);
        assert_eq!(first.body[0]["mapping"], Value::Null);
        fx.as_key("DELETE", &format!("/sources/{id}"), None).await;

        // With the trait defined, the seed is written when the stream is first stored...
        zirv_flow_store::create_trait_definition(
            &pool,
            fx.ws,
            &zirv_flow_store::NewTraitDefinition {
                key: "crm_status".into(),
                trait_type: zirv_flow_core::people::TraitType::String,
                precedence: zirv_flow_core::people::Precedence::Latest,
                source_order: Vec::new(),
                pii: false,
                channel_address: false,
                inferred: false,
                description: String::new(),
            },
        )
        .await
        .unwrap();
        let id = fx.create_source(fx.manifest()).await;
        fx.connect(&id).await;
        let seeded = fx
            .as_key("POST", &format!("/sources/{id}/discover"), None)
            .await;
        assert_eq!(seeded.body[0]["mapping"]["columns"][0]["column"], "email");

        // ...and never over what the user saved afterwards.
        let edited = json!({"columns": [{"column": "email",
            "target": {"type": "identifier", "kind": "email"}}]});
        zirv_flow_store::set_stream_mapping(&pool, fx.ws, id.parse().unwrap(), "contacts", &edited)
            .await
            .unwrap();
        let again = fx
            .as_key("POST", &format!("/sources/{id}/discover"), None)
            .await;
        assert_eq!(
            again.body[0]["mapping"]["columns"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_oauth_tokens_and_the_client_secret_reach_no_response_audit_entry_or_table_row(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;
        let id = fx.create_source(fx.manifest()).await;
        fx.connect(&id).await;
        let sample = json!({"stream": "contacts"});
        let mut seen = Vec::new();
        let ask = |method: &'static str, tail: String, body: Option<Value>| {
            let fx = &fx;
            async move { fx.as_key(method, &tail, body).await.body.to_string() }
        };
        for (method, tail, body) in [
            ("GET", format!("/sources/{id}"), None),
            ("GET", "/sources".to_owned(), None),
            ("POST", format!("/sources/{id}/test"), None),
            ("POST", format!("/sources/{id}/discover"), None),
            (
                "POST",
                format!("/sources/{id}/manifest/sample"),
                Some(sample.clone()),
            ),
            ("GET", format!("/sources/{id}/runs"), None),
        ] {
            seen.push(ask(method, tail, body).await);
        }
        // The provider revokes the access token: the call refreshes and the rotated pair is stored.
        *fx.upstream.valid_access.lock().unwrap() = "revoked".to_owned();
        seen.push(
            ask(
                "POST",
                format!("/sources/{id}/manifest/sample"),
                Some(sample),
            )
            .await,
        );
        assert_eq!(fx.upstream.refreshes.load(Ordering::SeqCst), 1);
        seen.push(
            ask(
                "PATCH",
                format!("/sources/{id}"),
                Some(json!({"credential":
                    {"client_id": "client", "client_secret": REPLACED_CLIENT_SECRET}})),
            )
            .await,
        );
        seen.push(ask("GET", format!("/sources/{id}"), None).await);
        seen.push(ask("GET", format!("/sources/{id}/runs"), None).await);
        seen.push(serde_json::to_string(&fx.audit_details().await).unwrap());
        let everything = seen.join("\n");

        let secrets = [
            CODE,
            CLIENT_SECRET,
            REPLACED_CLIENT_SECRET,
            "access-0",
            "rt-0",
            "access-1",
            "rt-1",
        ];
        for secret in secrets {
            assert!(!everything.contains(secret), "{secret} leaked");
        }
        tables_hold_no(&pool, &secrets).await;
        the_scan_finds_a_planted(&pool, &secrets).await;
    }
}
