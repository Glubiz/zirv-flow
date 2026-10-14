//! End-to-end tests of accounts, sessions, API keys, isolation and the audit trail,
//! driven through the router.

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
    use zirv_flow_core::UserId;

    use crate::admin::{self, Context};
    use crate::{AppState, router};

    const PASSWORD: &str = "correct horse battery";
    const OWNER_EMAIL: &str = "owner@example.com";

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Value,
    }

    impl Reply {
        /// The body without the trace id, which differs on every request.
        fn problem(&self) -> Value {
            let mut body = self.body.clone();
            if let Some(object) = body.as_object_mut() {
                object.remove("trace_id");
            }
            body
        }
    }

    struct Login {
        cookie: String,
        csrf: String,
    }

    enum Auth<'a> {
        Anonymous,
        Session(&'a Login),
        SessionWithoutCsrf(&'a Login),
        SessionWithCsrf(&'a Login, &'a str),
        Key(&'a str),
    }

    async fn app(pool: &PgPool) -> Router {
        zirv_flow_store::migrate(pool).await.unwrap();
        router(
            AppState::new(pool.clone(), Arc::new(String::new)).with_hmac_root(
                &zirv_flow_core::HmacRoot::bootstrap_from(&zirv_flow_core::MasterKey::generate()),
            ),
        )
    }

    fn with_auth(
        request: axum::http::request::Builder,
        auth: Auth<'_>,
    ) -> axum::http::request::Builder {
        match auth {
            Auth::Anonymous => request,
            Auth::Session(login) => request
                .header(header::COOKIE, &login.cookie)
                .header("x-csrf-token", &login.csrf),
            Auth::SessionWithoutCsrf(login) => request.header(header::COOKIE, &login.cookie),
            Auth::SessionWithCsrf(login, csrf) => request
                .header(header::COOKIE, &login.cookie)
                .header("x-csrf-token", csrf),
            Auth::Key(key) => request.header(header::AUTHORIZATION, format!("Bearer {key}")),
        }
    }

    struct TextReply {
        status: StatusCode,
        headers: HeaderMap,
        text: String,
    }

    /// A request with a plain-text body and the given content type, and the reply as text.
    async fn call_text(
        router: &Router,
        method: &str,
        uri: &str,
        content_type: Option<&str>,
        body: String,
        auth: Auth<'_>,
    ) -> TextReply {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        let response = router
            .clone()
            .oneshot(with_auth(request, auth).body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        TextReply {
            status,
            headers,
            text: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    async fn call(
        router: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
        auth: Auth<'_>,
    ) -> Reply {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-request-id", "test-request");
        request = with_auth(request, auth);
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
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Reply {
            status,
            headers,
            body,
        }
    }

    async fn sign_in(router: &Router, email: &str, password: &str) -> Reply {
        call(
            router,
            "POST",
            "/api/v1/session",
            Some(json!({"email": email, "password": password})),
            Auth::Anonymous,
        )
        .await
    }

    async fn login(router: &Router, email: &str) -> Login {
        let reply = sign_in(router, email, PASSWORD).await;
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

    async fn owner(pool: &PgPool, router: &Router) -> Login {
        admin::create_owner(pool, &Context::system(), OWNER_EMAIL, PASSWORD)
            .await
            .unwrap();
        login(router, OWNER_EMAIL).await
    }

    async fn user(pool: &PgPool, router: &Router, email: &str) -> (UserId, Login) {
        let user = admin::create_user(pool, &Context::system(), email, PASSWORD)
            .await
            .unwrap();
        (user.id, login(router, email).await)
    }

    async fn workspace(router: &Router, owner: &Login, name: &str) -> String {
        let reply = call(
            router,
            "POST",
            "/api/v1/workspaces",
            Some(json!({"name": name})),
            Auth::Session(owner),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
        reply.body["id"].as_str().unwrap().to_owned()
    }

    async fn add_member(router: &Router, owner: &Login, ws: &str, user: UserId, role: &str) {
        let reply = call(
            router,
            "PUT",
            &format!("/api/v1/workspaces/{ws}/members/{user}"),
            Some(json!({"role": role})),
            Auth::Session(owner),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    }

    /// Creates a key as the owner and returns `(id, plaintext)`.
    async fn make_key(
        router: &Router,
        owner: &Login,
        ws: &str,
        scopes: &[&str],
    ) -> (String, String) {
        let reply = call(
            router,
            "POST",
            &format!("/api/v1/workspaces/{ws}/api-keys"),
            Some(json!({"name": "test key", "scopes": scopes})),
            Auth::Session(owner),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
        (
            reply.body["id"].as_str().unwrap().to_owned(),
            reply.body["key"].as_str().unwrap().to_owned(),
        )
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_owner_signs_in_creates_a_workspace_and_a_key_and_the_key_reads(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;

        let created = call(
            &router,
            "POST",
            "/api/v1/workspaces",
            Some(json!({"name": "Acme Corp"})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED);
        assert_eq!(created.body["slug"], "acme-corp");
        let ws = created.body["id"].as_str().unwrap().to_owned();
        let (key_id, key) = make_key(&router, &owner, &ws, &["read"]).await;
        assert!(key.starts_with("zfk_"));

        let read = call(
            &router,
            "GET",
            &format!("/api/v1/workspaces/{ws}"),
            None,
            Auth::Key(&key),
        )
        .await;

        assert_eq!(read.status, StatusCode::OK);
        assert_eq!(read.body["id"], ws.as_str());
        assert_eq!(read.body["name"], "Acme Corp");
        let listed = call(
            &router,
            "GET",
            &format!("/api/v1/workspaces/{ws}/api-keys"),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(listed.status, StatusCode::OK);
        assert_eq!(listed.body[0]["id"], key_id.as_str());
        assert!(!listed.body.to_string().contains(&key));
        assert!(listed.body[0]["last_used_at"].is_string());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn sign_in_sets_a_hardened_cookie_and_the_session_shows_the_same_csrf_token(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;

        let reply = sign_in(&router, " OWNER@example.com ", PASSWORD).await;
        let cookie = reply.headers[header::SET_COOKIE].to_str().unwrap();
        let current = call(
            &router,
            "GET",
            "/api/v1/session",
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(reply.status, StatusCode::OK);
        for attribute in ["HttpOnly", "SameSite=Lax", "Path=/", "Secure"] {
            assert!(cookie.contains(attribute), "{cookie}");
        }
        assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(reply.body["user"]["email"], OWNER_EMAIL);
        assert_eq!(reply.body["user"]["is_owner"], true);
        assert!(!reply.body.to_string().contains("argon2"));
        assert_eq!(current.status, StatusCode::OK);
        assert_eq!(current.body["csrf_token"], owner.csrf.as_str());
        assert_eq!(current.body["user"]["email"], OWNER_EMAIL);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_cookie_is_not_secure_when_the_server_is_configured_for_plain_http(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        admin::create_owner(&pool, &Context::system(), OWNER_EMAIL, PASSWORD)
            .await
            .unwrap();
        let router =
            router(AppState::new(pool.clone(), Arc::new(String::new)).with_cookie_secure(false));

        let reply = sign_in(&router, OWNER_EMAIL, PASSWORD).await;

        let cookie = reply.headers[header::SET_COOKIE].to_str().unwrap();
        assert!(!cookie.contains("Secure"), "{cookie}");
        assert!(cookie.contains("HttpOnly"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cookie_mutation_needs_a_valid_csrf_token(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let body = || Some(json!({"name": "Acme"}));

        let missing = call(
            &router,
            "POST",
            "/api/v1/workspaces",
            body(),
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;
        let wrong = call(
            &router,
            "POST",
            "/api/v1/workspaces",
            body(),
            Auth::SessionWithCsrf(&owner, "nope"),
        )
        .await;
        let other_session = login(&router, OWNER_EMAIL).await;
        let foreign = call(
            &router,
            "POST",
            "/api/v1/workspaces",
            body(),
            Auth::SessionWithCsrf(&owner, &other_session.csrf),
        )
        .await;
        let ok = call(
            &router,
            "POST",
            "/api/v1/workspaces",
            body(),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(missing.status, StatusCode::FORBIDDEN);
        assert_eq!(
            missing.headers[header::CONTENT_TYPE],
            "application/problem+json"
        );
        assert_eq!(wrong.status, StatusCode::FORBIDDEN);
        assert_eq!(foreign.status, StatusCode::FORBIDDEN);
        assert_eq!(ok.status, StatusCode::CREATED);
        let count = zirv_flow_store::list_workspaces_for_user(&pool, UserId::new(), true)
            .await
            .unwrap()
            .len();
        assert_eq!(
            count, 1,
            "only the request with the token created a workspace"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn csrf_applies_to_every_mutating_method_but_not_to_reads_or_sign_in(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (user_id, _) = user(&pool, &router, "jane@example.com").await;
        let (key_id, _) = make_key(&router, &owner, &ws, &["read"]).await;

        let put = call(
            &router,
            "PUT",
            &format!("/api/v1/workspaces/{ws}/members/{user_id}"),
            Some(json!({"role": "member"})),
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;
        let delete_key = call(
            &router,
            "DELETE",
            &format!("/api/v1/workspaces/{ws}/api-keys/{key_id}"),
            None,
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;
        let create_user = call(
            &router,
            "POST",
            "/api/v1/users",
            Some(json!({"email": "x@example.com", "password": PASSWORD})),
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;
        let sign_out = call(
            &router,
            "DELETE",
            "/api/v1/session",
            None,
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;
        let read = call(
            &router,
            "GET",
            &format!("/api/v1/workspaces/{ws}"),
            None,
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;

        assert_eq!(put.status, StatusCode::FORBIDDEN);
        assert_eq!(delete_key.status, StatusCode::FORBIDDEN);
        assert_eq!(create_user.status, StatusCode::FORBIDDEN);
        assert_eq!(sign_out.status, StatusCode::FORBIDDEN);
        assert_eq!(read.status, StatusCode::OK);
        let sign_in = sign_in(&router, OWNER_EMAIL, PASSWORD).await;
        assert_eq!(sign_in.status, StatusCode::OK);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_api_key_mutates_without_a_csrf_token(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (_, admin_key) = make_key(&router, &owner, &ws, &["admin"]).await;

        let created = call(
            &router,
            "POST",
            &format!("/api/v1/workspaces/{ws}/api-keys"),
            Some(json!({"name": "second", "scopes": ["read"]})),
            Auth::Key(&admin_key),
        )
        .await;

        assert_eq!(created.status, StatusCode::CREATED);
        assert!(created.body["created_by"].is_null());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_workspace_admin_and_a_key_of_a_get_404_for_every_resource_of_b(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let a = workspace(&router, &owner, "Workspace A").await;
        let b = workspace(&router, &owner, "Workspace B").await;
        let (admin_id, admin_a) = user(&pool, &router, "admin-a@example.com").await;
        add_member(&router, &owner, &a, admin_id, "admin").await;
        let (_, key_a) = make_key(&router, &owner, &a, &["read", "write", "admin"]).await;
        let (key_b_id, _) = make_key(&router, &owner, &b, &["read"]).await;
        let (stranger, _) = user(&pool, &router, "stranger@example.com").await;
        let missing_workspace = "00000000-0000-0000-0000-00000000dead";

        let requests: Vec<(&str, String, Option<Value>)> = vec![
            ("GET", format!("/api/v1/workspaces/{b}"), None),
            ("GET", format!("/api/v1/workspaces/{b}/members"), None),
            (
                "PUT",
                format!("/api/v1/workspaces/{b}/members/{stranger}"),
                Some(json!({"role": "admin"})),
            ),
            (
                "DELETE",
                format!("/api/v1/workspaces/{b}/members/{stranger}"),
                None,
            ),
            ("GET", format!("/api/v1/workspaces/{b}/api-keys"), None),
            (
                "POST",
                format!("/api/v1/workspaces/{b}/api-keys"),
                Some(json!({"name": "x", "scopes": ["read"]})),
            ),
            (
                "DELETE",
                format!("/api/v1/workspaces/{b}/api-keys/{key_b_id}"),
                None,
            ),
        ];
        for (method, uri, body) in requests {
            let by_admin = call(&router, method, &uri, body.clone(), Auth::Session(&admin_a)).await;
            let by_key = call(&router, method, &uri, body.clone(), Auth::Key(&key_a)).await;
            let unknown_uri = uri.replace(&b, missing_workspace);
            let unknown = call(&router, method, &unknown_uri, body, Auth::Session(&admin_a)).await;

            assert_eq!(
                by_admin.status,
                StatusCode::NOT_FOUND,
                "admin {method} {uri}"
            );
            assert_eq!(by_key.status, StatusCode::NOT_FOUND, "key {method} {uri}");
            assert_eq!(
                by_admin.problem(),
                unknown.problem(),
                "a workspace the caller cannot see looks like one that does not exist"
            );
        }
        // B's key is still alive: nothing above reached it.
        let listed = call(
            &router,
            "GET",
            &format!("/api/v1/workspaces/{b}/api-keys"),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert!(listed.body[0]["revoked_at"].is_null());
        let b_members = call(
            &router,
            "GET",
            &format!("/api/v1/workspaces/{b}/members"),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(b_members.body, json!([]));
        // And A is fully usable by the same callers.
        let own = call(
            &router,
            "GET",
            &format!("/api/v1/workspaces/{a}"),
            None,
            Auth::Key(&key_a),
        )
        .await;
        assert_eq!(own.status, StatusCode::OK);
        let own_keys = call(
            &router,
            "GET",
            &format!("/api/v1/workspaces/{a}/api-keys"),
            None,
            Auth::Session(&admin_a),
        )
        .await;
        assert_eq!(own_keys.status, StatusCode::OK);
        let listing = call(
            &router,
            "GET",
            "/api/v1/workspaces",
            None,
            Auth::Session(&admin_a),
        )
        .await;
        assert_eq!(listing.body.as_array().unwrap().len(), 1);
        assert_eq!(listing.body[0]["id"], a.as_str());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_member_reads_but_cannot_manage_members_or_keys(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (key_id, _) = make_key(&router, &owner, &ws, &["read"]).await;

        let read = call(
            &router,
            "GET",
            &format!("/api/v1/workspaces/{ws}"),
            None,
            Auth::Session(&member),
        )
        .await;
        let members = call(
            &router,
            "GET",
            &format!("/api/v1/workspaces/{ws}/members"),
            None,
            Auth::Session(&member),
        )
        .await;
        let list_keys = call(
            &router,
            "GET",
            &format!("/api/v1/workspaces/{ws}/api-keys"),
            None,
            Auth::Session(&member),
        )
        .await;
        let create_key = call(
            &router,
            "POST",
            &format!("/api/v1/workspaces/{ws}/api-keys"),
            Some(json!({"name": "x", "scopes": ["read"]})),
            Auth::Session(&member),
        )
        .await;
        let revoke = call(
            &router,
            "DELETE",
            &format!("/api/v1/workspaces/{ws}/api-keys/{key_id}"),
            None,
            Auth::Session(&member),
        )
        .await;
        let promote = call(
            &router,
            "PUT",
            &format!("/api/v1/workspaces/{ws}/members/{member_id}"),
            Some(json!({"role": "admin"})),
            Auth::Session(&member),
        )
        .await;
        let remove = call(
            &router,
            "DELETE",
            &format!("/api/v1/workspaces/{ws}/members/{member_id}"),
            None,
            Auth::Session(&member),
        )
        .await;

        assert_eq!(read.status, StatusCode::OK);
        assert_eq!(members.status, StatusCode::OK);
        assert_eq!(members.body[0]["role"], "member");
        for (name, reply) in [
            ("list keys", list_keys),
            ("create key", create_key),
            ("revoke key", revoke),
            ("promote self", promote),
            ("remove member", remove),
        ] {
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{name}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn key_scopes_limit_what_a_key_may_do_in_its_own_workspace(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let (_, write_key) = make_key(&router, &owner, &ws, &["write"]).await;
        let (_, ingest_key) = make_key(&router, &owner, &ws, &["ingest"]).await;
        let (_, admin_key) = make_key(&router, &owner, &ws, &["admin"]).await;
        let ws_uri = format!("/api/v1/workspaces/{ws}");
        let keys_uri = format!("{ws_uri}/api-keys");
        let members_uri = format!("{ws_uri}/members");

        let status = |reply: Reply| reply.status;
        assert_eq!(
            status(call(&router, "GET", &ws_uri, None, Auth::Key(&read_key)).await),
            StatusCode::OK
        );
        assert_eq!(
            status(call(&router, "GET", &members_uri, None, Auth::Key(&read_key)).await),
            StatusCode::OK
        );
        assert_eq!(
            status(call(&router, "GET", &keys_uri, None, Auth::Key(&read_key)).await),
            StatusCode::FORBIDDEN
        );
        for key in [&write_key, &ingest_key] {
            assert_eq!(
                status(call(&router, "GET", &ws_uri, None, Auth::Key(key)).await),
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                status(call(&router, "GET", &keys_uri, None, Auth::Key(key)).await),
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            status(call(&router, "GET", &keys_uri, None, Auth::Key(&admin_key)).await),
            StatusCode::OK
        );
        assert_eq!(
            status(call(&router, "GET", &members_uri, None, Auth::Key(&admin_key)).await),
            StatusCode::OK
        );
        assert_eq!(
            status(call(&router, "GET", &ws_uri, None, Auth::Key(&admin_key)).await),
            StatusCode::FORBIDDEN
        );
        let create = call(
            &router,
            "POST",
            &keys_uri,
            Some(json!({"name": "x", "scopes": ["read"]})),
            Auth::Key(&read_key),
        )
        .await;
        assert_eq!(create.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_api_key_never_reaches_instance_level_endpoints(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (_, key) = make_key(&router, &owner, &ws, &["read", "write", "admin", "ingest"]).await;

        let attempts = [
            ("GET", "/api/v1/users", None),
            (
                "POST",
                "/api/v1/users",
                Some(json!({"email": "x@example.com", "password": PASSWORD})),
            ),
            ("GET", "/api/v1/workspaces", None),
            ("POST", "/api/v1/workspaces", Some(json!({"name": "Other"}))),
            ("GET", "/api/v1/session", None),
            ("DELETE", "/api/v1/session", None),
        ];
        for (method, uri, body) in attempts {
            let reply = call(&router, method, uri, body, Auth::Key(&key)).await;

            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        }
        assert_eq!(zirv_flow_store::list_users(&pool).await.unwrap().len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_the_owner_manages_users_and_creates_workspaces(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let (_, jane) = user(&pool, &router, "jane@example.com").await;

        let list = call(&router, "GET", "/api/v1/users", None, Auth::Session(&jane)).await;
        let create_user = call(
            &router,
            "POST",
            "/api/v1/users",
            Some(json!({"email": "x@example.com", "password": PASSWORD})),
            Auth::Session(&jane),
        )
        .await;
        let create_ws = call(
            &router,
            "POST",
            "/api/v1/workspaces",
            Some(json!({"name": "Mine"})),
            Auth::Session(&jane),
        )
        .await;
        let owner_list = call(&router, "GET", "/api/v1/users", None, Auth::Session(&owner)).await;

        assert_eq!(list.status, StatusCode::FORBIDDEN);
        assert_eq!(create_user.status, StatusCode::FORBIDDEN);
        assert_eq!(create_ws.status, StatusCode::FORBIDDEN);
        assert_eq!(owner_list.status, StatusCode::OK);
        assert_eq!(owner_list.body.as_array().unwrap().len(), 2);
        assert!(!owner_list.body.to_string().contains("argon2"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn validation_failures_are_422_naming_the_field_without_echoing_secrets(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;

        let short = call(
            &router,
            "POST",
            "/api/v1/users",
            Some(json!({"email": "a@example.com", "password": "hunter2"})),
            Auth::Session(&owner),
        )
        .await;
        let bad_email = call(
            &router,
            "POST",
            "/api/v1/users",
            Some(json!({"email": "not-an-email", "password": PASSWORD})),
            Auth::Session(&owner),
        )
        .await;
        let bad_slug = call(
            &router,
            "POST",
            "/api/v1/workspaces",
            Some(json!({"name": "A", "slug": "Bad Slug"})),
            Auth::Session(&owner),
        )
        .await;
        let bad_json = call(
            &router,
            "POST",
            "/api/v1/users",
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(short.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            short.body["detail"]
                .as_str()
                .unwrap()
                .starts_with("password:")
        );
        assert!(!short.body.to_string().contains("hunter2"));
        assert_eq!(bad_email.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            bad_email.body["detail"]
                .as_str()
                .unwrap()
                .starts_with("email:")
        );
        assert_eq!(bad_slug.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            bad_slug.body["detail"]
                .as_str()
                .unwrap()
                .starts_with("slug:")
        );
        assert_eq!(bad_json.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(bad_json.body["type"], "about:blank");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn duplicate_emails_and_slugs_are_409(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        workspace(&router, &owner, "Acme").await;

        let same_slug = call(
            &router,
            "POST",
            "/api/v1/workspaces",
            Some(json!({"name": "Other", "slug": "acme"})),
            Auth::Session(&owner),
        )
        .await;
        let same_email = call(
            &router,
            "POST",
            "/api/v1/users",
            Some(json!({"email": "OWNER@example.com", "password": PASSWORD})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(same_slug.status, StatusCode::CONFLICT);
        assert_eq!(same_email.status, StatusCode::CONFLICT);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unknown_email_and_a_wrong_password_give_identical_responses(pool: PgPool) {
        let router = app(&pool).await;
        owner(&pool, &router).await;

        let unknown = sign_in(&router, "nobody@example.com", PASSWORD).await;
        let wrong = sign_in(&router, OWNER_EMAIL, "not the right password").await;

        assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
        assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
        assert_eq!(unknown.problem(), wrong.problem());
        assert_eq!(unknown.body["detail"], "invalid email or password");
        assert!(unknown.headers.get(header::SET_COOKIE).is_none());
        assert!(wrong.headers.get(header::SET_COOKIE).is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn five_failures_lock_the_account_with_the_same_response_and_a_success_resets(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        owner(&pool, &router).await;
        let wrong = sign_in(&router, OWNER_EMAIL, "not the right password").await;

        for _ in 0..4 {
            sign_in(&router, OWNER_EMAIL, "not the right password").await;
        }
        let locked: bool = sqlx::query_scalar("SELECT locked_until IS NOT NULL FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(locked, "the fifth failure locks the account");
        // The first lock lasts one second, shorter than a sign-in on a slow runner.
        sqlx::query("UPDATE users SET locked_until = now() + interval '1 hour'")
            .execute(&pool)
            .await
            .unwrap();
        let locked_out = sign_in(&router, OWNER_EMAIL, PASSWORD).await;

        assert_eq!(
            locked_out.status,
            StatusCode::UNAUTHORIZED,
            "the correct password is refused while locked"
        );
        assert_eq!(locked_out.problem(), wrong.problem());

        sqlx::query("UPDATE users SET locked_until = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        let after = sign_in(&router, OWNER_EMAIL, PASSWORD).await;
        assert_eq!(after.status, StatusCode::OK);
        let (failures, until): (i32, Option<time::OffsetDateTime>) =
            sqlx::query_as("SELECT failed_sign_ins, locked_until FROM users")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(failures, 0);
        assert!(until.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn four_failures_do_not_lock_the_account(pool: PgPool) {
        let router = app(&pool).await;
        owner(&pool, &router).await;

        for _ in 0..4 {
            sign_in(&router, OWNER_EMAIL, "not the right password").await;
        }
        let reply = sign_in(&router, OWNER_EMAIL, PASSWORD).await;

        assert_eq!(reply.status, StatusCode::OK);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn revoked_malformed_and_wrong_keys_are_401_with_one_body(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (id, key) = make_key(&router, &owner, &ws, &["read"]).await;
        let uri = format!("/api/v1/workspaces/{ws}");
        let before = call(&router, "GET", &uri, None, Auth::Key(&key)).await;
        let wrong_secret = format!("{}x", key);
        let other_id = zirv_flow_core::ApiKeyId::new();
        let secret = crate::credentials::parse_key(&key).unwrap().1;
        let unknown_id = format!("zfk_{}_{secret}", other_id.as_uuid().simple());

        let revoke = call(
            &router,
            "DELETE",
            &format!("{uri}/api-keys/{id}"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let revoked = call(&router, "GET", &uri, None, Auth::Key(&key)).await;
        let repeat = call(
            &router,
            "DELETE",
            &format!("{uri}/api-keys/{id}"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let malformed = call(&router, "GET", &uri, None, Auth::Key("not-a-key")).await;
        let wrong = call(&router, "GET", &uri, None, Auth::Key(&wrong_secret)).await;
        let unknown = call(&router, "GET", &uri, None, Auth::Key(&unknown_id)).await;
        let none = call(&router, "GET", &uri, None, Auth::Anonymous).await;

        assert_eq!(before.status, StatusCode::OK);
        assert_eq!(revoke.status, StatusCode::NO_CONTENT);
        assert_eq!(repeat.status, StatusCode::NOT_FOUND);
        for reply in [&revoked, &malformed, &wrong, &unknown, &none] {
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
            assert_eq!(reply.problem(), revoked.problem());
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_bad_bearer_header_does_not_fall_back_to_a_valid_cookie(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let request = Request::get("/api/v1/session")
            .header(header::COOKIE, &owner.cookie)
            .header(header::AUTHORIZATION, "Bearer zfk_garbage")
            .body(Body::empty())
            .unwrap();

        let response = router.clone().oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn expired_idle_and_signed_out_sessions_are_401(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let uri = "/api/v1/session";
        let ok = call(&router, "GET", uri, None, Auth::Session(&owner)).await;

        sqlx::query("UPDATE sessions SET last_seen_at = now() - interval '13 hours'")
            .execute(&pool)
            .await
            .unwrap();
        let idle = call(&router, "GET", uri, None, Auth::Session(&owner)).await;
        sqlx::query(
            "UPDATE sessions SET last_seen_at = now(), expires_at = now() - interval '1 second'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let expired = call(&router, "GET", uri, None, Auth::Session(&owner)).await;

        assert_eq!(ok.status, StatusCode::OK);
        assert_eq!(idle.status, StatusCode::UNAUTHORIZED);
        assert_eq!(expired.status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn signing_out_ends_the_session_and_clears_the_cookie(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;

        let out = call(
            &router,
            "DELETE",
            "/api/v1/session",
            None,
            Auth::Session(&owner),
        )
        .await;
        let after = call(
            &router,
            "GET",
            "/api/v1/session",
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(out.status, StatusCode::NO_CONTENT);
        let cleared = out.headers[header::SET_COOKIE].to_str().unwrap();
        assert!(cleared.contains("Max-Age=0"), "{cleared}");
        assert_eq!(after.status, StatusCode::UNAUTHORIZED);
        let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sessions, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_database_holds_only_hashes_of_tokens_and_keys(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (_, key) = make_key(&router, &owner, &ws, &["read"]).await;
        let session_token = owner.cookie.split_once('=').unwrap().1;
        let key_secret = crate::credentials::parse_key(&key).unwrap().1;

        let (token_hash, csrf_hash): (Vec<u8>, Vec<u8>) =
            sqlx::query_as("SELECT token_hash, csrf_hash FROM sessions")
                .fetch_one(&pool)
                .await
                .unwrap();
        let secret_hash: Vec<u8> = sqlx::query_scalar("SELECT secret_hash FROM api_keys")
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(token_hash, zirv_flow_core::token::sha256(session_token));
        assert_eq!(csrf_hash, zirv_flow_core::token::sha256(&owner.csrf));
        assert_eq!(secret_hash, zirv_flow_core::token::sha256(key_secret));
        for stored in [&token_hash, &csrf_hash, &secret_hash] {
            assert_eq!(stored.len(), 32);
        }
        let hash: String = sqlx::query_scalar("SELECT password_hash FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(!hash.contains(PASSWORD));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_listed_event_is_audited_and_no_row_holds_personal_data(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Acme").await;
        let (jane_id, jane) = user(&pool, &router, "jane@example.com").await;
        add_member(&router, &owner, &ws, jane_id, "member").await;
        let (key_id, key) = make_key(&router, &owner, &ws, &["read"]).await;
        call(
            &router,
            "DELETE",
            &format!("/api/v1/workspaces/{ws}/members/{jane_id}"),
            None,
            Auth::Session(&owner),
        )
        .await;
        call(
            &router,
            "DELETE",
            &format!("/api/v1/workspaces/{ws}/api-keys/{key_id}"),
            None,
            Auth::Session(&owner),
        )
        .await;
        sign_in(&router, "jane@example.com", "wrong password here").await;
        sign_in(&router, "ghost@example.com", "wrong password here").await;
        for _ in 0..5 {
            sign_in(&router, "jane@example.com", "wrong password here").await;
        }
        sign_in(&router, "jane@example.com", PASSWORD).await;
        call(
            &router,
            "DELETE",
            "/api/v1/session",
            None,
            Auth::Session(&jane),
        )
        .await;
        admin::reset_password(
            &pool,
            &Context::system(),
            "jane@example.com",
            "a brand new password",
        )
        .await
        .unwrap();

        let rows = zirv_flow_store::list_audit(&pool, 500).await.unwrap();

        let actions: std::collections::BTreeSet<&str> =
            rows.iter().map(|row| row.action.as_str()).collect();
        for expected in [
            "session.signed_in",
            "session.sign_in_failed",
            "session.signed_out",
            "user.created",
            "user.password_reset",
            "workspace.created",
            "membership.set",
            "membership.removed",
            "api_key.created",
            "api_key.revoked",
        ] {
            assert!(
                actions.contains(expected),
                "missing {expected}: {actions:?}"
            );
        }
        let reasons: std::collections::BTreeSet<String> = rows
            .iter()
            .filter(|row| row.action == "session.sign_in_failed")
            .map(|row| row.summary["reason"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            reasons,
            ["invalid_credentials".to_owned(), "locked".to_owned()].into()
        );
        let ghost = rows
            .iter()
            .find(|row| row.action == "session.sign_in_failed" && row.actor_kind == "anonymous")
            .expect("a failed sign-in for an unknown email is recorded as anonymous");
        assert!(ghost.actor_id.is_none());
        // Nobody has proved who they are, so no failed sign-in names a user as the actor;
        // a known account is only the object.
        let failures: Vec<_> = rows
            .iter()
            .filter(|row| row.action == "session.sign_in_failed")
            .collect();
        assert!(failures.iter().all(|row| row.actor_kind == "anonymous"));
        assert!(failures.iter().all(|row| row.actor_id.is_none()));
        assert!(
            failures
                .iter()
                .any(|row| row.object_kind.as_deref() == Some("user") && row.object_id.is_some())
        );
        let created = rows
            .iter()
            .find(|row| row.action == "workspace.created")
            .unwrap();
        assert_eq!(created.actor_kind, "user");
        assert_eq!(created.request_id.as_deref(), Some("test-request"));
        assert_eq!(
            created.workspace_id.map(|id| id.to_string()),
            Some(ws.clone())
        );

        let dump = rows
            .iter()
            .map(|row| format!("{row:?}"))
            .collect::<Vec<_>>()
            .join("\n");
        let session_token = owner.cookie.split_once('=').unwrap().1;
        let key_secret = crate::credentials::parse_key(&key).unwrap().1;
        for secret in [
            "owner@example.com",
            "jane@example.com",
            "ghost@example.com",
            "example.com",
            PASSWORD,
            "wrong password here",
            "a brand new password",
            session_token,
            owner.csrf.as_str(),
            key.as_str(),
            key_secret,
        ] {
            assert!(!dump.contains(secret), "the audit log contains {secret:?}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_unauthenticated_get_of_a_protected_route_is_401_problem_json(pool: PgPool) {
        let router = app(&pool).await;

        let reply = call(&router, "GET", "/api/v1/workspaces", None, Auth::Anonymous).await;

        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            reply.headers[header::CONTENT_TYPE],
            "application/problem+json"
        );
        assert_eq!(reply.body["request_id"], "test-request");
    }

    async fn record(
        pool: &PgPool,
        ws: &str,
        trace: &zirv_flow_core::TraceContext,
        action: &'static str,
    ) {
        let decision = zirv_flow_store::NewDecision {
            workspace_id: ws.parse().unwrap(),
            trace: trace.clone(),
            action,
            outcome: "done",
            reason: None,
            subject_ids: Vec::new(),
            job: None,
        };
        zirv_flow_store::append_decision(pool, &decision)
            .await
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_trace_endpoint_returns_a_workspaces_records_and_nothing_of_another(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let a = workspace(&router, &owner, "Alpha").await;
        let b = workspace(&router, &owner, "Beta").await;
        let trace = zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("req-1"));
        record(&pool, &a, &trace, "first").await;
        record(&pool, &a, &trace, "second").await;
        let uri = |ws: &str, id: &str| format!("/api/v1/workspaces/{ws}/traces/{id}");
        let id = trace.trace_id.to_string();

        let found = call(&router, "GET", &uri(&a, &id), None, Auth::Session(&owner)).await;
        let elsewhere = call(&router, "GET", &uri(&b, &id), None, Auth::Session(&owner)).await;
        let unknown = call(
            &router,
            "GET",
            &uri(&a, &zirv_flow_core::TraceId::new().to_string()),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(found.status, StatusCode::OK, "{:?}", found.body);
        assert_eq!(found.body["trace_id"], id.as_str());
        assert_eq!(found.body["truncated"], false);
        let actions: Vec<&str> = found.body["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record["action"].as_str().unwrap())
            .collect();
        assert_eq!(actions, ["first", "second"]);
        assert_eq!(
            found.body["records"][0]["cause"],
            json!({"kind": "api", "id": "req-1"})
        );
        assert_eq!(elsewhere.status, StatusCode::NOT_FOUND);
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        assert_eq!(elsewhere.problem(), unknown.problem());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_malformed_trace_id_is_a_400_problem(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;

        for bad in [
            "nope",
            "0123456789ABCDEF0123456789ABCDEF",
            "01234567-89ab-cdef-0123-456789abcdef",
        ] {
            let reply = call(
                &router,
                "GET",
                &format!("/api/v1/workspaces/{ws}/traces/{bad}"),
                None,
                Auth::Session(&owner),
            )
            .await;

            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(
                reply.headers[header::CONTENT_TYPE],
                "application/problem+json"
            );
            assert!(reply.body["trace_id"].is_string());
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_trace_endpoint_follows_the_workspace_read_rules(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let other = workspace(&router, &owner, "Beta").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (_, outsider) = user(&pool, &router, "outsider@example.com").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let (_, write_key) = make_key(&router, &owner, &ws, &["write"]).await;
        let (_, other_key) = make_key(&router, &owner, &other, &["read"]).await;
        let trace = zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("req-1"));
        record(&pool, &ws, &trace, "step").await;
        let uri = format!("/api/v1/workspaces/{ws}/traces/{}", trace.trace_id);
        let status = |auth| {
            let router = router.clone();
            let uri = uri.clone();
            async move { call(&router, "GET", &uri, None, auth).await.status }
        };

        assert_eq!(status(Auth::Anonymous).await, StatusCode::UNAUTHORIZED);
        assert_eq!(status(Auth::Session(&member)).await, StatusCode::OK);
        assert_eq!(status(Auth::Key(&read_key)).await, StatusCode::OK);
        assert_eq!(status(Auth::Key(&write_key)).await, StatusCode::FORBIDDEN);
        assert_eq!(status(Auth::Key(&other_key)).await, StatusCode::NOT_FOUND);
        assert_eq!(
            status(Auth::Session(&outsider)).await,
            StatusCode::NOT_FOUND
        );
    }

    const SETUP_PASSWORD: &str = "a first-run password";

    /// A router over a migrated database, with its state, for tests that look inside it.
    async fn app_with_state(pool: &PgPool) -> (Router, AppState) {
        zirv_flow_store::migrate(pool).await.unwrap();
        let state = AppState::new(pool.clone(), Arc::new(String::new));
        (router(state.clone()), state)
    }

    async fn post_setup(router: &Router, token: Option<&str>, email: &str) -> Reply {
        let mut body = json!({"email": email, "password": SETUP_PASSWORD});
        if let Some(token) = token {
            body["token"] = json!(token);
        }
        call(router, "POST", "/api/v1/setup", Some(body), Auth::Anonymous).await
    }

    async fn user_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM users")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn setup_is_required_on_an_empty_database_and_not_once_an_owner_exists(pool: PgPool) {
        let router = app(&pool).await;

        let before = call(&router, "GET", "/api/v1/setup", None, Auth::Anonymous).await;
        owner(&pool, &router).await;
        let after = call(&router, "GET", "/api/v1/setup", None, Auth::Anonymous).await;

        assert_eq!(before.status, StatusCode::OK);
        assert_eq!(before.body, json!({"required": true}));
        assert_eq!(after.status, StatusCode::OK);
        assert_eq!(after.body, json!({"required": false}));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_right_token_creates_the_owner_signs_them_in_clears_the_token_and_audits(
        pool: PgPool,
    ) {
        let (router, state) = app_with_state(&pool).await;
        let token = state.mint_setup_token().await.unwrap().unwrap();

        let reply = post_setup(&router, Some(&token), "First@Example.com").await;

        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        assert_eq!(reply.body["user"]["email"], "first@example.com");
        assert_eq!(reply.body["user"]["is_owner"], true);
        let csrf = reply.body["csrf_token"].as_str().unwrap().to_owned();
        let cookie = reply.headers[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.starts_with("zf_session="), "{cookie}");
        assert!(cookie.contains("HttpOnly"));
        assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
        assert!(state.setup_token().is_none(), "the token is spent");
        let login = Login {
            cookie: cookie.split(';').next().unwrap().to_owned(),
            csrf,
        };
        let current = call(
            &router,
            "GET",
            "/api/v1/session",
            None,
            Auth::Session(&login),
        )
        .await;
        assert_eq!(current.status, StatusCode::OK);
        let rows = zirv_flow_store::list_audit(&pool, 10).await.unwrap();
        let created = rows
            .iter()
            .find(|row| row.action == "user.created")
            .expect("the owner creation is audited");
        assert_eq!(created.actor_kind, "anonymous");
        assert_eq!(created.summary["via"], "first-run setup");
        assert_eq!(created.request_id.as_deref(), Some("test-request"));
        let serialised =
            serde_json::to_string(&rows.iter().map(|r| &r.summary).collect::<Vec<_>>()).unwrap();
        assert!(!serialised.contains(&token) && !serialised.contains(SETUP_PASSWORD));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_wrong_missing_or_unminted_token_is_refused_and_creates_nothing(pool: PgPool) {
        let (router, state) = app_with_state(&pool).await;
        let unminted = post_setup(&router, Some("anything"), "a@example.com").await;
        let token = state.mint_setup_token().await.unwrap().unwrap();

        let wrong = post_setup(&router, Some("not-the-token"), "a@example.com").await;
        let missing = post_setup(&router, None, "a@example.com").await;
        let empty = post_setup(&router, Some(""), "a@example.com").await;

        for reply in [&unminted, &wrong, &missing, &empty] {
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{:?}", reply.body);
            assert_eq!(reply.body["detail"], "invalid setup token");
            assert!(!reply.headers.contains_key(header::SET_COOKIE));
        }
        assert_eq!(user_count(&pool).await, 0);
        assert!(state.setup_token().is_some(), "a refusal keeps the token");
        let right = post_setup(&router, Some(&token), "a@example.com").await;
        assert_eq!(right.status, StatusCode::OK);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn setup_with_an_existing_owner_is_a_conflict_whatever_the_token_says(pool: PgPool) {
        let (router, state) = app_with_state(&pool).await;
        let token = state.mint_setup_token().await.unwrap().unwrap();
        admin::create_owner(&pool, &Context::system(), OWNER_EMAIL, PASSWORD)
            .await
            .unwrap();

        let right = post_setup(&router, Some(&token), "b@example.com").await;
        let wrong = post_setup(&router, Some("nope"), "b@example.com").await;

        assert_eq!(right.status, StatusCode::CONFLICT);
        assert_eq!(wrong.status, StatusCode::CONFLICT);
        assert_eq!(user_count(&pool).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_concurrent_correct_setups_make_exactly_one_owner(pool: PgPool) {
        let (router, state) = app_with_state(&pool).await;
        let token = state.mint_setup_token().await.unwrap().unwrap();

        let (first, second) = tokio::join!(
            post_setup(&router, Some(&token), "one@example.com"),
            post_setup(&router, Some(&token), "two@example.com"),
        );

        let mut statuses = [first.status, second.status];
        statuses.sort();
        assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);
        let owners: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE is_owner")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(owners, 1);
        assert_eq!(user_count(&pool).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn setup_validates_the_email_and_password_only_after_the_token(pool: PgPool) {
        let (router, state) = app_with_state(&pool).await;
        let token = state.mint_setup_token().await.unwrap().unwrap();

        let bad_email = post_setup(&router, Some(&token), "nope").await;
        let short = call(
            &router,
            "POST",
            "/api/v1/setup",
            Some(json!({"token": token, "email": "a@example.com", "password": "short"})),
            Auth::Anonymous,
        )
        .await;
        let unauthorised = post_setup(&router, Some("wrong"), "nope").await;

        assert_eq!(bad_email.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(short.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(unauthorised.status, StatusCode::FORBIDDEN);
        assert!(state.setup_token().is_some());
        assert_eq!(user_count(&pool).await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn no_token_is_minted_when_an_owner_exists_and_each_mint_replaces_the_last(pool: PgPool) {
        let (router, state) = app_with_state(&pool).await;
        let first = state.mint_setup_token().await.unwrap().unwrap();
        let second = state.mint_setup_token().await.unwrap().unwrap();

        assert_ne!(first, second);
        assert_eq!(
            post_setup(&router, Some(&first), "a@example.com")
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        admin::create_owner(&pool, &Context::system(), OWNER_EMAIL, PASSWORD)
            .await
            .unwrap();
        assert_eq!(state.mint_setup_token().await.unwrap(), None);
    }

    /// Inserts a job directly, with a payload that must never be shown. Returns its id and trace.
    async fn seed_job(
        pool: &PgPool,
        kind: &str,
        state: &str,
        workspace: Option<&str>,
        unique_key: Option<&str>,
        dead_at: Option<&str>,
    ) -> (uuid::Uuid, uuid::Uuid) {
        let id = uuid::Uuid::new_v4();
        let trace = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO jobs (id, workspace_id, kind, payload, state, attempts, max_attempts, \
                last_error, unique_key, trace_id, cause_kind, cause_id, dead_at) \
             VALUES ($1, $2::text::uuid, $3, '{\"secret\": \"hunter2\"}', $4, 5, 5, \
                CASE WHEN $4 = 'dead' THEN 'boom' END, $5, $6, 'schedule', 'sched-1', \
                $7::text::timestamptz)",
        )
        .bind(id)
        .bind(workspace)
        .bind(kind)
        .bind(state)
        .bind(unique_key)
        .bind(trace)
        .bind(dead_at)
        .execute(pool)
        .await
        .unwrap();
        (id, trace)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_jobs_admin_endpoints_are_for_an_owner_session_only(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (_, member) = user(&pool, &router, "member@example.com").await;
        let (_, key) = make_key(&router, &owner, &ws, &["read", "write", "admin"]).await;
        let (dead, _) = seed_job(&pool, "a.kind", "dead", None, None, Some("2030-01-01")).await;
        let retry_uri = format!("/api/v1/admin/jobs/{dead}/retry");
        let endpoints = [
            ("GET", "/api/v1/admin/jobs".to_owned()),
            ("GET", "/api/v1/admin/jobs/dead".to_owned()),
            ("POST", retry_uri),
        ];

        for (method, uri) in &endpoints[..2] {
            let status = |auth| {
                let router = router.clone();
                async move { call(&router, method, uri, None, auth).await.status }
            };
            assert_eq!(status(Auth::Session(&owner)).await, StatusCode::OK, "{uri}");
            assert_eq!(status(Auth::Session(&member)).await, StatusCode::FORBIDDEN);
            assert_eq!(status(Auth::Anonymous).await, StatusCode::UNAUTHORIZED);
            assert_eq!(status(Auth::Key(&key)).await, StatusCode::FORBIDDEN);
        }
        let (method, uri) = &endpoints[2];
        let status = |auth| {
            let router = router.clone();
            async move { call(&router, method, uri, None, auth).await.status }
        };
        assert_eq!(status(Auth::Session(&member)).await, StatusCode::FORBIDDEN);
        assert_eq!(status(Auth::Anonymous).await, StatusCode::UNAUTHORIZED);
        assert_eq!(status(Auth::Key(&key)).await, StatusCode::FORBIDDEN);
        assert_eq!(
            status(Auth::SessionWithoutCsrf(&owner)).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(status(Auth::Session(&owner)).await, StatusCode::OK);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_depth_endpoint_counts_states_per_kind_and_ages_the_oldest_ready_job(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        for _ in 0..2 {
            seed_job(&pool, "b.kind", "ready", None, None, None).await;
        }
        seed_job(&pool, "b.kind", "running", None, None, None).await;
        seed_job(&pool, "a.kind", "dead", None, None, Some("2030-01-01")).await;
        sqlx::query(
            "UPDATE jobs SET run_at = now() - interval '120 seconds' WHERE state = 'ready'",
        )
        .execute(&pool)
        .await
        .unwrap();
        seed_job(&pool, "b.kind", "ready", None, None, None).await;
        sqlx::query("UPDATE jobs SET run_at = now() + interval '1 hour' WHERE id = (SELECT id FROM jobs WHERE state = 'ready' ORDER BY run_at DESC LIMIT 1)")
            .execute(&pool)
            .await
            .unwrap();

        let reply = call(
            &router,
            "GET",
            "/api/v1/admin/jobs",
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(reply.status, StatusCode::OK);
        let kinds = reply.body["kinds"].as_array().unwrap();
        assert_eq!(kinds.len(), 2);
        assert_eq!(kinds[0]["kind"], "a.kind");
        assert_eq!(
            (
                kinds[0]["ready"].as_i64(),
                kinds[0]["running"].as_i64(),
                kinds[0]["dead"].as_i64()
            ),
            (Some(0), Some(0), Some(1))
        );
        assert!(kinds[0]["oldest_ready_seconds"].is_null());
        assert_eq!(kinds[1]["kind"], "b.kind");
        assert_eq!(
            (
                kinds[1]["ready"].as_i64(),
                kinds[1]["running"].as_i64(),
                kinds[1]["dead"].as_i64()
            ),
            (Some(3), Some(1), Some(0))
        );
        let oldest = kinds[1]["oldest_ready_seconds"].as_f64().unwrap();
        assert!((120.0..180.0).contains(&oldest), "{oldest}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_dead_list_is_newest_first_pages_by_cursor_and_never_shows_payloads(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let mut expected = Vec::new();
        for day in 1..=5 {
            let at = format!("2030-01-0{day}");
            let (id, trace) = seed_job(&pool, "a.kind", "dead", None, None, Some(&at)).await;
            expected.push((id.to_string(), trace.simple().to_string()));
        }
        seed_job(&pool, "a.kind", "ready", None, None, None).await;
        expected.reverse();
        let get = |uri: String| {
            let router = router.clone();
            let owner = &owner;
            async move { call(&router, "GET", &uri, None, Auth::Session(owner)).await }
        };

        let all = get("/api/v1/admin/jobs/dead".into()).await;
        assert_eq!(all.status, StatusCode::OK);
        assert!(all.body["next_before"].is_null());
        let jobs = all.body["jobs"].as_array().unwrap();
        let ids: Vec<&str> = jobs.iter().map(|job| job["id"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            expected
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(jobs[0]["trace_id"], expected[0].1.as_str());
        assert_eq!(
            jobs[0]["cause"],
            json!({"kind": "schedule", "id": "sched-1"})
        );
        assert_eq!(jobs[0]["last_error"], "boom");
        assert_eq!(jobs[0]["state"], "dead");
        assert_eq!(
            (
                jobs[0]["attempts"].as_i64(),
                jobs[0]["max_attempts"].as_i64()
            ),
            (Some(5), Some(5))
        );
        assert!(
            jobs[0]["dead_at"]
                .as_str()
                .unwrap()
                .starts_with("2030-01-05T00:00:00")
        );
        assert!(!all.body.to_string().contains("hunter2"));
        assert!(jobs[0].get("payload").is_none());

        let mut seen = Vec::new();
        let mut uri = "/api/v1/admin/jobs/dead?limit=2".to_owned();
        let mut pages = 0;
        loop {
            let page = get(uri.clone()).await;
            assert_eq!(page.status, StatusCode::OK);
            seen.extend(
                page.body["jobs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|job| job["id"].as_str().unwrap().to_owned()),
            );
            pages += 1;
            match page.body["next_before"].as_str() {
                Some(cursor) => uri = format!("/api/v1/admin/jobs/dead?limit=2&before={cursor}"),
                None => break,
            }
        }
        assert_eq!(pages, 3);
        assert_eq!(
            seen,
            expected
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        );

        for bad in ["limit=0", "limit=201", "limit=x", "before=garbage"] {
            let reply = get(format!("/api/v1/admin/jobs/dead?{bad}")).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
        }
        assert_eq!(
            get("/api/v1/admin/jobs/dead?limit=200".into()).await.status,
            StatusCode::OK
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn retrying_a_dead_workspace_job_requeues_it_under_its_trace_and_audits_and_records_it(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner_login = owner(&pool, &router).await;
        let ws = workspace(&router, &owner_login, "Alpha").await;
        let (id, trace) = seed_job(
            &pool,
            "retry.kind",
            "dead",
            Some(&ws),
            None,
            Some("2030-01-01"),
        )
        .await;

        let reply = call(
            &router,
            "POST",
            &format!("/api/v1/admin/jobs/{id}/retry"),
            None,
            Auth::Session(&owner_login),
        )
        .await;

        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        assert_eq!(reply.body["id"], id.to_string());
        assert_eq!(reply.body["state"], "ready");
        assert_eq!(reply.body["attempts"], 0);
        assert!(reply.body["last_error"].is_null());
        assert!(reply.body["dead_at"].is_null());
        assert_eq!(reply.body["trace_id"], trace.simple().to_string());
        assert_eq!(
            reply.body["cause"],
            json!({"kind": "schedule", "id": "sched-1"})
        );
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &["retry.kind"],
            std::time::Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .expect("a worker can claim the retried job");
        assert_eq!(claimed.id, id);
        assert_eq!(claimed.trace.trace_id.as_uuid(), trace);
        let audit = zirv_flow_store::list_audit(&pool, 5).await.unwrap();
        let retried = audit
            .iter()
            .find(|row| row.action == "job.retried")
            .unwrap();
        assert_eq!(retried.object_kind.as_deref(), Some("job"));
        assert_eq!(retried.object_id, Some(id));
        assert_eq!(retried.summary, json!({"kind": "retry.kind"}));
        assert_eq!(retried.actor_kind, "user");
        assert_eq!(
            retried.workspace_id.map(|w| w.to_string()),
            Some(ws.clone())
        );
        let records = zirv_flow_store::list_trace(
            &pool,
            ws.parse().unwrap(),
            zirv_flow_core::TraceId::from_uuid(trace),
            10,
        )
        .await
        .unwrap()
        .0;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].action, "job.retry");
        assert_eq!(records[0].outcome, "requeued");
        assert_eq!(records[0].job_id, Some(id));
        assert_eq!(records[0].cause.kind.to_string(), "manual");
        assert_eq!(
            records[0].cause.id,
            sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM users")
                .fetch_one(&pool)
                .await
                .unwrap()
                .to_string()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn retrying_an_instance_job_is_audited_but_records_no_decision(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let (id, _) = seed_job(&pool, "retry.kind", "dead", None, None, Some("2030-01-01")).await;

        let reply = call(
            &router,
            "POST",
            &format!("/api/v1/admin/jobs/{id}/retry"),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(reply.status, StatusCode::OK);
        let audit = zirv_flow_store::list_audit(&pool, 5).await.unwrap();
        assert!(audit.iter().any(|row| row.action == "job.retried"));
        let decisions: i64 = sqlx::query_scalar("SELECT count(*) FROM decision_records")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(decisions, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn retrying_a_job_that_is_not_dead_or_unknown_is_a_404_and_changes_nothing(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let (ready, _) = seed_job(&pool, "a.kind", "ready", None, None, None).await;
        let (running, _) = seed_job(&pool, "a.kind", "running", None, None, None).await;
        let unknown = uuid::Uuid::new_v4();

        for id in [
            ready.to_string(),
            running.to_string(),
            unknown.to_string(),
            "nope".into(),
        ] {
            let reply = call(
                &router,
                "POST",
                &format!("/api/v1/admin/jobs/{id}/retry"),
                None,
                Auth::Session(&owner),
            )
            .await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{id}");
        }

        let states: Vec<String> = sqlx::query_scalar("SELECT state FROM jobs ORDER BY state")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(states, ["ready", "running"]);
        let audit = zirv_flow_store::list_audit(&pool, 50).await.unwrap();
        assert!(audit.iter().all(|row| row.action != "job.retried"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn retrying_into_a_held_unique_key_is_a_409_and_changes_nothing(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (dead, _) = seed_job(
            &pool,
            "a.kind",
            "dead",
            Some(&ws),
            Some("k"),
            Some("2030-01-01"),
        )
        .await;
        seed_job(&pool, "a.kind", "ready", Some(&ws), Some("k"), None).await;

        let reply = call(
            &router,
            "POST",
            &format!("/api/v1/admin/jobs/{dead}/retry"),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(reply.status, StatusCode::CONFLICT, "{:?}", reply.body);
        assert_eq!(reply.body["detail"], "an equivalent job is already queued");
        let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id = $1")
            .bind(dead)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "dead");
        let audit = zirv_flow_store::list_audit(&pool, 50).await.unwrap();
        assert!(audit.iter().all(|row| row.action != "job.retried"));
        let decisions: i64 = sqlx::query_scalar("SELECT count(*) FROM decision_records")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(decisions, 0);
    }

    fn traits_uri(ws: &str) -> String {
        format!("/api/v1/workspaces/{ws}/traits")
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_field_registry_is_created_listed_updated_and_audited(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;

        let created = call(
            &router,
            "POST",
            &traits_uri(&ws),
            Some(
                json!({"key": "plan", "type": "string", "precedence": "source_priority",
                        "source_order": ["crm", "shop"], "pii": true,
                        "description": "secret plan text"}),
            ),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        assert_eq!(created.body["type"], "string");
        assert_eq!(created.body["precedence"], "source_priority");
        assert_eq!(created.body["source_order"], json!(["crm", "shop"]));
        assert_eq!(created.body["inferred"], false);
        let duplicate = call(
            &router,
            "POST",
            &traits_uri(&ws),
            Some(json!({"key": "plan", "type": "number"})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(duplicate.status, StatusCode::CONFLICT);
        for bad in [
            json!({"key": "Plan", "type": "string"}),
            json!({"key": "ok", "type": "integer"}),
            json!({"key": "ok", "type": "string", "precedence": "newest"}),
        ] {
            let reply = call(
                &router,
                "POST",
                &traits_uri(&ws),
                Some(bad),
                Auth::Session(&owner),
            )
            .await;
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{:?}",
                reply.body
            );
        }

        let patched = call(
            &router,
            "PATCH",
            &format!("{}/plan", traits_uri(&ws)),
            Some(json!({"archived": true, "description": "retired"})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);
        assert_eq!(patched.body["archived"], true);
        assert_eq!(patched.body["precedence"], "source_priority");
        let listed = call(
            &router,
            "GET",
            &traits_uri(&ws),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(listed.status, StatusCode::OK);
        assert_eq!(listed.body.as_array().unwrap().len(), 1);
        assert_eq!(listed.body[0]["archived"], true);

        let audit = zirv_flow_store::list_audit(&pool, 50).await.unwrap();
        let created_row = audit
            .iter()
            .find(|row| row.action == "trait.created")
            .unwrap();
        assert_eq!(created_row.summary, json!({"key": "plan"}));
        let updated_row = audit
            .iter()
            .find(|row| row.action == "trait.updated")
            .unwrap();
        assert_eq!(
            updated_row.summary,
            json!({"key": "plan", "fields": ["description", "archived"]})
        );
        let text = serde_json::to_string(&audit.iter().map(|row| &row.summary).collect::<Vec<_>>())
            .unwrap();
        assert!(!text.contains("retired") && !text.contains("secret plan text"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_type_of_a_trait_cannot_be_changed_and_an_empty_patch_is_refused(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        call(
            &router,
            "POST",
            &traits_uri(&ws),
            Some(json!({"key": "age", "type": "number"})),
            Auth::Session(&owner),
        )
        .await;
        let uri = format!("{}/age", traits_uri(&ws));

        let retyped = call(
            &router,
            "PATCH",
            &uri,
            Some(json!({"type": "string"})),
            Auth::Session(&owner),
        )
        .await;
        let empty = call(
            &router,
            "PATCH",
            &uri,
            Some(json!({})),
            Auth::Session(&owner),
        )
        .await;
        let missing = call(
            &router,
            "PATCH",
            &format!("{}/nope", traits_uri(&ws)),
            Some(json!({"pii": true})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(retyped.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(retyped.body["detail"], "type: cannot be changed");
        assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        let listed = call(
            &router,
            "GET",
            &traits_uri(&ws),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(listed.body[0]["type"], "number");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_member_who_is_not_an_admin_reads_but_cannot_change_the_registry(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        call(
            &router,
            "POST",
            &traits_uri(&ws),
            Some(json!({"key": "age", "type": "number"})),
            Auth::Session(&owner),
        )
        .await;

        let read = call(
            &router,
            "GET",
            &traits_uri(&ws),
            None,
            Auth::Session(&member),
        )
        .await;
        let create = call(
            &router,
            "POST",
            &traits_uri(&ws),
            Some(json!({"key": "city", "type": "string"})),
            Auth::Session(&member),
        )
        .await;
        let patch = call(
            &router,
            "PATCH",
            &format!("{}/age", traits_uri(&ws)),
            Some(json!({"pii": true})),
            Auth::Session(&member),
        )
        .await;
        let no_csrf = call(
            &router,
            "POST",
            &traits_uri(&ws),
            Some(json!({"key": "city", "type": "string"})),
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;

        assert_eq!(read.status, StatusCode::OK);
        assert_eq!(read.body.as_array().unwrap().len(), 1);
        assert_eq!(create.status, StatusCode::FORBIDDEN);
        assert_eq!(patch.status, StatusCode::FORBIDDEN);
        assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_registry_of_another_workspace_is_a_404(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let mine = workspace(&router, &owner, "Mine").await;
        let theirs = workspace(&router, &owner, "Theirs").await;
        let (admin_id, admin) = user(&pool, &router, "admin@example.com").await;
        add_member(&router, &owner, &mine, admin_id, "admin").await;
        let (_, key) = make_key(&router, &owner, &mine, &["admin"]).await;
        call(
            &router,
            "POST",
            &traits_uri(&theirs),
            Some(json!({"key": "age", "type": "number"})),
            Auth::Session(&owner),
        )
        .await;

        let list = call(
            &router,
            "GET",
            &traits_uri(&theirs),
            None,
            Auth::Session(&admin),
        )
        .await;
        let create = call(
            &router,
            "POST",
            &traits_uri(&theirs),
            Some(json!({"key": "city", "type": "string"})),
            Auth::Session(&admin),
        )
        .await;
        let by_key = call(&router, "GET", &traits_uri(&theirs), None, Auth::Key(&key)).await;
        let own = call(
            &router,
            "GET",
            &traits_uri(&mine),
            None,
            Auth::Session(&admin),
        )
        .await;

        assert_eq!(list.status, StatusCode::NOT_FOUND);
        assert_eq!(create.status, StatusCode::NOT_FOUND);
        assert_eq!(by_key.status, StatusCode::NOT_FOUND);
        assert_eq!(own.status, StatusCode::OK);
        assert_eq!(own.body, json!([]));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_null_type_is_refused_like_any_other_type(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        call(
            &router,
            "POST",
            &traits_uri(&ws),
            Some(json!({"key": "age", "type": "number"})),
            Auth::Session(&owner),
        )
        .await;

        let reply = call(
            &router,
            "PATCH",
            &format!("{}/age", traits_uri(&ws)),
            Some(json!({"type": null, "pii": true})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(reply.body["detail"], "type: cannot be changed");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn changing_a_rule_enqueues_a_recompute_in_the_same_transaction(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        call(
            &router,
            "POST",
            &traits_uri(&ws),
            Some(json!({"key": "age", "type": "number"})),
            Auth::Session(&owner),
        )
        .await;
        let uri = format!("{}/age", traits_uri(&ws));
        let queued = |pool: PgPool| async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM jobs WHERE kind = 'traits.recompute'",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        };

        call(
            &router,
            "PATCH",
            &uri,
            Some(json!({"description": "x", "pii": true})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(queued(pool.clone()).await, 0);
        let archived = call(
            &router,
            "PATCH",
            &uri,
            Some(json!({"archived": true})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(archived.status, StatusCode::OK);
        let rule = call(
            &router,
            "PATCH",
            &uri,
            Some(json!({"precedence": "first"})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(rule.status, StatusCode::OK);

        assert_eq!(queued(pool.clone()).await, 2);
        let payload: Value =
            sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = 'traits.recompute' LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(payload, json!({"workspace_id": ws, "key": "age"}));
        let refused = call(
            &router,
            "PATCH",
            &uri,
            Some(json!({"precedence": "newest"})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(queued(pool).await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn re_sending_the_current_rule_enqueues_no_recompute(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        call(
            &router,
            "POST",
            &traits_uri(&ws),
            Some(json!({"key": "age", "type": "number", "source_order": ["crm"]})),
            Auth::Session(&owner),
        )
        .await;
        let uri = format!("{}/age", traits_uri(&ws));

        let reply = call(
            &router,
            "PATCH",
            &uri,
            Some(json!({"precedence": "latest", "source_order": ["crm"], "archived": false})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(reply.status, StatusCode::OK);
        let queued: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'traits.recompute'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(queued, 0);
    }

    fn conflicts_uri(ws: &str) -> String {
        format!("/api/v1/workspaces/{ws}/identity-conflicts")
    }

    fn profile(key: &str, identifiers: &[(&str, &str)]) -> zirv_flow_store::ProfileRecord {
        zirv_flow_store::ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: key.into(),
            identifiers: identifiers
                .iter()
                .map(|(kind, value)| ((*kind).into(), (*value).into()))
                .collect(),
            traits: serde_json::Map::new(),
            observed_at: time::macros::datetime!(2026-01-01 00:00 UTC),
            trace: zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("seed")),
        }
    }

    /// P1 holds `{external:crm 1, email x}` and P2 `{external:crm 2}`; a record with
    /// `{external:crm 2, email x}` records a `unique_kind` conflict. Returns the person the
    /// record attached to (P2), the other person (P1) and the conflict id.
    async fn seed_conflict(pool: &PgPool, ws: &str) -> (String, String, String) {
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let outcomes = zirv_flow_store::apply_profile_batch(
            pool,
            workspace,
            &[
                profile("1", &[("external:crm", "1"), ("email", "x@example.com")]),
                profile("2", &[("external:crm", "2")]),
                profile("3", &[("external:crm", "2"), ("email", "x@example.com")]),
            ],
        )
        .await
        .unwrap();
        let person = |outcome: &zirv_flow_store::ProfileOutcome| match outcome {
            zirv_flow_store::ProfileOutcome::Applied { person_id, .. } => person_id.to_string(),
            other => panic!("not applied: {other:?}"),
        };
        let (first, second) = (person(&outcomes[0]), person(&outcomes[1]));
        let id: uuid::Uuid = sqlx::query_scalar(
            "SELECT id FROM identity_conflicts WHERE workspace_id = $1 AND resolved_at IS NULL",
        )
        .bind(workspace.as_uuid())
        .fetch_one(pool)
        .await
        .unwrap();
        (second, first, id.to_string())
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn conflicts_are_listed_by_status_and_shown_with_both_peoples_identities(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (holder, other, id) = seed_conflict(&pool, &ws).await;

        let open = call(
            &router,
            "GET",
            &conflicts_uri(&ws),
            None,
            Auth::Session(&owner),
        )
        .await;
        let resolved = call(
            &router,
            "GET",
            &format!("{}?status=resolved", conflicts_uri(&ws)),
            None,
            Auth::Session(&owner),
        )
        .await;
        let bad = call(
            &router,
            "GET",
            &format!("{}?status=all", conflicts_uri(&ws)),
            None,
            Auth::Session(&owner),
        )
        .await;
        let detail = call(
            &router,
            "GET",
            &format!("{}/{id}", conflicts_uri(&ws)),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(open.status, StatusCode::OK, "{:?}", open.body);
        let listed = &open.body["conflicts"][0];
        assert_eq!(listed["id"], id);
        assert_eq!(listed["kind"], "email");
        assert_eq!(listed["reason"], "unique_kind");
        assert_eq!(listed["person_id"], holder);
        assert_eq!(listed["other_person_id"], other);
        assert!(listed["resolved_at"].is_null() && listed["resolution"].is_null());
        assert!(open.body["next_before"].is_null());
        assert_eq!(resolved.body["conflicts"], json!([]));
        assert_eq!(bad.status, StatusCode::BAD_REQUEST);
        assert_eq!(detail.status, StatusCode::OK, "{:?}", detail.body);
        let people = detail.body["people"].as_array().unwrap();
        assert_eq!(people[0]["person_id"], holder);
        assert_eq!(people[1]["person_id"], other);
        assert_eq!(
            people[0]["identities"],
            json!([{"kind": "external:crm", "value": "2", "masked": false, "shared": false}])
        );
        assert_eq!(
            people[1]["identities"],
            json!([
                {"kind": "email", "value": "x@example.com", "masked": false, "shared": true},
                {"kind": "external:crm", "value": "1", "masked": false, "shared": false}
            ])
        );
        let missing = call(
            &router,
            "GET",
            &format!("{}/{}", conflicts_uri(&ws), uuid::Uuid::now_v7()),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_list_pages_by_cursor(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (_, _, id) = seed_conflict(&pool, &ws).await;
        for _ in 0..2 {
            // Another open conflict about the same identifier, from another person.
            let extra = uuid::Uuid::now_v7();
            sqlx::query(
                "INSERT INTO people (id, workspace_id) \
                 SELECT $1, workspace_id FROM identity_conflicts WHERE id = $2",
            )
            .bind(extra)
            .bind(uuid::Uuid::parse_str(&id).unwrap())
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO identity_conflicts \
                    (id, workspace_id, person_id, other_person_id, kind, source_record_id, reason) \
                 SELECT $1, workspace_id, $3, other_person_id, kind, source_record_id, \
                        reason FROM identity_conflicts WHERE id = $2",
            )
            .bind(uuid::Uuid::now_v7())
            .bind(uuid::Uuid::parse_str(&id).unwrap())
            .bind(extra)
            .execute(&pool)
            .await
            .unwrap();
        }

        let first = call(
            &router,
            "GET",
            &format!("{}?limit=2", conflicts_uri(&ws)),
            None,
            Auth::Session(&owner),
        )
        .await;
        let cursor = first.body["next_before"].as_str().unwrap().to_owned();
        let second = call(
            &router,
            "GET",
            &format!("{}?limit=2&before={cursor}", conflicts_uri(&ws)),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(first.body["conflicts"].as_array().unwrap().len(), 2);
        assert_eq!(second.body["conflicts"].as_array().unwrap().len(), 1);
        assert!(second.body["next_before"].is_null());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn members_and_read_keys_see_masked_values_and_admin_keys_see_them(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (_, _, id) = seed_conflict(&pool, &ws).await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let (_, admin_key) = make_key(&router, &owner, &ws, &["admin"]).await;
        let uri = format!("{}/{id}", conflicts_uri(&ws));

        let as_member = call(&router, "GET", &uri, None, Auth::Session(&member)).await;
        let as_read_key = call(&router, "GET", &uri, None, Auth::Key(&read_key)).await;
        let as_admin_key = call(&router, "GET", &uri, None, Auth::Key(&admin_key)).await;

        for masked in [&as_member, &as_read_key] {
            assert_eq!(masked.status, StatusCode::OK);
            let text = masked.body.to_string();
            assert!(!text.contains("x@example.com") && !text.contains("\"value\":\""));
            for person in masked.body["people"].as_array().unwrap() {
                for identity in person["identities"].as_array().unwrap() {
                    assert!(identity["value"].is_null());
                    assert_eq!(identity["masked"], true);
                }
            }
        }
        assert_eq!(as_admin_key.status, StatusCode::OK);
        assert_eq!(
            as_admin_key.body["people"][1]["identities"][0]["value"],
            "x@example.com"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn keeping_two_people_separate_closes_the_conflict_audits_and_records_a_decision(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (holder, other, id) = seed_conflict(&pool, &ws).await;
        let uri = format!("{}/{id}/resolve", conflicts_uri(&ws));

        let closed = call(
            &router,
            "POST",
            &uri,
            Some(json!({"outcome": "keep_separate"})),
            Auth::Session(&owner),
        )
        .await;
        let again = call(
            &router,
            "POST",
            &uri,
            Some(json!({"outcome": "keep_separate"})),
            Auth::Session(&owner),
        )
        .await;
        let resolved = call(
            &router,
            "GET",
            &format!("{}?status=resolved", conflicts_uri(&ws)),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(closed.status, StatusCode::OK, "{:?}", closed.body);
        assert_eq!(closed.body["resolution"], "kept_separate");
        assert!(closed.body["resolved_at"].is_string());
        assert_eq!(again.status, StatusCode::CONFLICT);
        assert_eq!(resolved.body["conflicts"].as_array().unwrap().len(), 1);
        let audit = zirv_flow_store::list_audit(&pool, 50).await.unwrap();
        let entry = audit
            .iter()
            .find(|row| row.action == "identity_conflict.kept_separate")
            .expect("an audit entry");
        assert_eq!(entry.object_kind.as_deref(), Some("identity_conflict"));
        assert_eq!(entry.object_id.map(|id| id.to_string()), Some(id.clone()));
        assert_eq!(
            entry.summary,
            json!({"person_id": holder, "other_person_id": other})
        );
        let decision: (String, String) = sqlx::query_as(
            "SELECT action, outcome FROM decision_records WHERE action = 'identity.conflict.resolved'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            decision,
            ("identity.conflict.resolved".into(), "kept_separate".into())
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn moving_the_identifier_gives_it_to_the_chosen_person_and_audits_ids_only(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (holder, other, id) = seed_conflict(&pool, &ws).await;
        let uri = format!("{}/{id}/resolve", conflicts_uri(&ws));

        let moved = call(
            &router,
            "POST",
            &uri,
            Some(json!({"outcome": "move_identifier", "to_person_id": holder})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);
        assert_eq!(moved.body["resolution"], "identifier_moved");
        let owner_of_email: String =
            sqlx::query_scalar("SELECT person_id::text FROM identities WHERE kind = 'email'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(owner_of_email, holder);
        let audit = zirv_flow_store::list_audit(&pool, 50).await.unwrap();
        let entry = audit
            .iter()
            .find(|row| row.action == "identity_conflict.identifier_moved")
            .expect("an audit entry");
        assert_eq!(
            entry.summary,
            json!({"person_id": holder, "other_person_id": other, "to_person_id": holder})
        );
        assert!(!entry.summary.to_string().contains("x@example.com"));
        let decision: String = sqlx::query_scalar(
            "SELECT outcome FROM decision_records WHERE action = 'identity.conflict.resolved'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(decision, "identifier_moved");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_move_that_breaks_a_rule_is_409_a_stranger_is_422_and_nothing_changes(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (holder, other, id) = seed_conflict(&pool, &ws).await;
        let workspace_id: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let mut fill = profile("fill", &[("external:crm", "2")]);
        fill.identifiers
            .extend((0..10).map(|n| ("email".to_owned(), format!("e{n}@example.com"))));
        zirv_flow_store::apply_profile_batch(&pool, workspace_id, &[fill])
            .await
            .unwrap();
        let uri = format!("{}/{id}/resolve", conflicts_uri(&ws));

        let over = call(
            &router,
            "POST",
            &uri,
            Some(json!({"outcome": "move_identifier", "to_person_id": holder})),
            Auth::Session(&owner),
        )
        .await;
        let stranger = call(
            &router,
            "POST",
            &uri,
            Some(json!({"outcome": "move_identifier", "to_person_id": uuid::Uuid::now_v7()})),
            Auth::Session(&owner),
        )
        .await;
        let already = call(
            &router,
            "POST",
            &uri,
            Some(json!({"outcome": "move_identifier", "to_person_id": other})),
            Auth::Session(&owner),
        )
        .await;
        let malformed = call(
            &router,
            "POST",
            &uri,
            Some(json!({"outcome": "move_identifier"})),
            Auth::Session(&owner),
        )
        .await;
        let unknown = call(
            &router,
            "POST",
            &format!("{}/{}/resolve", conflicts_uri(&ws), uuid::Uuid::now_v7()),
            Some(json!({"outcome": "keep_separate"})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(over.status, StatusCode::CONFLICT, "{:?}", over.body);
        assert_eq!(stranger.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(already.status, StatusCode::CONFLICT);
        assert_eq!(malformed.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        let open: i64 =
            sqlx::query_scalar("SELECT count(*) FROM identity_conflicts WHERE resolved_at IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(open, 1);
        let audit = zirv_flow_store::list_audit(&pool, 50).await.unwrap();
        assert!(
            audit
                .iter()
                .all(|row| !row.action.starts_with("identity_conflict."))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_admins_resolve_and_another_workspace_sees_nothing(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let mine = workspace(&router, &owner, "Alpha").await;
        let theirs = workspace(&router, &owner, "Beta").await;
        let (_, _, id) = seed_conflict(&pool, &mine).await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &mine, member_id, "member").await;
        let (_, read_key) = make_key(&router, &owner, &mine, &["read"]).await;
        let (_, theirs_key) = make_key(&router, &owner, &theirs, &["admin"]).await;
        let uri = format!("{}/{id}/resolve", conflicts_uri(&mine));
        let body = || Some(json!({"outcome": "keep_separate"}));

        let as_member = call(&router, "POST", &uri, body(), Auth::Session(&member)).await;
        let as_read_key = call(&router, "POST", &uri, body(), Auth::Key(&read_key)).await;
        let no_csrf = call(
            &router,
            "POST",
            &uri,
            body(),
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;
        let anonymous = call(&router, "POST", &uri, body(), Auth::Anonymous).await;
        let foreign_key = call(&router, "POST", &uri, body(), Auth::Key(&theirs_key)).await;
        let foreign_workspace = call(
            &router,
            "GET",
            &format!("{}/{id}", conflicts_uri(&theirs)),
            None,
            Auth::Session(&owner),
        )
        .await;
        let foreign_list = call(
            &router,
            "GET",
            &conflicts_uri(&theirs),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(as_member.status, StatusCode::FORBIDDEN);
        assert_eq!(as_read_key.status, StatusCode::FORBIDDEN);
        assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(foreign_key.status, StatusCode::NOT_FOUND);
        assert_eq!(foreign_workspace.status, StatusCode::NOT_FOUND);
        assert_eq!(foreign_list.body["conflicts"], json!([]));
        let open: i64 =
            sqlx::query_scalar("SELECT count(*) FROM identity_conflicts WHERE resolved_at IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(open, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_move_after_a_person_was_merged_away_is_409_conflict_changed(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (holder, other, id) = seed_conflict(&pool, &ws).await;
        sqlx::query("UPDATE people SET merged_into = $1 WHERE id = $2")
            .bind(uuid::Uuid::parse_str(&holder).unwrap())
            .bind(uuid::Uuid::parse_str(&other).unwrap())
            .execute(&pool)
            .await
            .unwrap();

        let reply = call(
            &router,
            "POST",
            &format!("{}/{id}/resolve", conflicts_uri(&ws)),
            Some(json!({"outcome": "move_identifier", "to_person_id": other})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(reply.status, StatusCode::CONFLICT, "{:?}", reply.body);
        assert!(
            reply.body["detail"]
                .as_str()
                .unwrap()
                .starts_with("conflict_changed")
        );
    }

    fn schemas_uri(ws: &str) -> String {
        format!("/api/v1/workspaces/{ws}/event-schemas")
    }

    fn total_property() -> Value {
        json!({"name": "total", "type": "number", "required": true})
    }

    async fn make_schema(router: &Router, owner: &Login, ws: &str, name: &str) {
        let reply = call(
            router,
            "POST",
            &schemas_uri(ws),
            Some(json!({"name": name, "mode": "block", "properties": [total_property()]})),
            Auth::Session(owner),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_schema_is_created_drafted_published_and_audited(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;

        let created = call(
            &router,
            "POST",
            &schemas_uri(&ws),
            Some(
                json!({"name": "Order Completed", "mode": "block", "description": "secret words",
                        "properties": [total_property()]}),
            ),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        assert_eq!(created.body["schema"]["mode"], "block");
        assert_eq!(created.body["schema"]["has_draft"], true);
        assert_eq!(created.body["schema"]["current_version"], Value::Null);
        assert_eq!(created.body["draft"][0]["name"], "total");
        let uri = format!("{}/Order%20Completed", schemas_uri(&ws));

        let duplicate = call(
            &router,
            "POST",
            &schemas_uri(&ws),
            Some(json!({"name": "Order Completed"})),
            Auth::Session(&owner),
        )
        .await;
        let saved = call(
            &router,
            "PUT",
            &format!("{uri}/draft"),
            Some(json!({"properties": [total_property(),
                {"name": "tier", "type": "string", "enum": ["free", "pro"], "pii": true}]})),
            Auth::Session(&owner),
        )
        .await;
        let first = call(
            &router,
            "POST",
            &format!("{uri}/versions"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let again = call(
            &router,
            "POST",
            &format!("{uri}/versions"),
            None,
            Auth::Session(&owner),
        )
        .await;
        call(
            &router,
            "PUT",
            &format!("{uri}/draft"),
            Some(json!({"properties": []})),
            Auth::Session(&owner),
        )
        .await;
        let second = call(
            &router,
            "POST",
            &format!("{uri}/versions"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let fetched = call(&router, "GET", &uri, None, Auth::Session(&owner)).await;

        assert_eq!(duplicate.status, StatusCode::CONFLICT);
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert_eq!(saved.body["draft"][1]["enum"], json!(["free", "pro"]));
        assert_eq!(first.status, StatusCode::CREATED, "{:?}", first.body);
        assert_eq!(first.body["schema"]["current_version"], 1);
        assert_eq!(first.body["draft"], Value::Null);
        assert_eq!(
            first.body["versions"][0]["properties"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(again.status, StatusCode::CONFLICT);
        assert_eq!(second.body["schema"]["current_version"], 2);
        assert_eq!(fetched.body["versions"].as_array().unwrap().len(), 2);
        assert_eq!(fetched.body["versions"][0]["version"], 2);
        assert_eq!(fetched.body["versions"][0]["properties"], json!([]));
        assert_eq!(
            fetched.body["versions"][1]["properties"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        let audit = zirv_flow_store::list_audit(&pool, 50).await.unwrap();
        for action in ["created", "draft_saved", "published"] {
            let name = format!("event_schema.{action}");
            assert!(audit.iter().any(|row| row.action == name), "{name}");
        }
        let published = audit
            .iter()
            .find(|row| row.action == "event_schema.published")
            .unwrap();
        assert_eq!(published.summary["name"], "Order Completed");
        let text = serde_json::to_string(&audit.iter().map(|row| &row.summary).collect::<Vec<_>>())
            .unwrap();
        assert!(!text.contains("secret words") && !text.contains("free"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn invalid_schemas_and_missing_ones_are_refused(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        for bad in [
            json!({"name": ""}),
            json!({"name": "bad\nname"}),
            json!({"name": "."}),
            json!({"name": ".."}),
            json!({"name": "ok", "mode": "strict"}),
            json!({"name": "ok", "properties": [{"name": "a", "type": "money"}]}),
            json!({"name": "ok", "properties": [{"name": "a", "type": "number", "enum": ["x"]}]}),
            json!({"name": "ok", "properties": [{"name": "a", "type": "string"}, {"name": "a", "type": "string"}]}),
        ] {
            let reply = call(
                &router,
                "POST",
                &schemas_uri(&ws),
                Some(bad.clone()),
                Auth::Session(&owner),
            )
            .await;
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{bad}: {:?}",
                reply.body
            );
        }
        let missing = format!("{}/nothing", schemas_uri(&ws));

        let get = call(&router, "GET", &missing, None, Auth::Session(&owner)).await;
        let draft = call(
            &router,
            "PUT",
            &format!("{missing}/draft"),
            Some(json!({"properties": []})),
            Auth::Session(&owner),
        )
        .await;
        let publish = call(
            &router,
            "POST",
            &format!("{missing}/versions"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let patch = call(
            &router,
            "PATCH",
            &missing,
            Some(json!({"mode": "allow"})),
            Auth::Session(&owner),
        )
        .await;

        for reply in [get, draft, publish, patch] {
            assert_eq!(reply.status, StatusCode::NOT_FOUND);
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn mode_and_retention_are_updated_within_the_operators_bounds(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        make_schema(&router, &owner, &ws, "order").await;
        let uri = format!("{}/order", schemas_uri(&ws));
        let patch = |body: Value| call(&router, "PATCH", &uri, Some(body), Auth::Session(&owner));

        let set = patch(json!({"mode": "allow", "retention_days": 90})).await;
        let too_short = patch(json!({"retention_days": 29})).await;
        let too_long = patch(json!({"retention_days": 1096})).await;
        let bad_mode = patch(json!({"mode": "strict"})).await;
        let empty = patch(json!({})).await;
        let cleared = patch(json!({"retention_days": null})).await;

        assert_eq!(set.status, StatusCode::OK, "{:?}", set.body);
        assert_eq!(set.body["schema"]["mode"], "allow");
        assert_eq!(set.body["schema"]["retention_days"], 90);
        for refused in [too_short, too_long, bad_mode, empty] {
            assert_eq!(
                refused.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{:?}",
                refused.body
            );
        }
        assert_eq!(cleared.body["schema"]["retention_days"], Value::Null);
        assert_eq!(cleared.body["schema"]["mode"], "allow");
        let audit = zirv_flow_store::list_audit(&pool, 50).await.unwrap();
        let updated = audit
            .iter()
            .filter(|row| row.action == "event_schema.updated")
            .map(|row| row.summary.clone())
            .collect::<Vec<_>>();
        assert!(updated.contains(&json!({"name": "order", "fields": ["retention_days"]})));
        assert!(updated.contains(&json!({"name": "order", "fields": ["mode", "retention_days"]})));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn non_admins_read_events_but_cannot_change_them_and_other_workspaces_are_404(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let mine = workspace(&router, &owner, "Mine").await;
        let theirs = workspace(&router, &owner, "Theirs").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &mine, member_id, "member").await;
        let (_, read_key) = make_key(&router, &owner, &mine, &["read"]).await;
        make_schema(&router, &owner, &mine, "order").await;
        make_schema(&router, &owner, &theirs, "other").await;
        let policy = format!("/api/v1/workspaces/{mine}/event-policy");
        let rejects = format!("/api/v1/workspaces/{mine}/ingest-rejects");
        let uri = format!("{}/order", schemas_uri(&mine));

        for path in [schemas_uri(&mine), uri.clone(), policy.clone(), rejects] {
            let by_member = call(&router, "GET", &path, None, Auth::Session(&member)).await;
            let by_key = call(&router, "GET", &path, None, Auth::Key(&read_key)).await;
            assert_eq!(by_member.status, StatusCode::OK, "{path}");
            assert_eq!(by_key.status, StatusCode::OK, "{path}");
        }
        let policy_body = json!({"unplanned": "draft", "future_skew_seconds": 1,
                                 "lateness_seconds": 1, "redaction": {}});
        let writes = [
            ("POST", schemas_uri(&mine), Some(json!({"name": "x"}))),
            (
                "PUT",
                format!("{uri}/draft"),
                Some(json!({"properties": []})),
            ),
            ("POST", format!("{uri}/versions"), None),
            ("PATCH", uri.clone(), Some(json!({"mode": "allow"}))),
            ("PUT", policy, Some(policy_body)),
        ];
        for (method, path, body) in writes {
            let by_member =
                call(&router, method, &path, body.clone(), Auth::Session(&member)).await;
            let by_key = call(&router, method, &path, body, Auth::Key(&read_key)).await;
            assert_eq!(by_member.status, StatusCode::FORBIDDEN, "{method} {path}");
            assert_eq!(by_key.status, StatusCode::FORBIDDEN, "{method} {path}");
        }
        let foreign = format!("{}/other", schemas_uri(&theirs));
        for path in [schemas_uri(&theirs), foreign] {
            let by_member = call(&router, "GET", &path, None, Auth::Session(&member)).await;
            let by_key = call(&router, "GET", &path, None, Auth::Key(&read_key)).await;
            assert_eq!(by_member.status, StatusCode::NOT_FOUND);
            assert_eq!(by_key.status, StatusCode::NOT_FOUND);
        }
        let no_csrf = call(
            &router,
            "POST",
            &schemas_uri(&mine),
            Some(json!({"name": "x"})),
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;
        assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_policy_defaults_is_replaced_whole_validated_and_audited(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let uri = format!("/api/v1/workspaces/{ws}/event-policy");

        let defaults = call(&router, "GET", &uri, None, Auth::Session(&owner)).await;
        let saved = call(
            &router,
            "PUT",
            &uri,
            Some(
                json!({"unplanned": "reject", "future_skew_seconds": 60, "lateness_seconds": 7200,
                        "redaction": {"drop": ["card"], "hash": ["email"], "truncate_ip": ["ip"]}}),
            ),
            Auth::Session(&owner),
        )
        .await;
        let again = call(&router, "GET", &uri, None, Auth::Session(&owner)).await;
        let mut refused = Vec::new();
        for bad in [
            json!({"unplanned": "ignore", "future_skew_seconds": 1, "lateness_seconds": 1, "redaction": {}}),
            json!({"unplanned": "draft", "future_skew_seconds": -1, "lateness_seconds": 1, "redaction": {}}),
            json!({"unplanned": "draft", "future_skew_seconds": 90000, "lateness_seconds": 1, "redaction": {}}),
            json!({"unplanned": "draft", "future_skew_seconds": 1, "lateness_seconds": 99999999, "redaction": {}}),
            json!({"unplanned": "draft", "future_skew_seconds": 1, "lateness_seconds": 1, "redaction": {"drop": [""]}}),
        ] {
            let reply = call(&router, "PUT", &uri, Some(bad), Auth::Session(&owner)).await;
            refused.push(reply.status);
        }

        assert_eq!(defaults.body["unplanned"], "draft");
        assert_eq!(defaults.body["future_skew_seconds"], 300);
        assert_eq!(defaults.body["lateness_seconds"], 86_400);
        assert_eq!(
            defaults.body["redaction"]["truncate_ip"],
            json!(["ip", "context.ip"])
        );
        assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
        assert_eq!(again.body, saved.body);
        assert_eq!(again.body["redaction"]["hash"], json!(["email"]));
        assert!(
            refused
                .iter()
                .all(|status| *status == StatusCode::UNPROCESSABLE_ENTITY),
            "{refused:?}"
        );
        let audit = zirv_flow_store::list_audit(&pool, 50).await.unwrap();
        let row = audit
            .iter()
            .find(|row| row.action == "event_policy.updated")
            .unwrap();
        assert_eq!(
            row.summary,
            json!({"fields": ["unplanned", "future_skew_seconds", "lateness_seconds", "redaction"]})
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn rejects_list_newest_first_in_pages_with_filters_and_never_the_payload(pool: PgPool) {
        use zirv_flow_core::{Cause, HmacPurpose, HmacRoot, TraceContext};
        use zirv_flow_store::{EventRecord, apply_event_batch};

        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        make_schema(&router, &owner, &ws, "order").await;
        call(
            &router,
            "POST",
            &format!("{}/order/versions", schemas_uri(&ws)),
            None,
            Auth::Session(&owner),
        )
        .await;
        let workspace_id = ws.parse().unwrap();
        let key = HmacRoot::bootstrap_from(&zirv_flow_core::MasterKey::generate())
            .hmac_key(HmacPurpose::Redaction);
        let received = time::macros::datetime!(2026-06-01 12:00 UTC);
        let events: Vec<EventRecord> = [
            ("order", "order-total-1", 1),
            ("order", "order-total-2", 2),
            ("unknown", "unknown-total", 3),
        ]
        .into_iter()
        .map(|(name, value, minute)| EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), "a@example.com".into())],
            name: name.into(),
            properties: json!({"total": value, "card": "card-secret"})
                .as_object()
                .cloned()
                .unwrap(),
            timestamp: None,
            received_at: received + time::Duration::minutes(minute),
            message_id: None,
            trace: TraceContext::root(Cause::api("t")),
        })
        .collect();
        let policy = format!("/api/v1/workspaces/{ws}/event-policy");
        call(
            &router,
            "PUT",
            &policy,
            Some(json!({"unplanned": "reject", "future_skew_seconds": 300,
                        "lateness_seconds": 86400, "redaction": {}})),
            Auth::Session(&owner),
        )
        .await;
        apply_event_batch(&pool, workspace_id, &key, &events)
            .await
            .unwrap();
        let uri = format!("/api/v1/workspaces/{ws}/ingest-rejects");

        let first = call(
            &router,
            "GET",
            &format!("{uri}?limit=2"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let cursor = first.body["next_before"].as_str().unwrap().to_owned();
        let second = call(
            &router,
            "GET",
            &format!("{uri}?limit=2&before={cursor}"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let named = call(
            &router,
            "GET",
            &format!("{uri}?name=order"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let unplanned = call(
            &router,
            "GET",
            &format!("{uri}?reason=unplanned_event"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let bad_limit = call(
            &router,
            "GET",
            &format!("{uri}?limit=0"),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(first.status, StatusCode::OK, "{:?}", first.body);
        let rejects = first.body["rejects"].as_array().unwrap();
        assert_eq!(rejects.len(), 2);
        assert_eq!(rejects[0]["name"], "unknown");
        assert_eq!(rejects[0]["reason"], "unplanned_event");
        assert_eq!(
            rejects[1]["reasons"],
            json!([{"property": "total", "reason": "wrong_type"},
                   {"property": "card", "reason": "unplanned_property"}])
        );
        assert_eq!(second.body["rejects"].as_array().unwrap().len(), 1);
        assert_eq!(second.body["next_before"], Value::Null);
        assert_eq!(named.body["rejects"].as_array().unwrap().len(), 2);
        assert_eq!(unplanned.body["rejects"].as_array().unwrap().len(), 1);
        assert_eq!(bad_limit.status, StatusCode::BAD_REQUEST);
        for reply in [&first, &second, &named, &unplanned] {
            let text = reply.body.to_string();
            assert!(!text.contains("card-secret"), "{text}");
            assert!(!text.contains("unknown-total"), "{text}");
            assert!(!text.contains("payload"), "{text}");
        }
        assert_eq!(rejects[1]["sample"]["properties"]["total"], Value::Null);
    }

    fn lists_uri(ws: &str) -> String {
        format!("/api/v1/workspaces/{ws}/lists")
    }

    /// A raw-body call, for the CSV import.
    async fn call_raw(
        router: &Router,
        uri: &str,
        content_type: Option<&str>,
        body: Vec<u8>,
        auth: Auth<'_>,
    ) -> Reply {
        let mut request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("x-request-id", "test-request");
        match auth {
            Auth::Session(login) => {
                request = request
                    .header(header::COOKIE, &login.cookie)
                    .header("x-csrf-token", &login.csrf);
            }
            Auth::Key(key) => {
                request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
            }
            _ => panic!("not needed by these tests"),
        }
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
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

    async fn make_list(router: &Router, who: Auth<'_>, ws: &str, name: &str) -> String {
        let reply = call(
            router,
            "POST",
            &lists_uri(ws),
            Some(json!({"name": name})),
            who,
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
        reply.body["id"].as_str().unwrap().to_owned()
    }

    /// Three people: Ada (email and external id), Bob (email) and an anonymous visitor.
    async fn seed_people(pool: &PgPool, ws: &str) -> (String, String, String) {
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let outcomes = zirv_flow_store::apply_profile_batch(
            pool,
            workspace,
            &[
                profile("1", &[("email", "ada@example.com"), ("external:crm", "1")]),
                profile("2", &[("email", "bob@example.com")]),
                profile("3", &[("anonymous_id", "visitor")]),
            ],
        )
        .await
        .unwrap();
        let person = |outcome: &zirv_flow_store::ProfileOutcome| match outcome {
            zirv_flow_store::ProfileOutcome::Applied { person_id, .. } => person_id.to_string(),
            other => panic!("not applied: {other:?}"),
        };
        (
            person(&outcomes[0]),
            person(&outcomes[1]),
            person(&outcomes[2]),
        )
    }

    async fn people_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM people")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_member_creates_renames_reads_and_deletes_lists_and_each_change_is_audited(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;

        let id = make_list(&router, Auth::Session(&member), &ws, "Newsletter").await;
        let duplicate = call(
            &router,
            "POST",
            &lists_uri(&ws),
            Some(json!({"name": " newsletter "})),
            Auth::Session(&member),
        )
        .await;
        let blank = call(
            &router,
            "POST",
            &lists_uri(&ws),
            Some(json!({"name": "   "})),
            Auth::Session(&member),
        )
        .await;
        let other = make_list(&router, Auth::Session(&owner), &ws, "Beta").await;
        let taken = call(
            &router,
            "PATCH",
            &format!("{}/{other}", lists_uri(&ws)),
            Some(json!({"name": "NEWSLETTER"})),
            Auth::Session(&member),
        )
        .await;
        let empty_patch = call(
            &router,
            "PATCH",
            &format!("{}/{id}", lists_uri(&ws)),
            Some(json!({})),
            Auth::Session(&member),
        )
        .await;
        let renamed = call(
            &router,
            "PATCH",
            &format!("{}/{id}", lists_uri(&ws)),
            Some(json!({"name": "Weekly", "description": "Fridays"})),
            Auth::Session(&member),
        )
        .await;
        let listed = call(
            &router,
            "GET",
            &lists_uri(&ws),
            None,
            Auth::Session(&member),
        )
        .await;
        let one = call(
            &router,
            "GET",
            &format!("{}/{id}", lists_uri(&ws)),
            None,
            Auth::Session(&member),
        )
        .await;
        let deleted = call(
            &router,
            "DELETE",
            &format!("{}/{id}", lists_uri(&ws)),
            None,
            Auth::Session(&member),
        )
        .await;
        let gone = call(
            &router,
            "GET",
            &format!("{}/{id}", lists_uri(&ws)),
            None,
            Auth::Session(&member),
        )
        .await;

        assert_eq!(duplicate.status, StatusCode::CONFLICT);
        assert_eq!(blank.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(taken.status, StatusCode::CONFLICT);
        assert_eq!(empty_patch.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(renamed.status, StatusCode::OK);
        assert_eq!(renamed.body["name"], "Weekly");
        assert_eq!(renamed.body["member_count"], 0);
        assert_eq!(renamed.body["created_by"], member_id.to_string().as_str());
        let names: Vec<&str> = listed
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|list| list["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Beta", "Weekly"]);
        assert_eq!(one.body["description"], "Fridays");
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        assert_eq!(gone.status, StatusCode::NOT_FOUND);

        let audited: Vec<(String, Option<String>, Value)> = sqlx::query_as(
            "SELECT action, object_id::text, summary FROM audit_log \
             WHERE action LIKE 'list.%' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let actions: Vec<&str> = audited.iter().map(|(action, ..)| action.as_str()).collect();
        assert_eq!(
            actions,
            [
                "list.created",
                "list.created",
                "list.updated",
                "list.deleted"
            ]
        );
        assert_eq!(audited[0].2, json!({"name": "Newsletter"}));
        assert_eq!(
            audited[2].2,
            json!({"name": "Weekly", "fields": ["name", "description"]})
        );
        assert_eq!(audited[3].2, json!({"name": "Weekly", "member_count": 0}));
        assert_eq!(audited[3].1.as_deref(), Some(id.as_str()));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn people_are_added_by_id_and_by_identifier_idempotently_and_never_created(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (ada, bob, _) = seed_people(&pool, &ws).await;
        let list = make_list(&router, Auth::Session(&owner), &ws, "VIPs").await;
        let members = format!("{}/{list}/members", lists_uri(&ws));
        let before = people_count(&pool).await;

        let first = call(
            &router,
            "POST",
            &members,
            Some(json!({
                "person_ids": [ada, "00000000-0000-7000-8000-000000000000"],
                "identifiers": [
                    {"kind": "email", "value": " BOB@example.com "},
                    {"kind": "email", "value": "nobody@example.com"}
                ]
            })),
            Auth::Session(&owner),
        )
        .await;
        let again = call(
            &router,
            "POST",
            &members,
            Some(json!({"person_ids": [ada, bob]})),
            Auth::Session(&owner),
        )
        .await;
        let malformed = call(
            &router,
            "POST",
            &members,
            Some(json!({"identifiers": [{"kind": "email", "value": "not-an-email-secret"}]})),
            Auth::Session(&owner),
        )
        .await;
        let too_many = call(
            &router,
            "POST",
            &members,
            Some(json!({"person_ids": vec![ada.as_str(); 1001]})),
            Auth::Session(&owner),
        )
        .await;
        let one = call(
            &router,
            "GET",
            &format!("{}/{list}", lists_uri(&ws)),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(first.status, StatusCode::OK, "{:?}", first.body);
        assert_eq!(first.body["added"], 2);
        assert_eq!(first.body["already_member"], 0);
        assert_eq!(first.body["not_found_count"], 2);
        assert_eq!(
            first.body["not_found"],
            json!([
                {"field": "person_ids", "index": 1, "reason": "not_found"},
                {"field": "identifiers", "index": 1, "reason": "not_found"}
            ])
        );
        assert_eq!(again.body["added"], 0);
        assert_eq!(again.body["already_member"], 2);
        assert_eq!(malformed.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(malformed.body["detail"], "identifiers[0]: invalid_email");
        assert!(!malformed.body.to_string().contains("secret"));
        assert_eq!(too_many.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(one.body["member_count"], 2);
        assert_eq!(people_count(&pool).await, before);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn removing_is_idempotent_and_the_count_follows(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (ada, bob, visitor) = seed_people(&pool, &ws).await;
        let list = make_list(&router, Auth::Session(&owner), &ws, "VIPs").await;
        let members = format!("{}/{list}/members", lists_uri(&ws));
        call(
            &router,
            "POST",
            &members,
            Some(json!({"person_ids": [ada, bob]})),
            Auth::Session(&owner),
        )
        .await;

        let removed = call(
            &router,
            "POST",
            &format!("{members}/remove"),
            Some(json!({"person_ids": [ada, visitor, "00000000-0000-7000-8000-000000000000"]})),
            Auth::Session(&owner),
        )
        .await;
        let again = call(
            &router,
            "POST",
            &format!("{members}/remove"),
            Some(json!({"person_ids": [ada]})),
            Auth::Session(&owner),
        )
        .await;
        let one = call(
            &router,
            "GET",
            &format!("{}/{list}", lists_uri(&ws)),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(removed.status, StatusCode::OK, "{:?}", removed.body);
        assert_eq!(removed.body["removed"], 1);
        assert_eq!(removed.body["not_member"], 1);
        assert_eq!(removed.body["not_found_count"], 1);
        assert_eq!(
            removed.body["not_found"],
            json!([{"field": "person_ids", "index": 2, "reason": "not_found"}])
        );
        assert_eq!(again.body["removed"], 0);
        assert_eq!(again.body["not_member"], 1);
        assert_eq!(one.body["member_count"], 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn members_page_newest_first_and_values_are_masked_for_members_and_read_keys(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (ada, bob, visitor) = seed_people(&pool, &ws).await;
        let list = make_list(&router, Auth::Session(&owner), &ws, "VIPs").await;
        let members = format!("{}/{list}/members", lists_uri(&ws));
        for person in [&ada, &bob, &visitor] {
            call(
                &router,
                "POST",
                &members,
                Some(json!({"person_ids": [person]})),
                Auth::Session(&owner),
            )
            .await;
        }
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let (_, admin_key) = make_key(&router, &owner, &ws, &["admin", "read"]).await;

        let first = call(
            &router,
            "GET",
            &format!("{members}?limit=2"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let cursor = first.body["next_before"].as_str().unwrap().to_owned();
        let second = call(
            &router,
            "GET",
            &format!("{members}?limit=2&before={cursor}"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let as_member = call(&router, "GET", &members, None, Auth::Session(&member)).await;
        let as_read_key = call(&router, "GET", &members, None, Auth::Key(&read_key)).await;
        let as_admin_key = call(&router, "GET", &members, None, Auth::Key(&admin_key)).await;
        let bad_limit = call(
            &router,
            "GET",
            &format!("{members}?limit=0"),
            None,
            Auth::Session(&owner),
        )
        .await;

        let ids = |reply: &Reply| -> Vec<String> {
            reply.body["members"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["person_id"].as_str().unwrap().to_owned())
                .collect()
        };
        let mut paged = ids(&first);
        paged.extend(ids(&second));
        assert_eq!(paged, [visitor.clone(), bob.clone(), ada.clone()]);
        assert!(second.body["next_before"].is_null());
        assert_eq!(first.body["members"][0]["source"], "ui");
        assert_eq!(
            first.body["members"][0]["identifier"]["kind"],
            "anonymous_id"
        );
        assert_eq!(first.body["members"][0]["identifier"]["value"], "visitor");
        assert_eq!(first.body["members"][0]["identifier"]["masked"], false);
        for masked in [&as_member, &as_read_key] {
            assert_eq!(masked.status, StatusCode::OK);
            let text = masked.body.to_string();
            assert!(!text.contains("example.com") && !text.contains("visitor"));
            for entry in masked.body["members"].as_array().unwrap() {
                assert!(entry["identifier"]["value"].is_null());
                assert_eq!(entry["identifier"]["masked"], true);
            }
        }
        assert_eq!(
            as_member.body["members"][2]["identifier"]["kind"],
            "external:crm"
        );
        assert_eq!(as_admin_key.body["members"][2]["identifier"]["value"], "1");
        assert_eq!(bad_limit.status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn writes_need_the_write_scope_and_a_csrf_token_and_reads_the_read_scope(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let other_ws = workspace(&router, &owner, "Beta").await;
        let (ada, ..) = seed_people(&pool, &ws).await;
        let list = make_list(&router, Auth::Session(&owner), &ws, "VIPs").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let (_, write_key) = make_key(&router, &owner, &ws, &["write"]).await;
        let (_, admin_key) = make_key(&router, &owner, &ws, &["admin", "ingest"]).await;
        let members = format!("{}/{list}/members", lists_uri(&ws));
        let add_body = Some(json!({"person_ids": [ada]}));

        let mut statuses = Vec::new();
        for key in [&read_key, &admin_key] {
            for (method, uri, body) in [
                ("POST", lists_uri(&ws), Some(json!({"name": "X"}))),
                (
                    "PATCH",
                    format!("{}/{list}", lists_uri(&ws)),
                    Some(json!({"name": "Y"})),
                ),
                ("DELETE", format!("{}/{list}", lists_uri(&ws)), None),
                ("POST", members.clone(), add_body.clone()),
                ("POST", format!("{members}/remove"), add_body.clone()),
            ] {
                statuses.push(
                    call(&router, method, &uri, body, Auth::Key(key))
                        .await
                        .status,
                );
            }
            let import = call_raw(
                &router,
                &format!("{members}/import"),
                Some("text/csv"),
                b"kind,value\n".to_vec(),
                Auth::Key(key),
            )
            .await;
            statuses.push(import.status);
        }
        let no_csrf = call(
            &router,
            "POST",
            &members,
            add_body.clone(),
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;
        let write_added = call(&router, "POST", &members, add_body, Auth::Key(&write_key)).await;
        let write_cannot_read = call(&router, "GET", &members, None, Auth::Key(&write_key)).await;
        let read_reads = call(&router, "GET", &lists_uri(&ws), None, Auth::Key(&read_key)).await;
        let foreign = call(
            &router,
            "GET",
            &format!("{}/{list}", lists_uri(&other_ws)),
            None,
            Auth::Session(&owner),
        )
        .await;
        let foreign_key = call(
            &router,
            "GET",
            &lists_uri(&other_ws),
            None,
            Auth::Key(&read_key),
        )
        .await;

        assert!(
            statuses
                .iter()
                .all(|status| *status == StatusCode::FORBIDDEN),
            "{statuses:?}"
        );
        assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
        assert_eq!(write_added.status, StatusCode::OK);
        assert_eq!(write_cannot_read.status, StatusCode::FORBIDDEN);
        assert_eq!(read_reads.status, StatusCode::OK);
        assert_eq!(foreign.status, StatusCode::NOT_FOUND);
        assert_eq!(foreign_key.status, StatusCode::NOT_FOUND);
        let source: String = sqlx::query_scalar("SELECT source FROM list_members")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(source, "api");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_csv_import_previews_then_commits_and_reports_rows_never_values(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (ada, ..) = seed_people(&pool, &ws).await;
        let list = make_list(&router, Auth::Session(&owner), &ws, "VIPs").await;
        let import = format!("{}/{list}/members/import", lists_uri(&ws));
        call(
            &router,
            "POST",
            &format!("{}/{list}/members", lists_uri(&ws)),
            Some(json!({"person_ids": [ada]})),
            Auth::Session(&owner),
        )
        .await;
        let before = people_count(&pool).await;
        let csv = "kind,value\n\
                   email,ada@example.com\n\
                   email,bob@example.com\n\
                   email,bob@example.com\n\
                   email,ghost-person@example.com\n\
                   email,not-an-email-secret\n\
                   fax,12345\n\
                   email\n\
                   anonymous_id,visitor\n";
        let post = |uri: String| {
            let router = router.clone();
            let owner = &owner;
            async move {
                call_raw(
                    &router,
                    &uri,
                    Some("text/csv; charset=utf-8"),
                    csv.as_bytes().to_vec(),
                    Auth::Session(owner),
                )
                .await
            }
        };

        let preview = post(format!("{import}?dry_run=true")).await;
        let count_after_preview: i64 = sqlx::query_scalar("SELECT count(*) FROM list_members")
            .fetch_one(&pool)
            .await
            .unwrap();
        let committed = post(import.clone()).await;
        let repeated = post(import.clone()).await;
        let count_after_commit: i64 = sqlx::query_scalar("SELECT count(*) FROM list_members")
            .fetch_one(&pool)
            .await
            .unwrap();

        let expected_unmatched = json!([
            {"row": 5, "reason": "not_found"},
            {"row": 6, "reason": "invalid_email"},
            {"row": 7, "reason": "unknown_identifier_kind"},
            {"row": 8, "reason": "malformed_row"}
        ]);
        assert_eq!(preview.status, StatusCode::OK, "{:?}", preview.body);
        assert_eq!(preview.body["matched"], 4);
        assert_eq!(preview.body["added"], 2);
        assert_eq!(preview.body["already_member"], 2);
        assert_eq!(preview.body["unmatched"], expected_unmatched);
        assert_eq!(preview.body["unmatched_count"], 4);
        assert_eq!(count_after_preview, 1);
        assert_eq!(committed.body, preview.body);
        assert_eq!(count_after_commit, 3);
        assert_eq!(repeated.body["added"], 0);
        assert_eq!(repeated.body["already_member"], 4);
        for reply in [&preview, &committed, &repeated] {
            let text = reply.body.to_string();
            assert!(!text.contains("example.com") && !text.contains("secret"));
        }
        assert_eq!(people_count(&pool).await, before);
        let imported: i64 =
            sqlx::query_scalar("SELECT count(*) FROM list_members WHERE source = 'import'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(imported, 2);
        let one = call(
            &router,
            "GET",
            &format!("{}/{list}", lists_uri(&ws)),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(one.body["member_count"], 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_import_refuses_a_wrong_type_a_bad_header_a_big_body_and_a_bad_flag(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let list = make_list(&router, Auth::Session(&owner), &ws, "VIPs").await;
        let import = format!("{}/{list}/members/import", lists_uri(&ws));
        let send = |uri: String, content_type: Option<&'static str>, body: Vec<u8>| {
            let router = router.clone();
            let owner = &owner;
            async move { call_raw(&router, &uri, content_type, body, Auth::Session(owner)).await }
        };

        let json_type = send(
            import.clone(),
            Some("application/json"),
            b"kind,value\n".to_vec(),
        )
        .await;
        let no_type = send(import.clone(), None, b"kind,value\n".to_vec()).await;
        let header = send(import.clone(), Some("text/csv"), b"email,value\n".to_vec()).await;
        let binary = send(import.clone(), Some("text/csv"), vec![0xff, 0xfe, 0x00]).await;
        let flag = send(
            format!("{import}?dry_run=maybe"),
            Some("text/csv"),
            b"kind,value\n".to_vec(),
        )
        .await;
        let mut big = b"kind,value\n".to_vec();
        big.resize(10 * 1024 * 1024 + 1, b'x');
        let too_big = send(import.clone(), Some("text/csv"), big).await;
        let missing = send(
            format!(
                "{}/00000000-0000-7000-8000-000000000000/members/import",
                lists_uri(&ws)
            ),
            Some("text/csv"),
            b"kind,value\n".to_vec(),
        )
        .await;
        let empty = send(import.clone(), Some("text/csv"), b"kind,value\n".to_vec()).await;

        assert_eq!(json_type.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(no_type.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(header.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(binary.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(flag.status, StatusCode::BAD_REQUEST);
        assert_eq!(too_big.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        assert_eq!(empty.status, StatusCode::OK);
        assert_eq!(empty.body["matched"], 0);
        assert_eq!(empty.body["unmatched"], json!([]));
        assert_eq!(empty.body["unmatched_count"], 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_persons_lists_and_history_follow_them_and_their_merges(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (ada, _, visitor) = seed_people(&pool, &ws).await;
        let first = make_list(&router, Auth::Session(&owner), &ws, "First").await;
        let second = make_list(&router, Auth::Session(&owner), &ws, "Second").await;
        for (list, person) in [(&first, &ada), (&second, &visitor)] {
            call(
                &router,
                "POST",
                &format!("{}/{list}/members", lists_uri(&ws)),
                Some(json!({"person_ids": [person]})),
                Auth::Session(&owner),
            )
            .await;
        }
        call(
            &router,
            "POST",
            &format!("{}/{first}/members/remove", lists_uri(&ws)),
            Some(json!({"person_ids": [ada]})),
            Auth::Session(&owner),
        )
        .await;
        call(
            &router,
            "POST",
            &format!("{}/{first}/members", lists_uri(&ws)),
            Some(json!({"person_ids": [ada]})),
            Auth::Session(&owner),
        )
        .await;
        // A record naming both identifiers merges the visitor into Ada.
        let workspace_id: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        zirv_flow_store::apply_profile_batch(
            &pool,
            workspace_id,
            &[profile(
                "9",
                &[("email", "ada@example.com"), ("anonymous_id", "visitor")],
            )],
        )
        .await
        .unwrap();
        let person_lists = |person: &str| format!("/api/v1/workspaces/{ws}/people/{person}/lists");

        let lists = call(
            &router,
            "GET",
            &person_lists(&ada),
            None,
            Auth::Session(&owner),
        )
        .await;
        let through_loser = call(
            &router,
            "GET",
            &person_lists(&visitor),
            None,
            Auth::Session(&owner),
        )
        .await;
        let history = call(
            &router,
            "GET",
            &format!("{}/history?limit=3", person_lists(&ada)),
            None,
            Auth::Session(&owner),
        )
        .await;
        let rest = call(
            &router,
            "GET",
            &format!(
                "{}/history?limit=3&before={}",
                person_lists(&ada),
                history.body["next_before"].as_str().unwrap()
            ),
            None,
            Auth::Session(&owner),
        )
        .await;
        let unknown = call(
            &router,
            "GET",
            &person_lists("00000000-0000-7000-8000-000000000000"),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(lists.status, StatusCode::OK, "{:?}", lists.body);
        let names: Vec<&str> = lists
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|list| list["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["First", "Second"]);
        assert_eq!(lists.body[1]["source"], "merge");
        assert_eq!(through_loser.body, lists.body);
        assert_eq!(history.body["changes"].as_array().unwrap().len(), 3);
        assert!(history.body["next_before"].is_string());
        assert_eq!(history.body["changes"][0]["action"], "added");
        assert_eq!(history.body["changes"][0]["list_name"], "First");
        assert_eq!(rest.body["changes"].as_array().unwrap().len(), 1);
        assert!(rest.body["next_before"].is_null());
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        let membership_audit: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action LIKE 'list.m%'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(membership_audit, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_long_unmatched_list_is_cut_at_a_thousand_with_the_total(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let list = make_list(&router, Auth::Session(&owner), &ws, "VIPs").await;
        let mut csv = String::from("kind,value\n");
        for n in 0..1200 {
            csv.push_str(&format!("email,nobody{n}@example.com\n"));
        }

        let reply = call_raw(
            &router,
            &format!("{}/{list}/members/import", lists_uri(&ws)),
            Some("text/csv"),
            csv.into_bytes(),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{:?}",
            reply.body.get("detail")
        );
        assert_eq!(reply.body["matched"], 0);
        assert_eq!(reply.body["unmatched_count"], 1200);
        let rows = reply.body["unmatched"].as_array().unwrap();
        assert_eq!(rows.len(), 1000);
        assert_eq!(rows[0]["row"], 2);
        assert_eq!(rows[999]["row"], 1001);
    }

    fn topics_uri(ws: &str) -> String {
        format!("/api/v1/workspaces/{ws}/topics")
    }

    fn consent_uri(ws: &str, person: &str) -> String {
        format!("/api/v1/workspaces/{ws}/people/{person}/consent")
    }

    fn suppressions_uri(ws: &str, rest: &str) -> String {
        format!("/api/v1/workspaces/{ws}/suppressions{rest}")
    }

    const SUPPRESSION_KEY: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";

    async fn keyed_app(pool: &PgPool) -> Router {
        zirv_flow_store::migrate(pool).await.unwrap();
        router(
            AppState::new(pool.clone(), Arc::new(String::new)).with_hmac_root(
                &zirv_flow_core::HmacRoot::bootstrap_from(
                    &zirv_flow_core::MasterKey::parse(SUPPRESSION_KEY).unwrap(),
                ),
            ),
        )
    }

    async fn seed_person(pool: &PgPool, ws: &str, email: &str) -> String {
        let outcomes = zirv_flow_store::apply_profile_batch(
            pool,
            ws.parse().unwrap(),
            &[profile(email, &[("email", email)])],
        )
        .await
        .unwrap();
        match &outcomes[0] {
            zirv_flow_store::ProfileOutcome::Applied { person_id, .. } => person_id.to_string(),
            other => panic!("not applied: {other:?}"),
        }
    }

    async fn create_topic(router: &Router, owner: &Login, ws: &str, key: &str, policy: &str) {
        let reply = call(
            router,
            "POST",
            &topics_uri(ws),
            Some(json!({"key": key, "name": key, "channel": "email", "policy": policy})),
            Auth::Session(owner),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn topics_are_created_listed_updated_archived_and_audited(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;

        let created = call(
            &router,
            "POST",
            &topics_uri(&ws),
            Some(json!({"key": "news", "name": "News", "description": "Monthly", "channel": "email", "policy": "opt_out"})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        assert_eq!(created.body["policy"], "opt_out");
        assert_eq!(created.body["archived"], false);
        let id = created.body["id"].as_str().unwrap().to_owned();
        let duplicate = call(
            &router,
            "POST",
            &topics_uri(&ws),
            Some(json!({"key": "news", "name": "Again", "channel": "email", "policy": "opt_in"})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(duplicate.status, StatusCode::CONFLICT);
        for bad in [
            json!({"key": "News", "name": "n", "channel": "email", "policy": "opt_in"}),
            json!({"key": "ok", "name": "", "channel": "email", "policy": "opt_in"}),
            json!({"key": "ok", "name": "n", "channel": "sms", "policy": "opt_in"}),
            json!({"key": "ok", "name": "n", "channel": "email", "policy": "always"}),
            json!({"key": "ok", "name": "n", "description": "d".repeat(501), "channel": "email", "policy": "opt_in"}),
        ] {
            let reply = call(
                &router,
                "POST",
                &topics_uri(&ws),
                Some(bad),
                Auth::Session(&owner),
            )
            .await;
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{:?}",
                reply.body
            );
        }

        let patched = call(
            &router,
            "PATCH",
            &format!("{}/{id}", topics_uri(&ws)),
            Some(json!({"policy": "opt_in", "archived": true})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);
        assert_eq!(patched.body["policy"], "opt_in");
        assert_eq!(patched.body["archived"], true);
        let visible = call(
            &router,
            "GET",
            &topics_uri(&ws),
            None,
            Auth::Session(&owner),
        )
        .await;
        let all = call(
            &router,
            "GET",
            &format!("{}?archived=true", topics_uri(&ws)),
            None,
            Auth::Session(&owner),
        )
        .await;
        let bad_flag = call(
            &router,
            "GET",
            &format!("{}?archived=maybe", topics_uri(&ws)),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(visible.body.as_array().unwrap().len(), 0);
        assert_eq!(all.body.as_array().unwrap().len(), 1);
        assert_eq!(bad_flag.status, StatusCode::BAD_REQUEST);
        for empty in [json!({}), json!({"policy": "never"}), json!({"name": " "})] {
            let reply = call(
                &router,
                "PATCH",
                &format!("{}/{id}", topics_uri(&ws)),
                Some(empty),
                Auth::Session(&owner),
            )
            .await;
            assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        }
        let missing = call(
            &router,
            "PATCH",
            &format!("{}/{}", topics_uri(&ws), uuid::Uuid::now_v7()),
            Some(json!({"name": "x"})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);

        let audit = zirv_flow_store::list_audit(&pool, 50).await.unwrap();
        let created_row = audit
            .iter()
            .find(|row| row.action == "topic.created")
            .unwrap();
        assert_eq!(created_row.summary, json!({"key": "news"}));
        assert_eq!(created_row.object_kind.as_deref(), Some("topic"));
        let updated_row = audit
            .iter()
            .find(|row| row.action == "topic.updated")
            .unwrap();
        assert_eq!(
            updated_row.summary,
            json!({"key": "news", "fields": ["policy", "archived"]})
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_admins_change_topics_and_other_workspaces_see_none(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let other = workspace(&router, &owner, "Beta").await;
        create_topic(&router, &owner, &ws, "news", "opt_out").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let (_, admin_key) = make_key(&router, &owner, &ws, &["admin"]).await;
        let body = json!({"key": "x", "name": "x", "channel": "email", "policy": "opt_in"});

        let member_read = call(
            &router,
            "GET",
            &topics_uri(&ws),
            None,
            Auth::Session(&member),
        )
        .await;
        let key_read = call(&router, "GET", &topics_uri(&ws), None, Auth::Key(&read_key)).await;
        let member_create = call(
            &router,
            "POST",
            &topics_uri(&ws),
            Some(body.clone()),
            Auth::Session(&member),
        )
        .await;
        let read_key_create = call(
            &router,
            "POST",
            &topics_uri(&ws),
            Some(body.clone()),
            Auth::Key(&read_key),
        )
        .await;
        let no_csrf = call(
            &router,
            "POST",
            &topics_uri(&ws),
            Some(body.clone()),
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;
        let admin_key_create = call(
            &router,
            "POST",
            &topics_uri(&ws),
            Some(body),
            Auth::Key(&admin_key),
        )
        .await;
        let foreign = call(
            &router,
            "GET",
            &topics_uri(&other),
            None,
            Auth::Key(&read_key),
        )
        .await;
        let anonymous = call(&router, "GET", &topics_uri(&ws), None, Auth::Anonymous).await;
        let in_other = call(
            &router,
            "GET",
            &topics_uri(&other),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(member_read.status, StatusCode::OK);
        assert_eq!(key_read.status, StatusCode::OK);
        assert_eq!(member_create.status, StatusCode::FORBIDDEN);
        assert_eq!(read_key_create.status, StatusCode::FORBIDDEN);
        assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
        assert_eq!(admin_key_create.status, StatusCode::CREATED);
        assert_eq!(foreign.status, StatusCode::NOT_FOUND);
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(in_other.body.as_array().unwrap().len(), 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn consent_is_set_read_and_its_history_shows_source_and_evidence(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        create_topic(&router, &owner, &ws, "news", "opt_out").await;
        let person = seed_person(&pool, &ws, "ada@example.com").await;
        let (_, write_key) = make_key(&router, &owner, &ws, &["write"]).await;
        let evidence = json!({"form": "footer", "ip": "203.0.113.9"});

        let subscribed = call(
            &router,
            "PUT",
            &consent_uri(&ws, &person),
            Some(json!({"channel": "email", "topic_key": "news", "status": "subscribed", "evidence": evidence})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(subscribed.status, StatusCode::OK, "{:?}", subscribed.body);
        assert_eq!(subscribed.body["person_id"], person);
        assert_eq!(subscribed.body["states"][0]["topic_key"], "news");
        assert_eq!(subscribed.body["states"][0]["status"], "subscribed");
        assert_eq!(subscribed.body["states"][0]["explicit"], true);
        let repeated = call(
            &router,
            "PUT",
            &consent_uri(&ws, &person),
            Some(json!({"channel": "email", "topic_key": "news", "status": "subscribed"})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(repeated.status, StatusCode::OK);
        let unsubscribed = call(
            &router,
            "PUT",
            &consent_uri(&ws, &person),
            Some(json!({"channel": "email", "status": "unsubscribed"})),
            Auth::Key(&write_key),
        )
        .await;
        assert_eq!(
            unsubscribed.status,
            StatusCode::OK,
            "{:?}",
            unsubscribed.body
        );
        let states = unsubscribed.body["states"].as_array().unwrap();
        assert_eq!(states.len(), 2);
        assert_eq!(states[0]["topic_key"], serde_json::Value::Null);
        assert_eq!(states[0]["status"], "unsubscribed");

        let read = call(
            &router,
            "GET",
            &consent_uri(&ws, &person),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(read.body, unsubscribed.body);
        let history = call(
            &router,
            "GET",
            &format!("{}/history", consent_uri(&ws, &person)),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(history.status, StatusCode::OK, "{:?}", history.body);
        let entries = history.body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "the repeated write left no history");
        assert_eq!(entries[0]["source"], "api");
        assert_eq!(entries[0]["actor_user_id"], serde_json::Value::Null);
        assert_eq!(entries[0]["from_status"], serde_json::Value::Null);
        assert_eq!(entries[0]["to_status"], "unsubscribed");
        assert_eq!(entries[1]["source"], "ui");
        assert!(entries[1]["actor_user_id"].is_string());
        assert_eq!(entries[1]["topic_key"], "news");
        assert_eq!(entries[1]["evidence"], evidence);
        assert_eq!(entries[1]["masked"], false);
        assert_eq!(history.body["next_before"], serde_json::Value::Null);

        let first_page = call(
            &router,
            "GET",
            &format!("{}/history?limit=1", consent_uri(&ws, &person)),
            None,
            Auth::Session(&owner),
        )
        .await;
        let cursor = first_page.body["next_before"].as_str().unwrap();
        let second_page = call(
            &router,
            "GET",
            &format!(
                "{}/history?limit=1&before={cursor}",
                consent_uri(&ws, &person)
            ),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(first_page.body["entries"][0]["source"], "api");
        assert_eq!(second_page.body["entries"][0]["source"], "ui");
        assert_eq!(second_page.body["next_before"], serde_json::Value::Null);
        let bad_limit = call(
            &router,
            "GET",
            &format!("{}/history?limit=0", consent_uri(&ws, &person)),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(bad_limit.status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn consent_writes_are_validated_and_follow_the_write_rules(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let other = workspace(&router, &owner, "Beta").await;
        create_topic(&router, &owner, &ws, "news", "opt_out").await;
        let person = seed_person(&pool, &ws, "ada@example.com").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let put = |body: serde_json::Value| {
            let router = router.clone();
            let uri = consent_uri(&ws, &person);
            let owner = &owner;
            async move { call(&router, "PUT", &uri, Some(body), Auth::Session(owner)).await }
        };

        for bad in [
            json!({"channel": "email", "status": "unknown"}),
            json!({"channel": "email", "status": "maybe"}),
            json!({"channel": "sms", "status": "subscribed"}),
            json!({"channel": "email", "topic_key": "nope", "status": "subscribed"}),
            json!({"channel": "email", "status": "subscribed", "evidence": ["x"]}),
            json!({"channel": "email", "status": "subscribed", "evidence": {"text": "x".repeat(5000)}}),
        ] {
            let reply = put(bad.clone()).await;
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{bad}: {:?}",
                reply.body
            );
        }
        let pending = put(json!({"channel": "email", "status": "pending_confirmation"})).await;
        assert_eq!(pending.status, StatusCode::OK);
        assert_eq!(pending.body["states"][0]["explicit"], false);

        let by_member = call(
            &router,
            "PUT",
            &consent_uri(&ws, &person),
            Some(json!({"channel": "email", "status": "subscribed"})),
            Auth::Session(&member),
        )
        .await;
        let by_read_key = call(
            &router,
            "PUT",
            &consent_uri(&ws, &person),
            Some(json!({"channel": "email", "status": "unsubscribed"})),
            Auth::Key(&read_key),
        )
        .await;
        let no_csrf = call(
            &router,
            "PUT",
            &consent_uri(&ws, &person),
            Some(json!({"channel": "email", "status": "unsubscribed"})),
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;
        let read_by_read_key = call(
            &router,
            "GET",
            &consent_uri(&ws, &person),
            None,
            Auth::Key(&read_key),
        )
        .await;
        let stranger = call(
            &router,
            "PUT",
            &consent_uri(&ws, &uuid::Uuid::now_v7().to_string()),
            Some(json!({"channel": "email", "status": "subscribed"})),
            Auth::Session(&owner),
        )
        .await;
        let wrong_workspace = call(
            &router,
            "GET",
            &consent_uri(&other, &person),
            None,
            Auth::Session(&owner),
        )
        .await;
        let wrong_workspace_history = call(
            &router,
            "GET",
            &format!("{}/history", consent_uri(&other, &person)),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(by_member.status, StatusCode::OK);
        assert_eq!(by_read_key.status, StatusCode::FORBIDDEN);
        assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
        assert_eq!(read_by_read_key.status, StatusCode::OK);
        assert_eq!(read_by_read_key.body["states"][0]["status"], "subscribed");
        assert_eq!(stranger.status, StatusCode::NOT_FOUND);
        assert_eq!(wrong_workspace.status, StatusCode::NOT_FOUND);
        assert_eq!(wrong_workspace_history.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_suppression_is_added_checked_listed_and_removed_and_no_value_is_stored(
        pool: PgPool,
    ) {
        let router = keyed_app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;

        let added = call(
            &router,
            "POST",
            &suppressions_uri(&ws, ""),
            Some(json!({"kind": "email", "value": " Ada@Example.COM ", "note": "asked by phone"})),
            Auth::Session(&owner),
        )
        .await;
        let again = call(
            &router,
            "POST",
            &suppressions_uri(&ws, ""),
            Some(json!({"kind": "email", "value": "ada@example.com"})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(added.status, StatusCode::OK, "{:?}", added.body);
        assert_eq!(added.body, json!({"added": true}));
        assert_eq!(again.body, json!({"added": false}));

        let check = |value: &'static str| {
            let router = router.clone();
            let uri = suppressions_uri(&ws, "/check");
            let owner = &owner;
            async move {
                call(
                    &router,
                    "POST",
                    &uri,
                    Some(json!({"kind": "email", "value": value})),
                    Auth::Session(owner),
                )
                .await
            }
        };
        assert_eq!(
            check("ADA@example.com").await.body,
            json!({"suppressed": true, "reason": "manual"})
        );
        assert_eq!(
            check("bob@example.com").await.body,
            json!({"suppressed": false, "reason": null})
        );

        let list = call(
            &router,
            "GET",
            &suppressions_uri(&ws, ""),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(list.status, StatusCode::OK);
        let entries = list.body["suppressions"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["hash_prefix"].as_str().unwrap().len(), 8);
        assert_eq!(entries[0]["kind"], "email");
        assert_eq!(entries[0]["reason"], "manual");
        assert_eq!(entries[0]["source"], "ui");
        assert_eq!(entries[0]["note"], "asked by phone");
        assert!(entries[0].get("hash").is_none() && entries[0].get("value").is_none());
        let hits = call(
            &router,
            "GET",
            &suppressions_uri(&ws, "?reason=hard_bounce"),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(hits.body["suppressions"].as_array().unwrap().len(), 0);
        let bad_reason = call(
            &router,
            "GET",
            &suppressions_uri(&ws, "?reason=rude"),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(bad_reason.status, StatusCode::BAD_REQUEST);

        let stored: String = sqlx::query_scalar("SELECT s::text FROM suppressions s")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!stored.to_lowercase().contains("ada@") && !stored.contains("example.com"));

        for bad in [
            json!({"kind": "email", "value": "nope"}),
            json!({"kind": "phone", "value": "12"}),
            json!({"kind": "anonymous_id", "value": "x"}),
            json!({"kind": "email", "value": "cy@example.com", "note": "this is Cy@Example.com"}),
            json!({"kind": "email", "value": "cy@example.com", "note": "n".repeat(501)}),
        ] {
            let reply = call(
                &router,
                "POST",
                &suppressions_uri(&ws, ""),
                Some(bad.clone()),
                Auth::Session(&owner),
            )
            .await;
            assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
            assert!(!reply.body["detail"].as_str().unwrap().contains('@'));
        }

        let removed = call(
            &router,
            "POST",
            &suppressions_uri(&ws, "/remove"),
            Some(json!({"kind": "email", "value": "ada@example.com"})),
            Auth::Session(&owner),
        )
        .await;
        let removed_again = call(
            &router,
            "POST",
            &suppressions_uri(&ws, "/remove"),
            Some(json!({"kind": "email", "value": "ada@example.com"})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(removed.status, StatusCode::NO_CONTENT);
        assert_eq!(removed_again.status, StatusCode::NOT_FOUND);
        assert_eq!(check("ada@example.com").await.body["suppressed"], false);

        let audit = zirv_flow_store::list_audit(&pool, 50).await.unwrap();
        let actions: Vec<&str> = audit
            .iter()
            .map(|row| row.action.as_str())
            .filter(|action| action.starts_with("suppression."))
            .collect();
        assert_eq!(actions, ["suppression.removed", "suppression.added"]);
        let text = serde_json::to_string(&audit.iter().map(|row| &row.summary).collect::<Vec<_>>())
            .unwrap();
        assert!(!text.to_lowercase().contains("ada@") && !text.contains("asked by phone"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn suppressions_follow_the_admin_and_read_rules(pool: PgPool) {
        let router = keyed_app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let other = workspace(&router, &owner, "Beta").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let (_, admin_key) = make_key(&router, &owner, &ws, &["admin"]).await;
        let identifier = json!({"kind": "email", "value": "ada@example.com"});

        for (method, rest) in [("POST", ""), ("POST", "/remove")] {
            for auth in [
                Auth::Session(&member),
                Auth::Key(&read_key),
                Auth::SessionWithoutCsrf(&owner),
            ] {
                let reply = call(
                    &router,
                    method,
                    &suppressions_uri(&ws, rest),
                    Some(identifier.clone()),
                    auth,
                )
                .await;
                assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {rest}");
            }
        }
        let import_as_member = call_text(
            &router,
            "POST",
            &suppressions_uri(&ws, "/import"),
            Some("text/csv"),
            "kind,value\n".into(),
            Auth::Session(&member),
        )
        .await;
        let export_as_member = call_text(
            &router,
            "GET",
            &suppressions_uri(&ws, "/export"),
            None,
            String::new(),
            Auth::Session(&member),
        )
        .await;
        let export_as_read_key = call_text(
            &router,
            "GET",
            &suppressions_uri(&ws, "/export"),
            None,
            String::new(),
            Auth::Key(&read_key),
        )
        .await;
        assert_eq!(import_as_member.status, StatusCode::FORBIDDEN);
        assert_eq!(export_as_member.status, StatusCode::FORBIDDEN);
        assert_eq!(export_as_read_key.status, StatusCode::FORBIDDEN);

        let member_check = call(
            &router,
            "POST",
            &suppressions_uri(&ws, "/check"),
            Some(identifier.clone()),
            Auth::Session(&member),
        )
        .await;
        let member_list = call(
            &router,
            "GET",
            &suppressions_uri(&ws, ""),
            None,
            Auth::Session(&member),
        )
        .await;
        let key_check = call(
            &router,
            "POST",
            &suppressions_uri(&ws, "/check"),
            Some(identifier.clone()),
            Auth::Key(&read_key),
        )
        .await;
        let key_add = call(
            &router,
            "POST",
            &suppressions_uri(&ws, ""),
            Some(identifier.clone()),
            Auth::Key(&admin_key),
        )
        .await;
        let foreign = call(
            &router,
            "POST",
            &suppressions_uri(&other, "/check"),
            Some(identifier.clone()),
            Auth::Key(&read_key),
        )
        .await;
        let in_other = call(
            &router,
            "POST",
            &suppressions_uri(&other, "/check"),
            Some(identifier),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(member_check.status, StatusCode::OK);
        assert_eq!(member_list.status, StatusCode::OK);
        assert_eq!(key_check.status, StatusCode::OK);
        assert_eq!(key_add.status, StatusCode::OK);
        assert_eq!(foreign.status, StatusCode::NOT_FOUND);
        assert_eq!(in_other.body["suppressed"], false);
        let source: String = sqlx::query_scalar("SELECT source FROM suppressions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(source, "api");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erased_suppression_cannot_be_removed_or_replaced(pool: PgPool) {
        let router = keyed_app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let key = zirv_flow_core::HmacRoot::bootstrap_from(
            &zirv_flow_core::MasterKey::parse(SUPPRESSION_KEY).unwrap(),
        )
        .hmac_key(zirv_flow_core::HmacPurpose::Identifier);
        let identifier =
            zirv_flow_core::people::Identifier::parse("email", "ada@example.com").unwrap();
        zirv_flow_store::add_suppression(
            &pool,
            ws.parse().unwrap(),
            &zirv_flow_store::NewSuppression {
                identifier_hash: zirv_flow_core::people::identifier_hash(&key, &identifier),
                kind: "email",
                reason: zirv_flow_core::consent::SuppressionReason::Erased,
                source: "erasure",
                note: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let body = json!({"kind": "email", "value": "ada@example.com"});

        let removed = call(
            &router,
            "POST",
            &suppressions_uri(&ws, "/remove"),
            Some(body.clone()),
            Auth::Session(&owner),
        )
        .await;
        let added = call(
            &router,
            "POST",
            &suppressions_uri(&ws, ""),
            Some(body.clone()),
            Auth::Session(&owner),
        )
        .await;
        let checked = call(
            &router,
            "POST",
            &suppressions_uri(&ws, "/check"),
            Some(body),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(removed.status, StatusCode::CONFLICT);
        assert_eq!(added.body, json!({"added": false}));
        assert_eq!(
            checked.body,
            json!({"suppressed": true, "reason": "erased"})
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_import_previews_with_a_dry_run_commits_and_reports_bad_rows_by_number(
        pool: PgPool,
    ) {
        let router = keyed_app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let csv = "kind,value,note\n\
                   email,a@example.com,\n\
                   email,b@example.com,from the old list\n\
                   email,secret-typo,\n\
                   email,a@example.com,\n\
                   phone,+4512345678,\n";
        let post = |query: &'static str, content_type: Option<&'static str>, body: &'static str| {
            let router = router.clone();
            let uri = suppressions_uri(&ws, &format!("/import{query}"));
            let owner = &owner;
            async move {
                call_text(
                    &router,
                    "POST",
                    &uri,
                    content_type,
                    body.to_owned(),
                    Auth::Session(owner),
                )
                .await
            }
        };
        let count = || async {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM suppressions")
                .fetch_one(&pool)
                .await
                .unwrap()
        };

        let dry = post("?dry_run=true", Some("text/csv; charset=utf-8"), csv).await;

        assert_eq!(dry.status, StatusCode::OK, "{}", dry.text);
        let dry_body: serde_json::Value = serde_json::from_str(&dry.text).unwrap();
        assert_eq!(
            dry_body,
            json!({"dry_run": true, "added": 3, "existing": 1,
                   "invalid": [{"row": 4, "reason": "invalid_email"}]})
        );
        assert!(!dry.text.contains("secret-typo"));
        assert_eq!(count().await, 0);

        let committed = post("", Some("text/csv"), csv).await;
        let committed_body: serde_json::Value = serde_json::from_str(&committed.text).unwrap();
        assert_eq!(committed_body["dry_run"], false);
        assert_eq!(committed_body["added"], 3);
        assert_eq!(count().await, 3);
        let again = post("", Some("text/csv"), csv).await;
        let again_body: serde_json::Value = serde_json::from_str(&again.text).unwrap();
        assert_eq!(
            (
                again_body["added"].as_u64(),
                again_body["existing"].as_u64()
            ),
            (Some(0), Some(4))
        );
        let dry_after = post("?dry_run=true", Some("text/csv"), csv).await;
        let dry_after_body: serde_json::Value = serde_json::from_str(&dry_after.text).unwrap();
        assert_eq!(
            (
                dry_after_body["added"].as_u64(),
                dry_after_body["existing"].as_u64()
            ),
            (Some(0), Some(4))
        );
        let sources: Vec<String> = sqlx::query_scalar("SELECT DISTINCT source FROM suppressions")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(sources, ["import"]);

        let audit = zirv_flow_store::list_audit(&pool, 50).await.unwrap();
        let imports: Vec<&serde_json::Value> = audit
            .iter()
            .filter(|row| row.action == "suppression.imported")
            .map(|row| &row.summary)
            .collect();
        assert_eq!(
            imports,
            [
                &json!({"added": 0, "existing": 4, "invalid": 1}),
                &json!({"added": 3, "existing": 1, "invalid": 1})
            ]
        );

        let wrong_type = post("", Some("application/json"), csv).await;
        let no_type = post("", None, csv).await;
        let bad_header = post("", Some("text/csv"), "value,kind\na@x.com,email\n").await;
        let bad_flag = post("?dry_run=maybe", Some("text/csv"), csv).await;
        assert_eq!(wrong_type.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(no_type.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(bad_header.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(bad_flag.status, StatusCode::BAD_REQUEST);
        assert_eq!(count().await, 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_import_over_ten_mebibytes_is_413(pool: PgPool) {
        let router = keyed_app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let body = format!("kind,value\n{}", "x".repeat(10 * 1024 * 1024));

        let reply = call_text(
            &router,
            "POST",
            &suppressions_uri(&ws, "/import"),
            Some("text/csv"),
            body,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_export_is_csv_of_hashes_only(pool: PgPool) {
        let router = keyed_app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let other = workspace(&router, &owner, "Beta").await;
        for (uri, value) in [
            (&ws, "a@example.com"),
            (&ws, "b@example.com"),
            (&other, "c@example.com"),
        ] {
            call(
                &router,
                "POST",
                &suppressions_uri(uri, ""),
                Some(json!({"kind": "email", "value": value, "note": "kept out of the export"})),
                Auth::Session(&owner),
            )
            .await;
        }

        let export = call_text(
            &router,
            "GET",
            &suppressions_uri(&ws, "/export"),
            None,
            String::new(),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(export.status, StatusCode::OK);
        assert!(
            export.headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/csv")
        );
        assert!(
            export.headers[header::CONTENT_DISPOSITION]
                .to_str()
                .unwrap()
                .contains("suppressions.csv")
        );
        let lines: Vec<&str> = export.text.lines().collect();
        assert_eq!(lines[0], "hash,kind,reason,source,created_at");
        assert_eq!(lines.len(), 3, "{}", export.text);
        for line in &lines[1..] {
            let fields: Vec<&str> = line.split(',').collect();
            assert_eq!(fields.len(), 5);
            assert_eq!(fields[0].len(), 64);
            assert!(fields[0].bytes().all(|b| b.is_ascii_hexdigit()));
            assert_eq!(&fields[1..4], ["email", "manual", "ui"]);
        }
        assert!(!export.text.contains('@') && !export.text.contains("kept out"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn suppressions_are_unavailable_without_a_master_key(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let router = router(AppState::new(pool.clone(), Arc::new(String::new)));
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;

        let check = call(
            &router,
            "POST",
            &suppressions_uri(&ws, "/check"),
            Some(json!({"kind": "email", "value": "a@example.com"})),
            Auth::Session(&owner),
        )
        .await;
        let list = call(
            &router,
            "GET",
            &suppressions_uri(&ws, ""),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(check.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(list.status, StatusCode::OK);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn consent_evidence_is_masked_for_callers_who_may_not_see_values(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let person = seed_person(&pool, &ws, "ada@example.com").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let (_, admin_key) = make_key(&router, &owner, &ws, &["read", "admin"]).await;
        let put = call(
            &router,
            "PUT",
            &consent_uri(&ws, &person),
            Some(json!({"channel": "email", "status": "subscribed", "evidence": {"ip": "203.0.113.9"}})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(put.status, StatusCode::OK);
        let uri = format!("{}/history", consent_uri(&ws, &person));

        let as_member = call(&router, "GET", &uri, None, Auth::Session(&member)).await;
        let as_read_key = call(&router, "GET", &uri, None, Auth::Key(&read_key)).await;
        let as_admin_key = call(&router, "GET", &uri, None, Auth::Key(&admin_key)).await;
        let as_owner = call(&router, "GET", &uri, None, Auth::Session(&owner)).await;

        for masked in [&as_member, &as_read_key] {
            assert_eq!(masked.status, StatusCode::OK);
            let entry = &masked.body["entries"][0];
            assert_eq!(entry["evidence"], serde_json::Value::Null);
            assert_eq!(entry["masked"], true);
            assert_eq!(entry["source"], "ui");
            assert!(!masked.body.to_string().contains("203.0.113.9"));
        }
        for shown in [&as_admin_key, &as_owner] {
            let entry = &shown.body["entries"][0];
            assert_eq!(entry["evidence"], json!({"ip": "203.0.113.9"}));
            assert_eq!(entry["masked"], false);
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn evidence_at_the_size_limit_is_stored_and_one_byte_over_is_a_422(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let person = seed_person(&pool, &ws, "ada@example.com").await;
        let mut evidence = serde_json::Map::new();
        let mut index = 0;
        while serde_json::Value::Object(evidence.clone())
            .to_string()
            .len()
            <= 4096
        {
            evidence.insert(format!("k{index:04}"), json!("v"));
            index += 1;
        }
        let over = evidence.clone();
        evidence.remove(&format!("k{:04}", index - 1));
        let mut at_limit = evidence;
        // Pad one value so the compact form is exactly 4096 bytes.
        let short = serde_json::Value::Object(at_limit.clone())
            .to_string()
            .len();
        at_limit.insert(
            "k0000".into(),
            json!(format!("v{}", "x".repeat(4096 - short))),
        );
        assert_eq!(
            serde_json::Value::Object(at_limit.clone())
                .to_string()
                .len(),
            4096
        );
        let put = |evidence: serde_json::Map<String, serde_json::Value>, status: &'static str| {
            let router = router.clone();
            let uri = consent_uri(&ws, &person);
            let owner = &owner;
            async move {
                call(
                    &router,
                    "PUT",
                    &uri,
                    Some(json!({"channel": "email", "status": status, "evidence": evidence})),
                    Auth::Session(owner),
                )
                .await
            }
        };

        let accepted = put(at_limit.clone(), "subscribed").await;
        let refused = put(over, "unsubscribed").await;

        assert_eq!(accepted.status, StatusCode::OK, "{:?}", accepted.body);
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        let stored: serde_json::Value = sqlx::query_scalar("SELECT evidence FROM consent_changes")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, serde_json::Value::Object(at_limit));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_suppression_cursor_is_opaque_and_import_errors_name_the_line(pool: PgPool) {
        let router = keyed_app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let csv = "kind,value,note

email,a@example.com,\"two
lines\"
email,nope,
email,b@example.com,
";

        let imported = call_text(
            &router,
            "POST",
            &suppressions_uri(&ws, "/import"),
            Some("text/csv"),
            csv.to_owned(),
            Auth::Session(&owner),
        )
        .await;
        let body: serde_json::Value = serde_json::from_str(&imported.text).unwrap();
        assert_eq!(
            body["invalid"],
            json!([{"row": 5, "reason": "invalid_email"}])
        );
        assert_eq!(body["added"], 2);

        let page = call(
            &router,
            "GET",
            &suppressions_uri(&ws, "?limit=1"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let cursor = page.body["next_before"].as_str().unwrap();
        let decoded = {
            use base64::Engine;
            String::from_utf8(
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(cursor)
                    .unwrap(),
            )
            .unwrap()
        };
        let hashes: Vec<Vec<u8>> = sqlx::query_scalar("SELECT identifier_hash FROM suppressions")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(hashes.len(), 2);
        for hash in hashes {
            let hex: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
            assert!(!decoded.contains(&hex) && !cursor.contains(&hex));
        }
        let next = call(
            &router,
            "GET",
            &suppressions_uri(&ws, &format!("?limit=1&before={cursor}")),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(next.body["suppressions"].as_array().unwrap().len(), 1);
        assert_eq!(next.body["next_before"], serde_json::Value::Null);
    }

    fn people_uri(ws: &str, rest: &str) -> String {
        format!("/api/v1/workspaces/{ws}/people{rest}")
    }

    /// Seeds, in `ws`, an identified person (email, a PII `city` trait and a plain `plan`
    /// trait) with an event, and an anonymous person merged into it. Returns the survivor and
    /// the merged person.
    async fn seed_profile(pool: &PgPool, ws: &str) -> (String, String) {
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        zirv_flow_store::create_trait_definition(
            pool,
            workspace,
            &zirv_flow_store::NewTraitDefinition {
                key: "city".into(),
                trait_type: zirv_flow_core::people::TraitType::String,
                precedence: zirv_flow_core::people::Precedence::Latest,
                source_order: Vec::new(),
                pii: true,
                channel_address: false,
                inferred: false,
                description: String::new(),
            },
        )
        .await
        .unwrap();
        let apply = |identifiers: &'static [(&'static str, &'static str)],
                     key: &'static str,
                     traits: Value| {
            let mut record = profile(key, identifiers);
            record.traits = traits.as_object().cloned().unwrap();
            async move {
                match zirv_flow_store::apply_profile_batch(pool, workspace, &[record])
                    .await
                    .unwrap()
                    .remove(0)
                {
                    zirv_flow_store::ProfileOutcome::Applied { person_id, .. }
                    | zirv_flow_store::ProfileOutcome::Unchanged { person_id, .. } => person_id,
                    other => panic!("not applied: {other:?}"),
                }
            }
        };
        let survivor = apply(
            &[("email", "ada@example.com")],
            "1",
            json!({"city": "Aarhus", "plan": "pro"}),
        )
        .await;
        let merged = apply(&[("anonymous_id", "anon-1")], "2", json!({})).await;
        let event = zirv_flow_store::EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), "ada@example.com".into())],
            name: "page_view".into(),
            properties: json!({"coupon": "SECRET-COUPON"})
                .as_object()
                .cloned()
                .unwrap(),
            timestamp: None,
            received_at: time::OffsetDateTime::now_utc(),
            message_id: None,
            trace: zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("seed")),
        };
        let key = zirv_flow_core::HmacRoot::bootstrap_from(&zirv_flow_core::MasterKey::generate())
            .hmac_key(zirv_flow_core::HmacPurpose::Redaction);
        zirv_flow_store::apply_event_batch(pool, workspace, &key, &[event])
            .await
            .unwrap();
        let both = apply(
            &[("email", "ada@example.com"), ("anonymous_id", "anon-1")],
            "3",
            json!({}),
        )
        .await;
        assert_eq!(both, survivor);
        (survivor.to_string(), merged.to_string())
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_profile_follows_a_merged_id_and_masks_values_for_members_and_read_keys(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let (_, admin_key) = make_key(&router, &owner, &ws, &["read", "admin"]).await;
        let (survivor, merged) = seed_profile(&pool, &ws).await;
        let uri = people_uri(&ws, &format!("/{merged}"));

        let as_owner = call(&router, "GET", &uri, None, Auth::Session(&owner)).await;
        let as_member = call(&router, "GET", &uri, None, Auth::Session(&member)).await;
        let as_read_key = call(&router, "GET", &uri, None, Auth::Key(&read_key)).await;
        let as_admin_key = call(&router, "GET", &uri, None, Auth::Key(&admin_key)).await;

        assert_eq!(as_owner.status, StatusCode::OK, "{:?}", as_owner.body);
        assert_eq!(as_owner.body["id"], survivor.as_str());
        assert_eq!(as_owner.body["merged_from"], merged.as_str());
        assert_eq!(as_owner.body["display"]["kind"], "email");
        assert_eq!(as_owner.body["display"]["value"], "ada@example.com");
        let city = &as_owner.body["traits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["key"] == "city")
            .unwrap();
        assert_eq!(city["value"], "Aarhus");
        assert_eq!(city["source"], "crm");
        assert_eq!(city["trait_type"], "string");
        assert_eq!(
            as_owner.body["merges"][0]["merged_person_id"],
            merged.as_str()
        );
        assert!(as_owner.body["consent"].is_array() && as_owner.body["lists"].is_array());
        assert_eq!(as_admin_key.body["display"]["value"], "ada@example.com");
        for masked in [&as_member, &as_read_key] {
            assert_eq!(masked.status, StatusCode::OK);
            let text = masked.body.to_string();
            assert!(!text.contains("ada@example.com"), "{text}");
            assert!(!text.contains("Aarhus"), "{text}");
            assert_eq!(masked.body["display"]["masked"], true);
            assert_eq!(masked.body["display"]["kind"], "email");
            let city = masked.body["traits"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["key"] == "city")
                .unwrap();
            assert_eq!(city["masked"], true);
            assert!(city["value"].is_null());
            let plan = masked.body["traits"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["key"] == "plan")
                .unwrap();
            assert_eq!(plan["value"], "pro");
        }
        let direct = call(
            &router,
            "GET",
            &people_uri(&ws, &format!("/{survivor}")),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert!(direct.body["merged_from"].is_null());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_person_of_another_workspace_is_a_404_for_the_profile_and_the_timeline(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let a = workspace(&router, &owner, "Alpha").await;
        let b = workspace(&router, &owner, "Beta").await;
        let (person, _) = seed_profile(&pool, &a).await;

        for rest in [format!("/{person}"), format!("/{person}/timeline")] {
            let foreign = call(
                &router,
                "GET",
                &people_uri(&b, &rest),
                None,
                Auth::Session(&owner),
            )
            .await;
            let unknown = call(
                &router,
                "GET",
                &people_uri(
                    &a,
                    &rest.replace(&person, &uuid::Uuid::now_v7().to_string()),
                ),
                None,
                Auth::Session(&owner),
            )
            .await;
            let malformed = call(
                &router,
                "GET",
                &people_uri(&a, &rest.replace(&person, "nope")),
                None,
                Auth::Session(&owner),
            )
            .await;
            assert_eq!(foreign.status, StatusCode::NOT_FOUND, "{rest}");
            assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{rest}");
            assert_eq!(malformed.status, StatusCode::NOT_FOUND, "{rest}");
        }
        let anonymous = call(
            &router,
            "GET",
            &people_uri(&a, &format!("/{person}")),
            None,
            Auth::Anonymous,
        )
        .await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn search_finds_by_identifier_and_trait_and_masks_the_display_for_members(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let (survivor, _) = seed_profile(&pool, &ws).await;
        let uri = people_uri(&ws, "/search");
        let by_identifier = json!({"identifier": {"kind": "anonymous_id", "value": "anon-1"}});
        let by_trait = json!({"trait": {"key": "plan", "op": "prefix", "value": "pr"}});

        let found = call(
            &router,
            "POST",
            &uri,
            Some(by_identifier.clone()),
            Auth::Session(&owner),
        )
        .await;
        let traits = call(
            &router,
            "POST",
            &uri,
            Some(by_trait.clone()),
            Auth::Session(&owner),
        )
        .await;
        let masked_member = call(
            &router,
            "POST",
            &uri,
            Some(by_identifier.clone()),
            Auth::Session(&member),
        )
        .await;
        let masked_key = call(&router, "POST", &uri, Some(by_trait), Auth::Key(&read_key)).await;

        assert_eq!(found.status, StatusCode::OK, "{:?}", found.body);
        assert_eq!(found.body["people"][0]["id"], survivor.as_str());
        assert_eq!(
            found.body["people"][0]["display"]["value"],
            "ada@example.com"
        );
        assert!(found.body["next_after"].is_null());
        assert_eq!(traits.body["people"][0]["id"], survivor.as_str());
        for masked in [&masked_member, &masked_key] {
            assert_eq!(masked.status, StatusCode::OK, "{:?}", masked.body);
            assert_eq!(masked.body["people"][0]["id"], survivor.as_str());
            assert_eq!(masked.body["people"][0]["display"]["masked"], true);
            assert!(!masked.body.to_string().contains("ada@example.com"));
        }
        let none = call(
            &router,
            "POST",
            &uri,
            Some(json!({"identifier": {"kind": "email", "value": "nobody@example.com"}})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(none.status, StatusCode::OK);
        assert_eq!(none.body["people"], json!([]));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn search_refuses_a_bad_request_per_field_without_echoing_the_value(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        seed_profile(&pool, &ws).await;
        let uri = people_uri(&ws, "/search");

        for body in [
            json!({}),
            json!({"identifier": {"kind": "email", "value": "a@example.com"}, "trait": {"key": "plan", "op": "exists"}}),
            json!({"identifier": {"kind": "fax", "value": "secret-value"}}),
            json!({"identifier": {"kind": "email", "value": "secret-value"}}),
            json!({"trait": {"key": "nope", "op": "exists"}}),
            json!({"trait": {"key": "plan", "op": "like", "value": "secret-value"}}),
            json!({"trait": {"key": "plan", "op": "gt", "value": "secret-value"}}),
            json!({"trait": {"key": "plan", "op": "eq"}}),
            json!({"trait": {"key": "plan", "op": "eq", "value": 5}}),
            json!({"identifier": {"kind": "email", "value": "a@example.com"}, "limit": 0}),
        ] {
            let reply = call(
                &router,
                "POST",
                &uri,
                Some(body.clone()),
                Auth::Session(&owner),
            )
            .await;
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{body}: {:?}",
                reply.body
            );
            assert!(!reply.body.to_string().contains("secret-value"));
        }
        let no_csrf = call(
            &router,
            "POST",
            &uri,
            Some(json!({"identifier": {"kind": "email", "value": "a@example.com"}})),
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;
        assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_timeline_interleaves_entries_links_traces_and_masks_event_values(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (survivor, merged) = seed_profile(&pool, &ws).await;
        let uri = people_uri(&ws, &format!("/{merged}/timeline"));

        let as_owner = call(&router, "GET", &uri, None, Auth::Session(&owner)).await;
        let as_member = call(&router, "GET", &uri, None, Auth::Session(&member)).await;

        assert_eq!(as_owner.status, StatusCode::OK, "{:?}", as_owner.body);
        let entries = as_owner.body["entries"].as_array().unwrap();
        let kinds: Vec<&str> = entries
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"event") && kinds.contains(&"decision"));
        let actions: Vec<&str> = entries
            .iter()
            .filter(|e| e["type"] == "decision")
            .map(|e| e["action"].as_str().unwrap())
            .collect();
        assert!(actions.contains(&"person.merge") && actions.contains(&"trait.change"));
        assert!(!actions.contains(&"event.accepted"));
        let times: Vec<&str> = entries.iter().map(|e| e["at"].as_str().unwrap()).collect();
        assert!(times.windows(2).all(|pair| pair[0] >= pair[1]), "{times:?}");
        let merge = entries
            .iter()
            .find(|e| e["action"] == "person.merge")
            .unwrap();
        assert_eq!(merge["subject_ids"]["person"], survivor.as_str());
        let event = entries.iter().find(|e| e["type"] == "event").unwrap();
        assert_eq!(event["name"], "page_view");
        assert_eq!(event["properties"]["coupon"], "SECRET-COUPON");
        for entry in entries {
            let trace = entry["trace_id"].as_str().unwrap();
            let opened = call(
                &router,
                "GET",
                &format!("/api/v1/workspaces/{ws}/traces/{trace}"),
                None,
                Auth::Session(&owner),
            )
            .await;
            assert_eq!(opened.status, StatusCode::OK, "{trace}: {:?}", opened.body);
            assert!(!opened.body.to_string().contains("SECRET-COUPON"));
        }
        let hidden_event = as_member.body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "event")
            .unwrap();
        assert!(hidden_event["properties"]["coupon"].is_null());
        assert_eq!(hidden_event["masked"], json!(["coupon"]));
        assert!(!as_member.body.to_string().contains("SECRET-COUPON"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_timeline_pages_by_cursor_filters_by_kind_and_refuses_bad_queries(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (survivor, _) = seed_profile(&pool, &ws).await;
        let timeline = |query: &str| people_uri(&ws, &format!("/{survivor}/timeline{query}"));
        let all = call(
            &router,
            "GET",
            &timeline("?limit=200"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let total = all.body["entries"].as_array().unwrap().len();
        assert!(total >= 4 && all.body["next_before"].is_null());

        let mut seen = Vec::new();
        let mut query = "?limit=2".to_owned();
        loop {
            let page = call(
                &router,
                "GET",
                &timeline(&query),
                None,
                Auth::Session(&owner),
            )
            .await;
            assert_eq!(page.status, StatusCode::OK, "{:?}", page.body);
            let entries = page.body["entries"].as_array().unwrap();
            assert!(entries.len() <= 2);
            seen.extend(entries.iter().map(|e| e["id"].as_str().unwrap().to_owned()));
            match page.body["next_before"].as_str() {
                Some(next) => query = format!("?limit=2&before={next}"),
                None => break,
            }
        }
        let expected: Vec<String> = all.body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(seen, expected);
        let events = call(
            &router,
            "GET",
            &timeline("?kinds=event"),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert!(
            events.body["entries"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["type"] == "event")
        );
        let decisions = call(
            &router,
            "GET",
            &timeline("?kinds=decision&all_decisions=true"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let actions: Vec<&str> = decisions.body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["action"].as_str().unwrap())
            .collect();
        assert!(actions.contains(&"event.accepted"));
        for bad in [
            "?limit=0",
            "?limit=201",
            "?before=%21",
            "?kinds=flow",
            "?all_decisions=maybe",
        ] {
            let reply = call(&router, "GET", &timeline(bad), None, Auth::Session(&owner)).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_pii_trait_cannot_be_searched_by_prefix_or_range_without_access_to_values(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let (survivor, _) = seed_profile(&pool, &ws).await;
        let uri = people_uri(&ws, "/search");
        let prefix = json!({"trait": {"key": "city", "op": "prefix", "value": "Aa"}});
        let eq = json!({"trait": {"key": "city", "op": "eq", "value": "Aarhus"}});

        let as_owner = call(
            &router,
            "POST",
            &uri,
            Some(prefix.clone()),
            Auth::Session(&owner),
        )
        .await;
        let as_member = call(
            &router,
            "POST",
            &uri,
            Some(prefix.clone()),
            Auth::Session(&member),
        )
        .await;
        let as_key = call(&router, "POST", &uri, Some(prefix), Auth::Key(&read_key)).await;
        let member_eq = call(&router, "POST", &uri, Some(eq), Auth::Session(&member)).await;

        assert_eq!(as_owner.status, StatusCode::OK, "{:?}", as_owner.body);
        assert_eq!(as_owner.body["people"][0]["id"], survivor.as_str());
        for refused in [&as_member, &as_key] {
            assert_eq!(refused.status, StatusCode::FORBIDDEN, "{:?}", refused.body);
            assert_eq!(refused.body["code"], "pii_trait_not_searchable");
            assert_eq!(
                refused.headers[header::CONTENT_TYPE],
                "application/problem+json"
            );
        }
        assert_eq!(member_eq.status, StatusCode::OK);
        assert_eq!(member_eq.body["people"][0]["id"], survivor.as_str());
    }

    fn privacy_uri(ws: &str, rest: &str) -> String {
        format!("/api/v1/workspaces/{ws}/privacy{rest}")
    }

    /// A GET whose reply is bytes, such as the gzip of an export.
    async fn call_bytes(
        router: &Router,
        uri: &str,
        auth: Auth<'_>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let request = with_auth(Request::builder().method("GET").uri(uri), auth)
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, bytes.to_vec())
    }

    async fn audit_actions(pool: &PgPool) -> Vec<String> {
        zirv_flow_store::list_audit(pool, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.action)
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_a_person_is_204_then_404_audited_with_the_id_only_and_admin_only(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let (_, write_key) = make_key(&router, &owner, &ws, &["write"]).await;
        let person = seed_person(&pool, &ws, "ada@example.com").await;
        let uri = format!("/api/v1/workspaces/{ws}/people/{person}");

        for auth in [
            Auth::Session(&member),
            Auth::Key(&read_key),
            Auth::Key(&write_key),
            Auth::SessionWithoutCsrf(&owner),
        ] {
            let refused = call(&router, "DELETE", &uri, None, auth).await;
            assert_eq!(refused.status, StatusCode::FORBIDDEN);
        }
        let anonymous = call(&router, "DELETE", &uri, None, Auth::Anonymous).await;
        let first = call(&router, "DELETE", &uri, None, Auth::Session(&owner)).await;
        let second = call(&router, "DELETE", &uri, None, Auth::Session(&owner)).await;

        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(first.status, StatusCode::NO_CONTENT);
        assert_eq!(second.status, StatusCode::NOT_FOUND);
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM people")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
        let audit = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        let deleted: Vec<_> = audit
            .iter()
            .filter(|entry| entry.action == "person.deleted")
            .collect();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].object_id.map(|id| id.to_string()), Some(person));
        assert_eq!(deleted[0].summary, json!({}));
        // Deletion is not erasure: no suppression and no ledger entry.
        let kept: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM suppressions) + (SELECT count(*) FROM erasure_requests)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kept, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erasure_request_by_identifier_creates_a_ledger_row_and_a_job_and_the_job_erases(
        pool: PgPool,
    ) {
        let router = keyed_app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let person = seed_person(&pool, &ws, "ada@example.com").await;
        let uri = privacy_uri(&ws, "/erasures");

        let by_identifier = call(
            &router,
            "POST",
            &uri,
            Some(json!({"identifier": {"kind": "email", "value": " Ada@Example.com "}})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(
            by_identifier.status,
            StatusCode::ACCEPTED,
            "{:?}",
            by_identifier.body
        );
        assert_eq!(by_identifier.body["status"], "pending");
        assert_eq!(by_identifier.body["person_id"], person.as_str());
        assert_eq!(by_identifier.body["trace_id"].as_str().unwrap().len(), 32);
        let request = by_identifier.body["id"].as_str().unwrap().to_owned();
        let ledger: (i64, i32) = sqlx::query_as(
            "SELECT count(*), max(cardinality(identifier_hashes)) FROM erasure_requests",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ledger, (1, 1));
        let jobs: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT kind, unique_key FROM jobs WHERE kind = 'privacy.erase'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            jobs,
            [(
                "privacy.erase".to_owned(),
                Some(format!("privacy.erase:{request}"))
            )]
        );
        assert!(
            audit_actions(&pool)
                .await
                .contains(&"privacy.erasure_requested".to_owned())
        );

        // Run the job's work, then read the request back.
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        zirv_flow_store::erase_person(
            &mut tx,
            ws.parse().unwrap(),
            request.parse().unwrap(),
            &zirv_flow_core::HmacRoot::bootstrap_from(
                &zirv_flow_core::MasterKey::parse(SUPPRESSION_KEY).unwrap(),
            )
            .hmac_key(zirv_flow_core::HmacPurpose::Identifier),
            &zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t")),
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let listed = call(&router, "GET", &uri, None, Auth::Session(&owner)).await;
        assert_eq!(listed.status, StatusCode::OK);
        assert_eq!(listed.body["erasures"][0]["status"], "completed");
        assert!(listed.body["erasures"][0]["completed_at"].is_string());
        assert!(!listed.body.to_string().contains("ada@example.com"));
        assert!(!listed.body.to_string().contains("identifier_hash"));
        let people: i64 = sqlx::query_scalar("SELECT count(*) FROM people")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(people, 0);
        let suppressed: Vec<String> = sqlx::query_scalar("SELECT reason FROM suppressions")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(suppressed, ["erased"]);
        let entry = zirv_flow_store::list_audit(&pool, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.action == "privacy.erased")
            .unwrap();
        assert_eq!(entry.object_id.map(|id| id.to_string()), Some(request));
        assert_eq!(entry.summary, json!({}));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn erasure_requests_resolve_the_person_check_the_body_and_follow_the_admin_rules(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (_, admin_key) = make_key(&router, &owner, &ws, &["admin"]).await;
        let (_, write_key) = make_key(&router, &owner, &ws, &["write"]).await;
        let person = seed_person(&pool, &ws, "ada@example.com").await;
        let uri = privacy_uri(&ws, "/erasures");
        let body = json!({"person_id": person});

        for auth in [
            Auth::Session(&member),
            Auth::Key(&write_key),
            Auth::SessionWithoutCsrf(&owner),
        ] {
            let refused = call(&router, "POST", &uri, Some(body.clone()), auth).await;
            assert_eq!(refused.status, StatusCode::FORBIDDEN);
        }
        for auth in [Auth::Session(&member), Auth::Key(&write_key)] {
            let refused = call(&router, "GET", &uri, None, auth).await;
            assert_eq!(refused.status, StatusCode::FORBIDDEN);
        }
        let unknown_identifier = call(
            &router,
            "POST",
            &uri,
            Some(json!({"identifier": {"kind": "email", "value": "nobody@example.com"}})),
            Auth::Session(&owner),
        )
        .await;
        let unknown_person = call(
            &router,
            "POST",
            &uri,
            Some(json!({"person_id": "00000000-0000-7000-8000-000000000000"})),
            Auth::Session(&owner),
        )
        .await;
        let both = call(
            &router,
            "POST",
            &uri,
            Some(json!({"person_id": person, "identifier": {"kind": "email", "value": "a@b.c"}})),
            Auth::Session(&owner),
        )
        .await;
        let neither = call(
            &router,
            "POST",
            &uri,
            Some(json!({})),
            Auth::Session(&owner),
        )
        .await;
        let by_key = call(
            &router,
            "POST",
            &uri,
            Some(body.clone()),
            Auth::Key(&admin_key),
        )
        .await;

        assert_eq!(unknown_identifier.status, StatusCode::NOT_FOUND);
        assert_eq!(unknown_person.status, StatusCode::NOT_FOUND);
        assert_eq!(both.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(neither.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(by_key.status, StatusCode::ACCEPTED, "{:?}", by_key.body);
        assert_eq!(by_key.body["requested_by"], Value::Null);
        let page = call(
            &router,
            "GET",
            &format!("{uri}?limit=1"),
            None,
            Auth::Key(&admin_key),
        )
        .await;
        assert_eq!(page.status, StatusCode::OK);
        assert_eq!(page.body["erasures"].as_array().unwrap().len(), 1);
        let bad_limit = call(
            &router,
            "GET",
            &format!("{uri}?limit=0"),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(bad_limit.status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn erasure_is_unavailable_without_a_master_key(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let router = router(AppState::new(pool.clone(), Arc::new(String::new)));
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let person = seed_person(&pool, &ws, "ada@example.com").await;

        let reply = call(
            &router,
            "POST",
            &privacy_uri(&ws, "/erasures"),
            Some(json!({"person_id": person})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_export_is_requested_built_downloaded_and_audited_and_is_gone_after_expiry(
        pool: PgPool,
    ) {
        use std::io::Read;

        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let person = seed_person(&pool, &ws, "ada@example.com").await;
        let uri = privacy_uri(&ws, "/exports");

        let requested = call(
            &router,
            "POST",
            &uri,
            Some(json!({"person_id": person})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(
            requested.status,
            StatusCode::ACCEPTED,
            "{:?}",
            requested.body
        );
        assert_eq!(requested.body["status"], "pending");
        let export = requested.body["id"].as_str().unwrap().to_owned();
        let download_uri = format!("{uri}/{export}/download");
        let pending = call_bytes(&router, &download_uri, Auth::Session(&owner)).await;
        assert_eq!(pending.0, StatusCode::CONFLICT);
        let jobs: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'privacy.export'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(jobs, 1);

        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        zirv_flow_store::build_export(&mut tx, ws.parse().unwrap(), export.parse().unwrap())
            .await
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();

        let listed = call(&router, "GET", &uri, None, Auth::Session(&owner)).await;
        assert_eq!(listed.body["exports"][0]["status"], "ready");
        assert!(listed.body["exports"][0]["expires_at"].is_string());
        assert!(listed.body["exports"][0].get("content").is_none());
        for auth in [Auth::Session(&member), Auth::Key(&read_key)] {
            let refused = call_bytes(&router, &download_uri, auth).await;
            assert_eq!(refused.0, StatusCode::FORBIDDEN);
        }
        let (status, headers, bytes) =
            call_bytes(&router, &download_uri, Auth::Session(&owner)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/gzip");
        assert!(
            headers[header::CONTENT_DISPOSITION]
                .to_str()
                .unwrap()
                .contains(&format!("person-export-{export}.json.gz"))
        );
        let mut text = String::new();
        flate2::read::GzDecoder::new(&bytes[..])
            .read_to_string(&mut text)
            .unwrap();
        let document: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(document["format"], "zirv-flow.person-export/v1");
        assert_eq!(document["person"]["id"], person.as_str());
        assert!(text.contains("ada@example.com"));
        let actions = audit_actions(&pool).await;
        for action in ["privacy.export_requested", "privacy.export_downloaded"] {
            assert!(
                actions.contains(&action.to_owned()),
                "{action}: {actions:?}"
            );
        }
        let audited = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        assert!(
            audited
                .iter()
                .all(|entry| !entry.summary.to_string().contains("ada@"))
        );

        sqlx::query("UPDATE privacy_exports SET expires_at = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        let gone = call_bytes(&router, &download_uri, Auth::Session(&owner)).await;
        let unknown = call_bytes(
            &router,
            &format!("{uri}/00000000-0000-7000-8000-000000000000/download"),
            Auth::Session(&owner),
        )
        .await;
        let listed = call(&router, "GET", &uri, None, Auth::Session(&owner)).await;
        assert_eq!(gone.0, StatusCode::GONE);
        assert_eq!(unknown.0, StatusCode::NOT_FOUND);
        assert_eq!(listed.body["exports"][0]["status"], "expired");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn export_requests_follow_the_admin_rules_and_resolve_identifiers(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let person = seed_person(&pool, &ws, "ada@example.com").await;
        let uri = privacy_uri(&ws, "/exports");

        let refused = call(
            &router,
            "POST",
            &uri,
            Some(json!({"person_id": person})),
            Auth::Session(&member),
        )
        .await;
        let listing = call(&router, "GET", &uri, None, Auth::Session(&member)).await;
        let missing = call(
            &router,
            "POST",
            &uri,
            Some(json!({"identifier": {"kind": "email", "value": "nobody@example.com"}})),
            Auth::Session(&owner),
        )
        .await;
        let found = call(
            &router,
            "POST",
            &uri,
            Some(json!({"identifier": {"kind": "email", "value": "ada@example.com"}})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(listing.status, StatusCode::FORBIDDEN);
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        assert_eq!(found.status, StatusCode::ACCEPTED);
        assert_eq!(found.body["person_id"], person.as_str());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_orphan_policy_is_read_by_members_and_set_by_admins_and_audited(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let uri = privacy_uri(&ws, "/settings");

        let default = call(&router, "GET", &uri, None, Auth::Session(&member)).await;
        let refused = call(
            &router,
            "PUT",
            &uri,
            Some(json!({"orphan_policy": "delete"})),
            Auth::Session(&member),
        )
        .await;
        let invalid = call(
            &router,
            "PUT",
            &uri,
            Some(json!({"orphan_policy": "purge"})),
            Auth::Session(&owner),
        )
        .await;
        let saved = call(
            &router,
            "PUT",
            &uri,
            Some(json!({"orphan_policy": "delete"})),
            Auth::Session(&owner),
        )
        .await;
        let again = call(&router, "GET", &uri, None, Auth::Session(&member)).await;

        assert_eq!(default.body["orphan_policy"], "keep");
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(saved.body["orphan_policy"], "delete");
        assert_eq!(again.body["orphan_policy"], "delete");
        let entry = zirv_flow_store::list_audit(&pool, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.action == "workspace.privacy_settings_updated")
            .unwrap();
        assert_eq!(entry.summary, json!({"orphan_policy": "delete"}));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn replaying_the_ledger_through_the_admin_operation_audits_counts_only(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let key = zirv_flow_core::HmacRoot::bootstrap_from(
            &zirv_flow_core::MasterKey::parse(SUPPRESSION_KEY).unwrap(),
        );

        let none = admin::replay_erasures(&pool, &Context::system(), Some("alpha"), &key)
            .await
            .unwrap();
        let missing = admin::replay_erasures(&pool, &Context::system(), Some("nope"), &key).await;

        assert_eq!(none, zirv_flow_store::ReplayReport::default());
        assert!(matches!(missing, Err(admin::AdminError::NotFound(_))));
        let entry = zirv_flow_store::list_audit(&pool, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.action == "privacy.erasures_replayed")
            .unwrap();
        assert_eq!(entry.workspace_id.map(|id| id.to_string()), Some(ws));
        assert_eq!(
            entry.summary,
            json!({"requests": 0, "suppressions": 0, "people": 0, "inbox_rows": 0})
        );
    }

    fn imports_uri(ws: &str, rest: &str) -> String {
        format!("/api/v1/workspaces/{ws}/imports{rest}")
    }

    const CUSTOMERS: &str = "Email,Plan,Newsletter\n\
                             ada@example.com,pro,yes\n\
                             bob@example.com,free,no\n\
                             not-an-email,free,yes\n";

    /// Uploads `csv` and returns the reply, its body parsed.
    async fn upload_csv(
        router: &Router,
        ws: &str,
        query: &str,
        csv: &str,
        auth: Auth<'_>,
    ) -> (StatusCode, Value) {
        let reply = call_text(
            router,
            "POST",
            &imports_uri(ws, &format!("?{query}")),
            Some("text/csv"),
            csv.to_owned(),
            auth,
        )
        .await;
        let body = serde_json::from_str(&reply.text).unwrap_or(Value::Null);
        (reply.status, body)
    }

    async fn mapped_import(router: &Router, owner: &Login, ws: &str) -> String {
        let (status, body) = upload_csv(
            router,
            ws,
            "name=Customers&file_name=customers.csv",
            CUSTOMERS,
            Auth::Session(owner),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body:?}");
        let id = body["id"].as_str().unwrap().to_owned();
        let reply = call(
            router,
            "PUT",
            &imports_uri(ws, &format!("/{id}/mapping")),
            Some(json!({"columns": [
                {"column": "Email", "target": {"type": "identifier", "kind": "email"},
                 "transforms": [{"op": "trim"}, {"op": "lower"}]}
            ]})),
            Auth::Session(owner),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_upload_stores_the_file_and_shows_its_header_and_first_rows(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;

        let (status, body) = upload_csv(
            &router,
            &ws,
            "name=Customers%20May&file_name=Customers%20May.csv",
            "\u{feff}Email;Plan\r\nada@example.com;pro\r\n\"b;ob@example.com\";free\r\n",
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(status, StatusCode::CREATED, "{body:?}");
        assert_eq!(body["status"], "uploaded");
        assert_eq!(body["header"], json!(["Email", "Plan"]));
        assert_eq!(body["delimiter"], ";");
        assert_eq!(body["rows_total"], 2);
        assert_eq!(body["source"], "csv");
        assert_eq!(body["stream"], "customers-may");
        assert_eq!(
            body["samples"],
            json!([["ada@example.com", "pro"], ["b;ob@example.com", "free"]])
        );
        assert_eq!(body["samples_masked"], false);
        let stored: Vec<u8> = sqlx::query_scalar("SELECT data FROM import_chunks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(stored.starts_with(b"\xEF\xBB\xBFEmail;Plan"));
        let hash: String = zirv_flow_core::token::sha256(std::str::from_utf8(&stored).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(body["file_sha256"], hash.as_str());
        let audit: Vec<(String, Value)> =
            sqlx::query_as("SELECT action, summary FROM audit_log WHERE action LIKE 'import.%'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].0, "import.created");
        assert!(!audit[0].1.to_string().contains("ada@"));
        assert_eq!(audit[0].1["rows"], 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_sample_rows_are_masked_for_a_member_who_may_not_see_values(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;

        let (status, body) = upload_csv(
            &router,
            &ws,
            "name=Customers",
            CUSTOMERS,
            Auth::Session(&member),
        )
        .await;
        let again = call(
            &router,
            "GET",
            &imports_uri(
                &ws,
                &format!("/{}?samples=true", body["id"].as_str().unwrap()),
            ),
            None,
            Auth::Session(&member),
        )
        .await;

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["samples_masked"], true);
        assert_eq!(body["samples"][0], json!([null, null, null]));
        assert_eq!(again.body["samples_masked"], true);
        assert_eq!(again.body["samples"].as_array().unwrap().len(), 3);
        assert!(!again.body.to_string().contains("ada@example.com"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_poll_shows_the_samples_only_when_asked_for(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let id = mapped_import(&router, &owner, &ws).await;

        let plain = call(
            &router,
            "GET",
            &imports_uri(&ws, &format!("/{id}")),
            None,
            Auth::Session(&owner),
        )
        .await;
        let with = call(
            &router,
            "GET",
            &imports_uri(&ws, &format!("/{id}?samples=true")),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(plain.body["samples"], json!([]));
        assert_eq!(with.body["samples"][0][0], "ada@example.com");
        assert_eq!(with.body["status"], "mapped");
        assert_eq!(with.body["mapping"]["columns"][0]["column"], "Email");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_file_over_the_limit_is_a_413_whether_declared_or_streamed_and_keeps_nothing(
        pool: PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let router = router(
            AppState::new(pool.clone(), Arc::new(String::new)).with_import_limits(
                crate::ImportLimits {
                    max_file_bytes: 64,
                    ..crate::ImportLimits::default()
                },
            ),
        );
        admin::create_owner(&pool, &Context::system(), OWNER_EMAIL, PASSWORD)
            .await
            .unwrap();
        let owner = login(&router, OWNER_EMAIL).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let big = format!("Email\n{}", "ada@example.com\n".repeat(10));

        let (declared, _) = upload_csv(&router, &ws, "name=x", &big, Auth::Session(&owner)).await;
        let streamed = {
            let pieces: Vec<Result<Vec<u8>, std::io::Error>> = big
                .as_bytes()
                .chunks(20)
                .map(|piece| Ok(piece.to_vec()))
                .collect();
            let request = Request::builder()
                .method("POST")
                .uri(imports_uri(&ws, "?name=x"))
                .header(header::CONTENT_TYPE, "text/csv")
                .header(header::COOKIE, &owner.cookie)
                .header("x-csrf-token", &owner.csrf)
                .body(Body::from_stream(futures_util::stream::iter(pieces)))
                .unwrap();
            router.clone().oneshot(request).await.unwrap().status()
        };
        let (fits, _) = upload_csv(
            &router,
            &ws,
            "name=x",
            "Email\na@example.com\n",
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(declared, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(streamed, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(fits, StatusCode::CREATED);
        let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM imports")
            .fetch_one(&pool)
            .await
            .unwrap();
        let chunks: i64 = sqlx::query_scalar("SELECT count(*) FROM import_chunks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((kept, chunks), (1, 1), "only the file that fit was kept");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_bad_file_is_refused_and_leaves_nothing_behind(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let post = |query: &'static str, content_type: Option<&'static str>, body: Vec<u8>| {
            let (router, owner, ws) = (router.clone(), &owner, ws.clone());
            async move {
                let reply = call_text_bytes(
                    &router,
                    &imports_uri(&ws, &format!("?{query}")),
                    content_type,
                    body,
                    Auth::Session(owner),
                )
                .await;
                (reply.0, reply.1)
            }
        };

        let duplicate = post("name=x", Some("text/csv"), b"A,B,A\n1,2,3\n".to_vec()).await;
        let blank = post("name=x", Some("text/csv"), b"A,,C\n1,2,3\n".to_vec()).await;
        let not_utf8 = post("name=x", Some("text/csv"), b"A\n\xFF\xFE\n".to_vec()).await;
        let empty = post("name=x", Some("text/csv"), b"\n\n".to_vec()).await;
        let wrong_type = post("name=x", Some("application/json"), b"{}".to_vec()).await;
        let no_type = post("name=x", None, b"A\n1\n".to_vec()).await;
        let no_name = post("", Some("text/csv"), b"A\n1\n".to_vec()).await;
        let bad_delimiter = post("name=x&delimiter=pipe", Some("text/csv"), b"A\n".to_vec()).await;

        assert_eq!(duplicate.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(duplicate.1.contains("unique"), "{}", duplicate.1);
        assert_eq!(blank.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(not_utf8.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(empty.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(wrong_type.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(no_type.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(no_name.0, StatusCode::BAD_REQUEST);
        assert_eq!(bad_delimiter.0, StatusCode::BAD_REQUEST);
        for table in ["imports", "import_chunks"] {
            let rows: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(rows, 0, "{table}");
        }
    }

    /// An upload with an optional content type, as status and body text.
    async fn call_text_bytes(
        router: &Router,
        uri: &str,
        content_type: Option<&str>,
        body: Vec<u8>,
        auth: Auth<'_>,
    ) -> (StatusCode, String) {
        let reply = call_raw(router, uri, content_type, body, auth).await;
        (reply.status, reply.body.to_string())
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_mapping_is_validated_against_the_header_the_registry_and_the_lists(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (_, body) = upload_csv(
            &router,
            &ws,
            "name=Customers",
            CUSTOMERS,
            Auth::Session(&owner),
        )
        .await;
        let id = body["id"].as_str().unwrap();
        let uri = imports_uri(&ws, &format!("/{id}/mapping"));
        let email = json!({"column": "Email", "target": {"type": "identifier", "kind": "email"}});
        let put = |columns: Value| {
            let (router, owner, uri) = (router.clone(), &owner, uri.clone());
            async move {
                call(
                    &router,
                    "PUT",
                    &uri,
                    Some(json!({ "columns": columns })),
                    Auth::Session(owner),
                )
                .await
            }
        };
        call(
            &router,
            "POST",
            &format!("/api/v1/workspaces/{ws}/traits"),
            Some(json!({"key": "plan", "type": "string"})),
            Auth::Session(&owner),
        )
        .await;

        let none = put(json!([])).await;
        let ghost = put(json!([email, {"column": "Ghost", "target": {"type": "ignore"}}])).await;
        let unknown_trait = put(json!([
            email,
            {"column": "Plan", "target": {"type": "trait", "key": "nope"}}
        ]))
        .await;
        let unknown_list = put(json!([
            email,
            {"column": "Newsletter",
             "target": {"type": "list", "list_id": "00000000-0000-7000-8000-000000000000"}}
        ]))
        .await;
        let bad_transform = put(json!([{
            "column": "Email", "target": {"type": "identifier", "kind": "email"},
            "transforms": [{"op": "timestamp", "format": "%Q"}]
        }]))
        .await;
        let script = put(json!([{
            "column": "Email", "target": {"type": "identifier", "kind": "email"},
            "transforms": [{"op": "eval", "code": "1"}]
        }]))
        .await;
        let ok = put(json!([
            email,
            {"column": "Plan", "target": {"type": "trait", "key": "plan"}}
        ]))
        .await;

        assert_eq!(none.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(none.body["detail"], "mapping: no_identifier");
        assert_eq!(ghost.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            ghost.body["detail"],
            "mapping: unknown_column (column \"Ghost\")"
        );
        assert_eq!(
            unknown_trait.body["detail"],
            "mapping: unknown_trait (column \"Plan\")"
        );
        assert_eq!(
            unknown_list.body["detail"],
            "mapping: unknown_list (column \"Newsletter\")"
        );
        assert_eq!(bad_transform.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(script.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(ok.status, StatusCode::OK, "{:?}", ok.body);
        assert_eq!(ok.body["status"], "mapped");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_pass_needs_a_mapping_and_a_run_is_audited_and_queued_once(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (_, body) = upload_csv(
            &router,
            &ws,
            "name=Customers",
            CUSTOMERS,
            Auth::Session(&owner),
        )
        .await;
        let id = body["id"].as_str().unwrap().to_owned();
        let post = |action: &str| {
            let (router, owner) = (router.clone(), &owner);
            let uri = imports_uri(&ws, &format!("/{id}/{action}"));
            async move { call(&router, "POST", &uri, None, Auth::Session(owner)).await }
        };

        let unmapped_dry = post("dry-run").await;
        let unmapped_run = post("run").await;
        call(
            &router,
            "PUT",
            &imports_uri(&ws, &format!("/{id}/mapping")),
            Some(json!({"columns": [
                {"column": "Email", "target": {"type": "identifier", "kind": "email"}}]})),
            Auth::Session(&owner),
        )
        .await;
        let started = post("run").await;
        let twice = post("run").await;
        let dry_while_running = post("dry-run").await;
        let remap = call(
            &router,
            "PUT",
            &imports_uri(&ws, &format!("/{id}/mapping")),
            Some(json!({"columns": [
                {"column": "Email", "target": {"type": "identifier", "kind": "email"}}]})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(unmapped_dry.status, StatusCode::CONFLICT);
        assert_eq!(unmapped_run.status, StatusCode::CONFLICT);
        assert_eq!(started.status, StatusCode::ACCEPTED, "{:?}", started.body);
        assert_eq!(started.body["status"], "running");
        assert!(started.body["started_at"].is_string());
        assert_eq!(twice.status, StatusCode::CONFLICT);
        assert_eq!(dry_while_running.status, StatusCode::CONFLICT);
        assert_eq!(remap.status, StatusCode::CONFLICT);
        let jobs: Vec<(String, Value, Option<String>)> =
            sqlx::query_as("SELECT kind, payload, unique_key FROM jobs WHERE kind LIKE 'import.%'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].0, "import.run");
        assert_eq!(jobs[0].1["import_id"], id.as_str());
        assert_eq!(
            jobs[0].2.as_deref(),
            Some(format!("import.run:{id}").as_str())
        );
        let audit: Vec<(String, Value)> = sqlx::query_as(
            "SELECT action, summary FROM audit_log WHERE action LIKE 'import.%' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let actions: Vec<&str> = audit.iter().map(|(action, _)| action.as_str()).collect();
        assert_eq!(actions, ["import.created", "import.run_started"]);
        assert_eq!(audit[1].1, json!({"rows": 3}));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dry_run_is_queued_and_a_poll_follows_the_counters(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let id = mapped_import(&router, &owner, &ws).await;

        let started = call(
            &router,
            "POST",
            &imports_uri(&ws, &format!("/{id}/dry-run")),
            None,
            Auth::Session(&owner),
        )
        .await;
        sqlx::query(
            "UPDATE imports SET rows_done = 2, rows_applied = 1, rows_rejected = 1 WHERE id = $1",
        )
        .bind(id.parse::<uuid::Uuid>().unwrap())
        .execute(&pool)
        .await
        .unwrap();
        let polled = call(
            &router,
            "GET",
            &imports_uri(&ws, &format!("/{id}")),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(started.status, StatusCode::ACCEPTED);
        assert_eq!(started.body["status"], "dry_running");
        assert_eq!(polled.status, StatusCode::OK);
        assert_eq!(
            (
                &polled.body["rows_total"],
                &polled.body["rows_done"],
                &polled.body["rows_applied"],
                &polled.body["rows_rejected"]
            ),
            (&json!(3), &json!(2), &json!(1), &json!(1))
        );
        let kind: String = sqlx::query_scalar("SELECT kind FROM jobs WHERE kind LIKE 'import.%'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kind, "import.dry_run");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_running_import_can_be_cancelled_once_and_the_job_stops_at_its_checkpoint(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let id = mapped_import(&router, &owner, &ws).await;
        call(
            &router,
            "POST",
            &imports_uri(&ws, &format!("/{id}/run")),
            None,
            Auth::Session(&owner),
        )
        .await;

        let cancelled = call(
            &router,
            "POST",
            &imports_uri(&ws, &format!("/{id}/cancel")),
            None,
            Auth::Session(&owner),
        )
        .await;
        let again = call(
            &router,
            "POST",
            &imports_uri(&ws, &format!("/{id}/cancel")),
            None,
            Auth::Session(&owner),
        )
        .await;
        let run_again = call(
            &router,
            "POST",
            &imports_uri(&ws, &format!("/{id}/run")),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(cancelled.status, StatusCode::OK);
        assert_eq!(cancelled.body["status"], "cancelled");
        assert!(cancelled.body["finished_at"].is_string());
        assert_eq!(again.status, StatusCode::CONFLICT);
        assert_eq!(run_again.status, StatusCode::CONFLICT);
        let actions: Vec<String> = sqlx::query_scalar(
            "SELECT action FROM audit_log WHERE action LIKE 'import.%' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            actions,
            ["import.created", "import.run_started", "import.cancelled"]
        );
        let import = zirv_flow_store::find_import(&pool, ws.parse().unwrap(), id.parse().unwrap())
            .await
            .unwrap()
            .unwrap();
        let outcome = zirv_flow_store::commit_import_batch(
            &pool,
            &zirv_flow_store::ImportBatch {
                import_id: import.id,
                workspace: ws.parse().unwrap(),
                expected_done: 0,
                records: &[],
                details: &[],
                rejected: &[],
                trace: &zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t")),
            },
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome, zirv_flow_store::BatchOutcome::Superseded);
    }

    /// Writes rejects the way a run does, through the store.
    async fn seed_rejects(pool: &PgPool, ws: &str, id: &str) {
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        zirv_flow_store::start_import_run(&mut tx, ws.parse().unwrap(), id.parse().unwrap())
            .await
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let rejected = [
            zirv_flow_store::RejectedRow {
                row_number: 4,
                reasons: vec![
                    zirv_flow_store::RejectReason {
                        column: "Email".into(),
                        reason: "invalid_identifier",
                    },
                    zirv_flow_store::RejectReason {
                        column: "Plan".into(),
                        reason: "wrong_type",
                    },
                ],
                raw: vec!["not-an-email".into(), "=cmd".into(), "yes".into()],
            },
            zirv_flow_store::RejectedRow {
                row_number: 6,
                reasons: vec![zirv_flow_store::RejectReason {
                    column: "Email".into(),
                    reason: "missing_identifier",
                }],
                raw: vec![String::new(), "x".into(), "no".into()],
            },
        ];
        zirv_flow_store::commit_import_batch(
            pool,
            &zirv_flow_store::ImportBatch {
                import_id: id.parse().unwrap(),
                workspace: ws.parse().unwrap(),
                expected_done: 0,
                records: &[],
                details: &[],
                rejected: &rejected,
                trace: &zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t")),
            },
            false,
        )
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn rejects_list_codes_and_the_csv_holds_cells_for_admins_only(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let id = mapped_import(&router, &owner, &ws).await;
        seed_rejects(&pool, &ws, &id).await;

        let json_page = call(
            &router,
            "GET",
            &imports_uri(&ws, &format!("/{id}/rejects?limit=1")),
            None,
            Auth::Session(&member),
        )
        .await;
        let next = call(
            &router,
            "GET",
            &imports_uri(
                &ws,
                &format!(
                    "/{id}/rejects?limit=1&after={}",
                    json_page.body["next_after"]
                ),
            ),
            None,
            Auth::Session(&member),
        )
        .await;
        let admin_csv = call_text(
            &router,
            "GET",
            &imports_uri(&ws, &format!("/{id}/rejects.csv")),
            None,
            String::new(),
            Auth::Session(&owner),
        )
        .await;
        let member_csv = call_text(
            &router,
            "GET",
            &imports_uri(&ws, &format!("/{id}/rejects.csv?phase=run")),
            None,
            String::new(),
            Auth::Session(&member),
        )
        .await;

        assert_eq!(json_page.status, StatusCode::OK);
        assert_eq!(
            json_page.body["rejects"],
            json!([{"row_number": 4, "phase": "run", "errors": [
                {"column": "Email", "reason": "invalid_identifier"},
                {"column": "Plan", "reason": "wrong_type"}]}])
        );
        assert_eq!(json_page.body["next_after"], 4);
        assert_eq!(next.body["rejects"][0]["row_number"], 6);
        assert_eq!(next.body["next_after"], Value::Null);
        assert!(!json_page.body.to_string().contains("not-an-email"));
        assert_eq!(admin_csv.status, StatusCode::OK);
        assert_eq!(
            admin_csv.headers[header::CONTENT_TYPE],
            "text/csv; charset=utf-8"
        );
        assert_eq!(
            admin_csv.text,
            "row_number,column,reason,Email,Plan,Newsletter\n\
             4,Email,invalid_identifier,not-an-email,'=cmd,yes\n\
             4,Plan,wrong_type\n\
             6,Email,missing_identifier,,x,no\n"
        );
        assert_eq!(member_csv.status, StatusCode::OK);
        assert_eq!(
            member_csv.text,
            "row_number,column,reason\n\
             4,Email,invalid_identifier\n\
             4,Plan,wrong_type\n\
             6,Email,missing_identifier\n"
        );
        let audit: Vec<Value> = sqlx::query_scalar(
            "SELECT summary FROM audit_log WHERE action = 'import.rejects_downloaded' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(audit.len(), 2);
        assert_eq!(audit[0]["with_cells"], true);
        assert_eq!(audit[1]["with_cells"], false);
        assert_eq!(audit[1]["phase"], "run");
        assert!(!audit[0].to_string().contains("not-an-email"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn read_keys_read_and_cannot_change_and_other_workspaces_see_nothing(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let other = workspace(&router, &owner, "Beta").await;
        let id = mapped_import(&router, &owner, &ws).await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let (_, write_key) = make_key(&router, &owner, &ws, &["write"]).await;
        let (_, admin_key) = make_key(&router, &owner, &ws, &["read", "admin"]).await;
        let mapping = json!({"columns": [
            {"column": "Email", "target": {"type": "identifier", "kind": "email"}}]});

        let mut forbidden = Vec::new();
        for (method, path, body) in [
            ("PUT", format!("/{id}/mapping"), Some(mapping.clone())),
            ("POST", format!("/{id}/dry-run"), None),
            ("POST", format!("/{id}/run"), None),
            ("POST", format!("/{id}/cancel"), None),
        ] {
            let reply = call(
                &router,
                method,
                &imports_uri(&ws, &path),
                body,
                Auth::Key(&read_key),
            )
            .await;
            forbidden.push(reply.status);
        }
        let upload = call_raw(
            &router,
            &imports_uri(&ws, "?name=x"),
            Some("text/csv"),
            b"Email\na@example.com\n".to_vec(),
            Auth::Key(&read_key),
        )
        .await;
        let list = call(
            &router,
            "GET",
            &imports_uri(&ws, ""),
            None,
            Auth::Key(&read_key),
        )
        .await;
        let get = call(
            &router,
            "GET",
            &imports_uri(&ws, &format!("/{id}?samples=true")),
            None,
            Auth::Key(&read_key),
        )
        .await;
        let rejects_csv = call_text(
            &router,
            "GET",
            &imports_uri(&ws, &format!("/{id}/rejects.csv")),
            None,
            String::new(),
            Auth::Key(&read_key),
        )
        .await;
        let write_cannot_read = call(
            &router,
            "GET",
            &imports_uri(&ws, ""),
            None,
            Auth::Key(&write_key),
        )
        .await;
        let write_uploads = call_raw(
            &router,
            &imports_uri(&ws, "?name=y"),
            Some("text/csv"),
            b"Email\na@example.com\n".to_vec(),
            Auth::Key(&write_key),
        )
        .await;
        let admin_get = call(
            &router,
            "GET",
            &imports_uri(&ws, &format!("/{id}?samples=true")),
            None,
            Auth::Key(&admin_key),
        )
        .await;
        let no_csrf = call(
            &router,
            "POST",
            &imports_uri(&ws, &format!("/{id}/run")),
            None,
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;
        let foreign = call(
            &router,
            "GET",
            &imports_uri(&other, &format!("/{id}")),
            None,
            Auth::Session(&owner),
        )
        .await;
        let foreign_key = call(
            &router,
            "GET",
            &imports_uri(&other, ""),
            None,
            Auth::Key(&read_key),
        )
        .await;
        let foreign_rejects = call(
            &router,
            "GET",
            &imports_uri(&other, &format!("/{id}/rejects")),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert!(
            forbidden
                .iter()
                .all(|status| *status == StatusCode::FORBIDDEN),
            "{forbidden:?}"
        );
        assert_eq!(upload.status, StatusCode::FORBIDDEN);
        assert_eq!(list.status, StatusCode::OK);
        assert_eq!(list.body["imports"].as_array().unwrap().len(), 1);
        assert_eq!(get.status, StatusCode::OK);
        assert_eq!(
            get.body["samples_masked"], true,
            "a read key sees no values"
        );
        assert_eq!(get.body["samples"][0], json!([null, null, null]));
        assert_eq!(rejects_csv.status, StatusCode::OK);
        assert_eq!(write_cannot_read.status, StatusCode::FORBIDDEN);
        assert_eq!(write_uploads.status, StatusCode::CREATED);
        assert_eq!(admin_get.body["samples_masked"], false);
        assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
        assert_eq!(foreign.status, StatusCode::NOT_FOUND);
        assert_eq!(foreign_key.status, StatusCode::NOT_FOUND);
        assert_eq!(foreign_rejects.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_history_lists_newest_first_in_pages_with_a_cursor(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let mut ids = Vec::new();
        for n in 0..3 {
            let (_, body) = upload_csv(
                &router,
                &ws,
                &format!("name=Import%20{n}"),
                "Email\na@example.com\n",
                Auth::Session(&owner),
            )
            .await;
            ids.push(body["id"].as_str().unwrap().to_owned());
        }
        ids.reverse();

        let first = call(
            &router,
            "GET",
            &imports_uri(&ws, "?limit=2"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let cursor = first.body["next_before"].as_str().unwrap().to_owned();
        let second = call(
            &router,
            "GET",
            &imports_uri(&ws, &format!("?limit=2&before={cursor}")),
            None,
            Auth::Session(&owner),
        )
        .await;

        let listed: Vec<&str> = first.body["imports"]
            .as_array()
            .unwrap()
            .iter()
            .chain(second.body["imports"].as_array().unwrap())
            .map(|import| import["id"].as_str().unwrap())
            .collect();
        assert_eq!(listed, ids.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(second.body["next_before"], Value::Null);
        assert!(!first.body.to_string().contains("a@example.com"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_rejects_csv_guards_the_header_names_and_columns_against_formulas(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (status, body) = upload_csv(
            &router,
            &ws,
            "name=Formulas",
            "Email,=HYPERLINK(\"http://x\")\nnot-an-email,1\n",
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body:?}");
        let id = body["id"].as_str().unwrap().to_owned();
        call(
            &router,
            "PUT",
            &imports_uri(&ws, &format!("/{id}/mapping")),
            Some(json!({"columns": [
                {"column": "Email", "target": {"type": "identifier", "kind": "email"}}]})),
            Auth::Session(&owner),
        )
        .await;
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        zirv_flow_store::start_import_run(&mut tx, ws.parse().unwrap(), id.parse().unwrap())
            .await
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let rejected = [zirv_flow_store::RejectedRow {
            row_number: 2,
            reasons: vec![zirv_flow_store::RejectReason {
                column: "=HYPERLINK(\"http://x\")".into(),
                reason: "wrong_type",
            }],
            raw: vec!["not-an-email".into(), "=1+1".into()],
        }];
        zirv_flow_store::commit_import_batch(
            &pool,
            &zirv_flow_store::ImportBatch {
                import_id: id.parse().unwrap(),
                workspace: ws.parse().unwrap(),
                expected_done: 0,
                records: &[],
                details: &[],
                rejected: &rejected,
                trace: &zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t")),
            },
            false,
        )
        .await
        .unwrap();

        let csv = call_text(
            &router,
            "GET",
            &imports_uri(&ws, &format!("/{id}/rejects.csv")),
            None,
            String::new(),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(
            csv.text,
            "row_number,column,reason,Email,\"'=HYPERLINK(\"\"http://x\"\")\"\n\
             2,\"'=HYPERLINK(\"\"http://x\"\")\",wrong_type,not-an-email,'=1+1\n"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_file_with_an_unterminated_quote_is_a_422_with_a_code_and_keeps_nothing(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let reply = call_text(
            &router,
            "POST",
            &imports_uri(&ws, "?name=Broken"),
            Some("text/csv"),
            "Email\n\"ada@example.com\nbob@example.com\n".to_owned(),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        let body: Value = serde_json::from_str(&reply.text).unwrap();
        assert_eq!(
            body["detail"],
            "body: a quoted field is never closed (unterminated_quote)"
        );
        let imports: i64 = sqlx::query_scalar("SELECT count(*) FROM imports")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(imports, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_upload_that_stalls_or_breaks_off_mid_body_leaves_no_import_and_no_chunks(
        pool: PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let router = router(
            AppState::new(pool.clone(), Arc::new(String::new)).with_import_limits(
                crate::ImportLimits {
                    idle_timeout: std::time::Duration::from_millis(200),
                    ..crate::ImportLimits::default()
                },
            ),
        );
        admin::create_owner(&pool, &Context::system(), OWNER_EMAIL, PASSWORD)
            .await
            .unwrap();
        let owner = login(&router, OWNER_EMAIL).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let first = format!("Email\n{}", "ada@example.com\n".repeat(150_000)).into_bytes();
        let send = |body: Body| {
            let request = Request::builder()
                .method("POST")
                .uri(imports_uri(&ws, "?name=x"))
                .header(header::CONTENT_TYPE, "text/csv")
                .header(header::COOKIE, &owner.cookie)
                .header("x-csrf-token", &owner.csrf)
                .body(body)
                .unwrap();
            let router = router.clone();
            async move { router.oneshot(request).await.unwrap().status() }
        };

        let stalled = send(Body::from_stream(futures_util::StreamExt::chain(
            futures_util::stream::iter([Ok::<_, std::io::Error>(first.clone())]),
            futures_util::stream::pending(),
        )))
        .await;
        let broken = send(Body::from_stream(futures_util::stream::iter([
            Ok(first.clone()),
            Err(std::io::Error::other("connection reset")),
        ])))
        .await;

        assert_eq!(stalled, StatusCode::REQUEST_TIMEOUT);
        assert_eq!(broken, StatusCode::BAD_REQUEST);
        for table in ["imports", "import_chunks"] {
            let rows: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(rows, 0, "{table}");
        }
    }

    // ---- The event debugger, the explorer and reject replay (#45) ----

    fn app_limits(limits: crate::ExplorerLimits) -> impl Fn(&PgPool) -> Router {
        move |pool| {
            router(
                AppState::new(pool.clone(), Arc::new(String::new))
                    .with_hmac_root(&zirv_flow_core::HmacRoot::bootstrap_from(
                        &zirv_flow_core::MasterKey::generate(),
                    ))
                    .with_explorer_limits(limits),
            )
        }
    }

    /// A schema `order` with a required number `total`, a PII `email` and a plain `plan`,
    /// published, in the given mode.
    async fn publish_order(router: &Router, owner: &Login, ws: &str, mode: &str) {
        let created = call(
            router,
            "POST",
            &schemas_uri(ws),
            Some(json!({"name": "order", "mode": mode, "properties": [
                {"name": "total", "type": "number", "required": true},
                {"name": "email", "type": "string", "pii": true},
                {"name": "plan", "type": "string"}
            ]})),
            Auth::Session(owner),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let published = call(
            router,
            "POST",
            &format!("{}/order/versions", schemas_uri(ws)),
            None,
            Auth::Session(owner),
        )
        .await;
        assert_eq!(
            published.status,
            StatusCode::CREATED,
            "{:?}",
            published.body
        );
    }

    async fn put_policy(router: &Router, owner: &Login, ws: &str, policy: Value) {
        let reply = call(
            router,
            "PUT",
            &format!("/api/v1/workspaces/{ws}/event-policy"),
            Some(policy),
            Auth::Session(owner),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    }

    fn inv_record(
        who: &str,
        name: &str,
        message: Option<&str>,
        minutes_ago: i64,
        properties: Value,
    ) -> zirv_flow_store::EventRecord {
        let now = time::OffsetDateTime::now_utc();
        zirv_flow_store::EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), who.into())],
            name: name.into(),
            properties: properties.as_object().cloned().unwrap(),
            timestamp: Some(now - time::Duration::minutes(minutes_ago)),
            received_at: now,
            message_id: message.map(str::to_owned),
            trace: zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t")),
        }
    }

    async fn inv_store(pool: &PgPool, ws: &str, records: Vec<zirv_flow_store::EventRecord>) {
        let key = zirv_flow_core::HmacRoot::bootstrap_from(&zirv_flow_core::MasterKey::generate())
            .hmac_key(zirv_flow_core::HmacPurpose::Redaction);
        zirv_flow_store::apply_event_batch(pool, ws.parse().unwrap(), &key, &records)
            .await
            .unwrap();
    }

    struct Frame {
        event: String,
        id: Option<String>,
        data: Value,
        raw: String,
    }

    struct Sse {
        body: Body,
        buffer: String,
    }

    async fn open_stream(
        router: &Router,
        uri: &str,
        auth: Auth<'_>,
        last_event_id: Option<&str>,
    ) -> Sse {
        let mut request = Request::builder().method("GET").uri(uri);
        if let Some(last) = last_event_id {
            request = request.header("last-event-id", last);
        }
        let response = router
            .clone()
            .oneshot(with_auth(request, auth).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        Sse {
            body: response.into_body(),
            buffer: String::new(),
        }
    }

    impl Sse {
        /// The next frame that is not a comment, or none within `wait`.
        async fn next(&mut self, wait: std::time::Duration) -> Option<Frame> {
            let deadline = tokio::time::Instant::now() + wait;
            loop {
                while let Some(end) = self.buffer.find("\n\n") {
                    let chunk: String = self.buffer.drain(..end + 2).collect();
                    if chunk.starts_with(':') {
                        continue;
                    }
                    let mut frame = Frame {
                        event: String::new(),
                        id: None,
                        data: Value::Null,
                        raw: chunk.clone(),
                    };
                    for line in chunk.lines() {
                        if let Some(rest) = line.strip_prefix("event: ") {
                            frame.event = rest.to_owned();
                        } else if let Some(rest) = line.strip_prefix("id: ") {
                            frame.id = Some(rest.to_owned());
                        } else if let Some(rest) = line.strip_prefix("data: ") {
                            frame.data = serde_json::from_str(rest).unwrap();
                        }
                    }
                    return Some(frame);
                }
                let left = deadline.checked_duration_since(tokio::time::Instant::now())?;
                let next = tokio::time::timeout(left, self.body.frame()).await.ok()??;
                if let Ok(data) = next.unwrap().into_data() {
                    self.buffer.push_str(std::str::from_utf8(&data).unwrap());
                }
            }
        }

        async fn take(&mut self, count: usize) -> Vec<Frame> {
            let mut frames = Vec::new();
            while frames.len() < count {
                let frame = self
                    .next(std::time::Duration::from_secs(10))
                    .await
                    .expect("a frame within 10 seconds");
                frames.push(frame);
            }
            frames
        }
    }

    fn stream_uri(ws: &str, query: &str) -> String {
        format!("/api/v1/workspaces/{ws}/debugger/stream{query}")
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_debugger_streams_outcomes_in_order_masked_per_viewer_and_never_unredacted(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        publish_order(&router, &owner, &ws, "warn").await;
        put_policy(
            &router,
            &owner,
            &ws,
            json!({"unplanned": "reject", "future_skew_seconds": 300,
                   "lateness_seconds": 86400, "redaction": {"hash": ["email"]}}),
        )
        .await;
        let mut as_owner = open_stream(
            &router,
            &stream_uri(&ws, "?reveal=true"),
            Auth::Session(&owner),
            None,
        )
        .await;
        let mut as_member =
            open_stream(&router, &stream_uri(&ws, ""), Auth::Session(&member), None).await;
        let mut member_reveals = open_stream(
            &router,
            &stream_uri(&ws, "?reveal=true"),
            Auth::Session(&member),
            None,
        )
        .await;

        inv_store(
            &pool,
            &ws,
            vec![
                inv_record(
                    "a@example.com",
                    "order",
                    Some("m-1"),
                    1,
                    json!({"total": 5, "email": "a@example.com", "plan": "pro"}),
                ),
                inv_record(
                    "a@example.com",
                    "order",
                    Some("m-2"),
                    1,
                    json!({"total": "free", "email": "a@example.com", "plan": "pro"}),
                ),
                inv_record(
                    "a@example.com",
                    "ghost",
                    Some("m-3"),
                    1,
                    json!({"note": "plain-note"}),
                ),
            ],
        )
        .await;

        let owner_frames = as_owner.take(3).await;
        let member_frames = as_member.take(3).await;
        let revealed_by_member = member_reveals.take(3).await;
        let outcomes: Vec<&str> = owner_frames
            .iter()
            .map(|frame| frame.data["outcome"].as_str().unwrap())
            .collect();
        assert_eq!(outcomes, ["accepted", "flagged", "rejected"]);
        assert!(owner_frames.iter().all(|frame| frame.event == "item"));
        let seqs: Vec<i64> = owner_frames
            .iter()
            .map(|frame| frame.id.as_ref().unwrap().parse().unwrap())
            .collect();
        assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(owner_frames[1].data["reasons"][0]["reason"], "wrong_type");
        assert_eq!(owner_frames[2].data["reason"], "unplanned_event");
        // The owner who reveals sees the stored (hashed) value, never the raw one.
        let hashed = owner_frames[0].data["properties"]["email"]
            .as_str()
            .unwrap();
        assert_eq!(hashed.len(), 64);
        assert_eq!(owner_frames[0].data["properties"]["plan"], "pro");
        // A member sees the PII property masked, with or without asking to reveal.
        for frames in [&member_frames, &revealed_by_member] {
            assert!(frames[0].data["properties"]["email"].is_null());
            assert!(
                frames[0].data["masked"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("email"))
            );
            assert_eq!(frames[0].data["properties"]["plan"], "pro");
            assert!(frames[2].data["properties"]["note"].is_null());
        }
        assert_eq!(owner_frames[2].data["properties"]["note"], "plain-note");
        for frame in owner_frames
            .iter()
            .chain(&member_frames)
            .chain(&revealed_by_member)
        {
            assert!(!frame.raw.contains("a@example.com"), "{}", frame.raw);
        }
        assert_eq!(owner_frames[0].data["trace_id"].as_str().unwrap().len(), 32);
        // The feed rows hold the redacted value only.
        let stored: String = sqlx::query_scalar(
            "SELECT coalesce(string_agg(properties::text, ' '), '') FROM debugger_feed",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!stored.contains("a@example.com"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_debugger_resumes_from_last_event_id_and_isolates_workspaces(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let one = workspace(&router, &owner, "One").await;
        let two = workspace(&router, &owner, "Two").await;
        let mut first =
            open_stream(&router, &stream_uri(&one, ""), Auth::Session(&owner), None).await;
        let mut other =
            open_stream(&router, &stream_uri(&two, ""), Auth::Session(&owner), None).await;
        inv_store(
            &pool,
            &one,
            vec![
                inv_record("a@example.com", "seen", None, 1, json!({})),
                inv_record("a@example.com", "seen", None, 1, json!({})),
                inv_record("a@example.com", "seen", None, 1, json!({})),
            ],
        )
        .await;
        let frames = first.take(3).await;
        let after_first = frames[0].id.clone().unwrap();

        let mut resumed = open_stream(
            &router,
            &stream_uri(&one, ""),
            Auth::Session(&owner),
            Some(&after_first),
        )
        .await;
        let replayed = resumed.take(2).await;
        inv_store(
            &pool,
            &two,
            vec![inv_record("b@example.com", "seen", None, 1, json!({}))],
        )
        .await;
        let theirs = other.take(1).await;

        assert_eq!(
            replayed
                .iter()
                .map(|f| f.id.clone().unwrap())
                .collect::<Vec<_>>(),
            [frames[1].id.clone().unwrap(), frames[2].id.clone().unwrap()]
        );
        assert_eq!(theirs.len(), 1);
        assert!(
            other
                .next(std::time::Duration::from_millis(500))
                .await
                .is_none()
        );
        let first_again = first.next(std::time::Duration::from_millis(300)).await;
        assert!(
            first_again.is_none(),
            "workspace one saw an event of workspace two"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_debugger_filters_and_refuses_bad_queries_and_unauthorised_viewers(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (_, write_key) = make_key(&router, &owner, &ws, &["ingest"]).await;
        let mut only_named = open_stream(
            &router,
            &stream_uri(&ws, "?name=wanted&outcome=unplanned"),
            Auth::Session(&owner),
            None,
        )
        .await;

        inv_store(
            &pool,
            &ws,
            vec![
                inv_record("a@example.com", "other", None, 1, json!({})),
                inv_record("a@example.com", "wanted", None, 1, json!({})),
            ],
        )
        .await;
        let frames = only_named.take(1).await;
        let bad = call(
            &router,
            "GET",
            &stream_uri(&ws, "?outcome=lost"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let anonymous = call(&router, "GET", &stream_uri(&ws, ""), None, Auth::Anonymous).await;
        let ingest_only = call(
            &router,
            "GET",
            &stream_uri(&ws, ""),
            None,
            Auth::Key(&write_key),
        )
        .await;

        assert_eq!(frames[0].data["name"], "wanted");
        assert!(
            only_named
                .next(std::time::Duration::from_millis(300))
                .await
                .is_none()
        );
        assert_eq!(bad.status, StatusCode::BAD_REQUEST);
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(ingest_only.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_slow_viewer_gets_a_gap_and_ingestion_is_not_held_up(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let mut slow =
            open_stream(&router, &stream_uri(&ws, ""), Auth::Session(&owner), None).await;

        let started = std::time::Instant::now();
        inv_store(
            &pool,
            &ws,
            (0..300)
                .map(|_| inv_record("a@example.com", "seen", None, 1, json!({})))
                .collect(),
        )
        .await;
        assert!(started.elapsed() < std::time::Duration::from_secs(30));

        let mut items = 0;
        let mut skipped = 0;
        for _ in 0..200 {
            let Some(frame) = slow.next(std::time::Duration::from_secs(12)).await else {
                break;
            };
            match frame.event.as_str() {
                "item" => items += 1,
                "gap" => {
                    skipped = frame.data["skipped"].as_u64().unwrap();
                    break;
                }
                other => panic!("unexpected frame {other}"),
            }
        }

        assert!(skipped > 0, "no gap after {items} items");
        assert!(items < 300);
    }

    fn explore_uri(ws: &str, what: &str) -> String {
        format!("/api/v1/workspaces/{ws}/events/explore/{what}")
    }

    fn explore_range() -> Value {
        let now = time::OffsetDateTime::now_utc();
        let format = time::format_description::well_known::Rfc3339;
        json!({
            "from": (now - time::Duration::days(3)).format(&format).unwrap(),
            "to": (now + time::Duration::hours(1)).format(&format).unwrap(),
        })
    }

    fn with_range(mut body: Value) -> Value {
        let range = explore_range();
        body["from"] = range["from"].clone();
        body["to"] = range["to"].clone();
        body
    }

    async fn seed_explorer(pool: &PgPool, router: &Router, owner: &Login, ws: &str) {
        publish_order(router, owner, ws, "warn").await;
        inv_store(
            pool,
            ws,
            vec![
                inv_record(
                    "a@example.com",
                    "order",
                    None,
                    60,
                    json!({"total": 10, "plan": "pro", "email": "a@example.com"}),
                ),
                inv_record(
                    "a@example.com",
                    "order",
                    None,
                    120,
                    json!({"total": 20, "plan": "pro", "email": "a@example.com"}),
                ),
                inv_record(
                    "b@example.com",
                    "order",
                    None,
                    60 * 25,
                    json!({"total": 30, "plan": "free"}),
                ),
                inv_record(
                    "b@example.com",
                    "order",
                    None,
                    60 * 26,
                    json!({"total": 40, "plan": "team"}),
                ),
                inv_record("b@example.com", "page", None, 5, json!({"path": "/x"})),
            ],
        )
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_explorer_count_breakdown_list_and_export_agree_and_mask_per_viewer(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        seed_explorer(&pool, &router, &owner, &ws).await;
        let scope = with_range(json!({
            "name": "order",
            "filters": [{"key": "total", "op": "gte", "value": 20}]
        }));
        let with = |extra: Value| {
            let mut body = scope.clone();
            for (key, value) in extra.as_object().unwrap() {
                body[key] = value.clone();
            }
            body
        };

        let count = call(
            &router,
            "POST",
            &explore_uri(&ws, "count"),
            Some(with(json!({"bucket": "day"}))),
            Auth::Session(&owner),
        )
        .await;
        let breakdown = call(
            &router,
            "POST",
            &explore_uri(&ws, "breakdown"),
            Some(with(json!({"property": "plan"}))),
            Auth::Session(&owner),
        )
        .await;
        let list = call(
            &router,
            "POST",
            &explore_uri(&ws, "list"),
            Some(with(json!({"limit": 2}))),
            Auth::Session(&owner),
        )
        .await;
        let member_list = call(
            &router,
            "POST",
            &explore_uri(&ws, "list"),
            Some(with(json!({}))),
            Auth::Session(&member),
        )
        .await;
        let exported = call_text(
            &router,
            "POST",
            &explore_uri(&ws, "export"),
            Some("application/json"),
            scope.to_string(),
            Auth::Session(&owner),
        )
        .await;
        let member_export = call_text(
            &router,
            "POST",
            &explore_uri(&ws, "export"),
            Some("application/json"),
            scope.to_string(),
            Auth::Session(&member),
        )
        .await;

        assert_eq!(count.status, StatusCode::OK, "{:?}", count.body);
        assert_eq!(count.body["total"], 3);
        assert_eq!(count.body["truncated"], false);
        let grouped: i64 = breakdown.body["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["count"].as_i64().unwrap())
            .sum();
        assert_eq!(
            grouped
                + breakdown.body["other"].as_i64().unwrap()
                + breakdown.body["missing"].as_i64().unwrap(),
            3
        );
        assert_eq!(list.body["events"].as_array().unwrap().len(), 2);
        let cursor = list.body["next_after"].as_str().unwrap();
        let rest = call(
            &router,
            "POST",
            &explore_uri(&ws, "list"),
            Some(with(json!({"limit": 2, "after": cursor}))),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(rest.body["events"].as_array().unwrap().len(), 1);
        assert!(rest.body["next_after"].is_null());
        assert_eq!(exported.status, StatusCode::OK);
        assert_eq!(
            exported.headers[header::CONTENT_TYPE],
            "text/csv; charset=utf-8"
        );
        assert_eq!(exported.text.lines().count(), 1 + 3);
        // Members see the PII property masked in the list and the export.
        let first = &member_list.body["events"][0];
        assert!(
            first["properties"]["email"].is_null() || first["properties"].get("email").is_none()
        );
        assert!(!member_list.body.to_string().contains("a@example.com"));
        assert!(!member_export.text.contains("a@example.com"));
        // The export is audited with its shape and row count, no values.
        let audited = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        let entry = audited
            .iter()
            .find(|row| row.action == "events.exported")
            .unwrap();
        assert_eq!(entry.summary["rows"], 3);
        assert_eq!(entry.summary["filter_ops"], json!(["gte"]));
        let summary = entry.summary.to_string();
        assert!(!summary.contains("order") && !summary.contains("total"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_explorer_refuses_pii_probes_for_members_and_bad_input_for_everyone(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        seed_explorer(&pool, &router, &owner, &ws).await;
        let by_email = with_range(json!({"name": "order", "property": "email"}));
        let prefix = with_range(json!({"name": "order", "bucket": "day",
            "filters": [{"key": "email", "op": "prefix", "value": "a"}]}));

        let member_breakdown = call(
            &router,
            "POST",
            &explore_uri(&ws, "breakdown"),
            Some(by_email.clone()),
            Auth::Session(&member),
        )
        .await;
        let owner_breakdown = call(
            &router,
            "POST",
            &explore_uri(&ws, "breakdown"),
            Some(by_email),
            Auth::Session(&owner),
        )
        .await;
        let member_prefix = call(
            &router,
            "POST",
            &explore_uri(&ws, "count"),
            Some(prefix),
            Auth::Session(&member),
        )
        .await;
        let bad_bucket = call(
            &router,
            "POST",
            &explore_uri(&ws, "count"),
            Some(with_range(json!({"bucket": "year"}))),
            Auth::Session(&owner),
        )
        .await;
        let too_wide = call(
            &router,
            "POST",
            &explore_uri(&ws, "count"),
            Some(json!({"bucket": "day", "from": "2024-01-01T00:00:00Z", "to": "2026-01-01T00:00:00Z"})),
            Auth::Session(&owner),
        )
        .await;
        let bad_limit = call(
            &router,
            "POST",
            &explore_uri(&ws, "list"),
            Some(with_range(json!({"limit": 0}))),
            Auth::Session(&owner),
        )
        .await;
        let no_csrf = call(
            &router,
            "POST",
            &explore_uri(&ws, "count"),
            Some(with_range(json!({"bucket": "day"}))),
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;

        assert_eq!(member_breakdown.status, StatusCode::FORBIDDEN);
        assert_eq!(member_breakdown.body["code"], "pii_property_not_explorable");
        assert_eq!(
            owner_breakdown.status,
            StatusCode::OK,
            "{:?}",
            owner_breakdown.body
        );
        assert_eq!(member_prefix.status, StatusCode::FORBIDDEN);
        assert_eq!(bad_bucket.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(too_wide.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(bad_limit.status, StatusCode::BAD_REQUEST);
        assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_explorer_query_past_the_timeout_is_a_503_query_timeout(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let hurried = app_limits(crate::ExplorerLimits {
            interactive_timeout: std::time::Duration::from_millis(1),
            ..crate::ExplorerLimits::default()
        })(&pool);
        let owner = owner(&pool, &hurried).await;
        let ws = workspace(&hurried, &owner, "Alpha").await;
        inv_store(
            &pool,
            &ws,
            vec![inv_record("a@example.com", "seen", None, 1, json!({}))],
        )
        .await;
        sqlx::query(
            "INSERT INTO events \
                (id, workspace_id, person_id, name, occurred_at, received_at, properties, \
                 source, trace_id, validation_state) \
             SELECT gen_random_uuid(), e.workspace_id, e.person_id, 'bulk', \
                    e.occurred_at - make_interval(secs => n), e.received_at, \
                    jsonb_build_object('n', n), 'web', gen_random_uuid(), 'unplanned' \
             FROM (SELECT * FROM events LIMIT 1) e, generate_series(1, 200000) AS n",
        )
        .execute(&pool)
        .await
        .unwrap();
        let body = with_range(json!({"bucket": "hour",
            "filters": [{"key": "n", "op": "prefix", "value": "99"}]}));

        let reply = call(
            &hurried,
            "POST",
            &explore_uri(&ws, "count"),
            Some(body),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(
            reply.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{:?}",
            reply.body
        );
        assert_eq!(reply.body["code"], "query_timeout");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_export_past_the_row_limit_ends_with_a_comment_line(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let small = app_limits(crate::ExplorerLimits {
            export_row_limit: 2,
            ..crate::ExplorerLimits::default()
        })(&pool);
        let owner = owner(&pool, &small).await;
        let ws = workspace(&small, &owner, "Alpha").await;
        inv_store(
            &pool,
            &ws,
            (0..4)
                .map(|n| inv_record("a@example.com", "seen", None, n, json!({})))
                .collect(),
        )
        .await;

        let reply = call_text(
            &small,
            "POST",
            &explore_uri(&ws, "export"),
            Some("application/json"),
            explore_range().to_string(),
            Auth::Session(&owner),
        )
        .await;

        let lines: Vec<&str> = reply.text.lines().collect();
        assert_eq!(lines.len(), 1 + 2 + 1, "{}", reply.text);
        assert!(lines[3].starts_with("# truncated"));
        let audited = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        let entry = audited
            .iter()
            .find(|row| row.action == "events.exported")
            .unwrap();
        assert_eq!(entry.summary["rows"], json!(2));
        assert_eq!(entry.summary["truncated"], json!(true));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn replay_is_for_admins_previews_without_writing_and_enqueues_an_audited_job(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        publish_order(&router, &owner, &ws, "block").await;
        inv_store(
            &pool,
            &ws,
            vec![inv_record(
                "a@example.com",
                "order",
                Some("m-0"),
                5,
                json!({"total": 1}),
            )],
        )
        .await;
        inv_store(
            &pool,
            &ws,
            vec![inv_record(
                "a@example.com",
                "order",
                Some("m-1"),
                5,
                json!({"total": "ten"}),
            )],
        )
        .await;
        let rejects = call(
            &router,
            "GET",
            &format!("/api/v1/workspaces/{ws}/ingest-rejects"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let reject = rejects.body["rejects"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let uri = format!("/api/v1/workspaces/{ws}/ingest-rejects/replay");
        let fixed = call(
            &router,
            "PUT",
            &format!("{}/order/draft", schemas_uri(&ws)),
            Some(json!({"properties": [{"name": "total", "type": "string", "required": true}]})),
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(fixed.status, StatusCode::OK, "{:?}", fixed.body);
        call(
            &router,
            "POST",
            &format!("{}/order/versions", schemas_uri(&ws)),
            None,
            Auth::Session(&owner),
        )
        .await;
        let request = |dry_run: bool| json!({"ids": [reject], "cause": "definition_change", "dry_run": dry_run});

        let as_member = call(
            &router,
            "POST",
            &uri,
            Some(request(true)),
            Auth::Session(&member),
        )
        .await;
        let as_read_key = call(
            &router,
            "POST",
            &uri,
            Some(request(true)),
            Auth::Key(&read_key),
        )
        .await;
        let no_csrf = call(
            &router,
            "POST",
            &uri,
            Some(request(true)),
            Auth::SessionWithoutCsrf(&owner),
        )
        .await;
        let both = call(
            &router,
            "POST",
            &uri,
            Some(json!({"ids": [reject], "filter": {}, "cause": "manual"})),
            Auth::Session(&owner),
        )
        .await;
        let bad_cause = call(
            &router,
            "POST",
            &uri,
            Some(json!({"ids": [reject], "cause": "whim"})),
            Auth::Session(&owner),
        )
        .await;
        let preview = call(
            &router,
            "POST",
            &uri,
            Some(request(true)),
            Auth::Session(&owner),
        )
        .await;
        let jobs_before: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'ingest.replay'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let queued = call(
            &router,
            "POST",
            &uri,
            Some(request(false)),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(as_member.status, StatusCode::FORBIDDEN);
        assert_eq!(as_read_key.status, StatusCode::FORBIDDEN);
        assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
        assert_eq!(both.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(bad_cause.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(preview.status, StatusCode::OK, "{:?}", preview.body);
        assert_eq!(preview.body["rejects"][0]["outcome"], "accept");
        assert_eq!(preview.body["totals"]["accept"], 1);
        assert_eq!(jobs_before, 0);
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events, 1);
        assert_eq!(queued.status, StatusCode::ACCEPTED, "{:?}", queued.body);
        assert_eq!(queued.body["selected"], 1);
        assert_eq!(queued.body["trace_id"].as_str().unwrap().len(), 32);
        let (kind, cause): (String, String) =
            sqlx::query_as("SELECT kind, cause_kind FROM jobs WHERE kind = 'ingest.replay'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            (kind.as_str(), cause.as_str()),
            ("ingest.replay", "definition_change")
        );
        let audited = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        let entries: Vec<_> = audited
            .iter()
            .filter(|row| row.action == "ingest_rejects.replayed")
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].summary["selected"], 1);
        assert_eq!(entries[0].summary["cause"], "definition_change");
        assert_eq!(entries[0].summary["emit_triggers"], false);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn replaying_nothing_is_refused_or_answered_without_a_job_or_an_audit_row(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let uri = format!("/api/v1/workspaces/{ws}/ingest-rejects/replay");
        let unknown = uuid::Uuid::now_v7();

        let empty = call(
            &router,
            "POST",
            &uri,
            Some(json!({"ids": [], "cause": "manual", "dry_run": false})),
            Auth::Session(&owner),
        )
        .await;
        let no_such_id = call(
            &router,
            "POST",
            &uri,
            Some(json!({"ids": [unknown], "cause": "manual", "dry_run": false})),
            Auth::Session(&owner),
        )
        .await;
        let no_match = call(
            &router,
            "POST",
            &uri,
            Some(json!({"filter": {"name": "ghost"}, "cause": "manual", "dry_run": false})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);
        for answered in [&no_such_id, &no_match] {
            assert_eq!(answered.status, StatusCode::OK, "{:?}", answered.body);
            assert_eq!(answered.body["selected"], 0);
            assert!(answered.body["trace_id"].is_null());
        }
        let jobs: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'ingest.replay'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(jobs, 0);
        let audited = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        assert!(
            audited
                .iter()
                .all(|row| row.action != "ingest_rejects.replayed")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replay_preview_lists_at_most_200_rejects_but_totals_all_of_them(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        sqlx::query(
            "INSERT INTO ingest_rejects \
                (id, workspace_id, received_at, source, kind, name, reason, trace_id) \
             SELECT gen_random_uuid(), $1::uuid, now() - make_interval(secs => n), 'web', 'event', \
                    'order', 'validation_failed', gen_random_uuid() \
             FROM generate_series(1, 250) AS n",
        )
        .bind(ws.parse::<uuid::Uuid>().unwrap())
        .execute(&pool)
        .await
        .unwrap();
        let uri = format!("/api/v1/workspaces/{ws}/ingest-rejects/replay");

        let preview = call(
            &router,
            "POST",
            &uri,
            Some(json!({"filter": {}, "cause": "manual", "dry_run": true})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(preview.status, StatusCode::OK, "{:?}", preview.body);
        assert_eq!(preview.body["selected"], 250);
        assert_eq!(preview.body["rejects"].as_array().unwrap().len(), 200);
        assert_eq!(preview.body["truncated"], true);
        assert_eq!(preview.body["totals"]["still_rejected"], 250);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replay_preview_past_its_timeout_is_a_503_query_timeout(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let hurried = app_limits(crate::ExplorerLimits {
            export_timeout: std::time::Duration::from_millis(1),
            ..crate::ExplorerLimits::default()
        })(&pool);
        let owner = owner(&pool, &hurried).await;
        let ws = workspace(&hurried, &owner, "Alpha").await;
        sqlx::query(
            "INSERT INTO ingest_rejects \
                (id, workspace_id, received_at, source, kind, name, reason, payload, trace_id) \
             SELECT gen_random_uuid(), $1::uuid, now() - make_interval(secs => n), 'web', 'event', \
                    'order', 'validation_failed', \
                    jsonb_build_object('properties', jsonb_build_object('blob', repeat('x', 2000))), \
                    gen_random_uuid() \
             FROM generate_series(1, 9000) AS n",
        )
        .bind(ws.parse::<uuid::Uuid>().unwrap())
        .execute(&pool)
        .await
        .unwrap();
        let uri = format!("/api/v1/workspaces/{ws}/ingest-rejects/replay");

        let preview = call(
            &hurried,
            "POST",
            &uri,
            Some(json!({"filter": {}, "cause": "manual", "dry_run": true})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(
            preview.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{:?}",
            preview.body
        );
        assert_eq!(preview.body["code"], "query_timeout");
    }

    async fn feed_row(executor: impl sqlx::PgExecutor<'_>, ws: &str, name: &str) {
        sqlx::query(
            "INSERT INTO debugger_feed (workspace_id, source, name, outcome, trace_id) \
             VALUES ($1::uuid, 'web', $2, 'accepted', gen_random_uuid())",
        )
        .bind(ws)
        .bind(name)
        .execute(executor)
        .await
        .unwrap();
    }

    async fn notify_feed(pool: &PgPool, ws: &str) {
        sqlx::query("SELECT pg_notify('zf_debugger', $1)")
            .bind(ws)
            .execute(pool)
            .await
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_row_that_commits_after_a_later_one_is_still_delivered(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let mut viewer =
            open_stream(&router, &stream_uri(&ws, ""), Auth::Session(&owner), None).await;

        // Writer A takes the lower seq and stays open; writer B takes the next and commits.
        let mut writer_a = pool.begin().await.unwrap();
        feed_row(&mut *writer_a, &ws, "from_a").await;
        feed_row(&pool, &ws, "from_b").await;
        notify_feed(&pool, &ws).await;
        let first = viewer.take(1).await;
        writer_a.commit().await.unwrap();
        notify_feed(&pool, &ws).await;
        let second = viewer.take(1).await;

        assert_eq!(first[0].data["name"], "from_b");
        assert_eq!(second[0].data["name"], "from_a");
        // The first frame could not claim a position past the open writer; the second can.
        assert_eq!(first[0].id.as_deref(), Some("0"));
        assert_eq!(second[0].id.as_deref(), Some("2"));
        assert!(
            viewer
                .next(std::time::Duration::from_millis(600))
                .await
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_resume_position_beyond_the_feed_is_clamped_and_announced(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;

        let mut viewer = open_stream(
            &router,
            &stream_uri(&ws, ""),
            Auth::Session(&owner),
            Some("500"),
        )
        .await;
        let announced = viewer.take(1).await;
        inv_store(
            &pool,
            &ws,
            vec![inv_record("a@example.com", "seen", None, 1, json!({}))],
        )
        .await;
        let live = viewer.take(1).await;

        assert_eq!(announced[0].event, "gap");
        assert_eq!(announced[0].data["reset"], true);
        assert_eq!(live[0].event, "item");
        assert_eq!(live[0].data["name"], "seen");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_down_notifier_is_announced_and_the_watch_renews_without_it(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let (router, state) = app_with_state(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let mut viewer =
            open_stream(&router, &stream_uri(&ws, ""), Auth::Session(&owner), None).await;

        // Let the real listener connect first, so it does not report itself up afterwards.
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        state.debugger.set_degraded_for_test(true);
        let down = viewer.take(1).await;
        // The viewer keeps reading by polling while the notifier is down.
        inv_store(
            &pool,
            &ws,
            vec![inv_record("a@example.com", "seen", None, 1, json!({}))],
        )
        .await;
        let polled = viewer.take(1).await;
        state.debugger.set_degraded_for_test(false);
        let up = viewer.take(1).await;
        sqlx::query("DELETE FROM debugger_watch")
            .execute(&pool)
            .await
            .unwrap();
        state.debugger.renew_once_for_test().await;
        let renewed: i64 =
            sqlx::query_scalar("SELECT count(*) FROM debugger_watch WHERE until > now()")
                .fetch_one(&pool)
                .await
                .unwrap();

        assert_eq!(
            (down[0].event.as_str(), &down[0].data["degraded"]),
            ("status", &json!(true))
        );
        assert_eq!(polled[0].data["name"], "seen");
        assert_eq!(
            (up[0].event.as_str(), &up[0].data["degraded"]),
            ("status", &json!(false))
        );
        assert_eq!(renewed, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_twenty_first_stream_of_a_workspace_is_refused_until_one_closes(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let other = workspace(&router, &owner, "Beta").await;
        let mut open = Vec::new();
        for _ in 0..20 {
            open.push(
                open_stream(&router, &stream_uri(&ws, ""), Auth::Session(&owner), None).await,
            );
        }

        let refused = call(
            &router,
            "GET",
            &stream_uri(&ws, ""),
            None,
            Auth::Session(&owner),
        )
        .await;
        let elsewhere = open_stream(
            &router,
            &stream_uri(&other, ""),
            Auth::Session(&owner),
            None,
        )
        .await;
        drop(open.pop());
        let mut admitted = None;
        for _ in 0..40 {
            let again = router
                .clone()
                .oneshot(
                    with_auth(
                        Request::builder().method("GET").uri(stream_uri(&ws, "")),
                        Auth::Session(&owner),
                    )
                    .body(Body::empty())
                    .unwrap(),
                )
                .await
                .unwrap();
            if again.status() == StatusCode::OK {
                admitted = Some(again);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }

        assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(refused.headers[header::RETRY_AFTER], "5");
        drop(elsewhere);
        assert!(admitted.is_some(), "a closed stream frees its place");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_shutdown_signal_ends_the_streams(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let state = AppState::new(pool.clone(), Arc::new(String::new))
            .with_hmac_root(&zirv_flow_core::HmacRoot::bootstrap_from(
                &zirv_flow_core::MasterKey::generate(),
            ))
            .with_stop(stopped);
        let router = router(state);
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let mut viewer =
            open_stream(&router, &stream_uri(&ws, ""), Auth::Session(&owner), None).await;
        assert!(
            viewer
                .next(std::time::Duration::from_millis(300))
                .await
                .is_none()
        );

        stop.send(true).unwrap();

        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match viewer.body.frame().await {
                    None => return true,
                    Some(Err(_)) => return true,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await;
        assert_eq!(
            ended,
            Ok(true),
            "the stream ends when the server shuts down"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn another_workspaces_numbers_do_not_hold_a_viewers_position_back(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let mine = workspace(&router, &owner, "Mine").await;
        let theirs = workspace(&router, &owner, "Theirs").await;
        let mut viewer =
            open_stream(&router, &stream_uri(&mine, ""), Auth::Session(&owner), None).await;

        // Two numbers go to another workspace, then one to this viewer's.
        feed_row(&pool, &theirs, "not_yours").await;
        feed_row(&pool, &theirs, "not_yours_either").await;
        feed_row(&pool, &mine, "yours").await;
        notify_feed(&pool, &mine).await;
        let frames = viewer.take(1).await;

        assert_eq!(frames[0].data["name"], "yours");
        assert!(!frames[0].raw.contains("not_yours"));
        // No wait for the numbers of the other workspace: the position is already past them.
        assert_eq!(frames[0].id.as_deref(), Some("3"));
        assert!(
            viewer
                .next(std::time::Duration::from_millis(300))
                .await
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_viewer_that_connects_between_two_commits_still_gets_the_late_one(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;

        // A takes seq 1 and stays open; B takes seq 2 and commits; then the viewer connects.
        let mut writer_a = pool.begin().await.unwrap();
        feed_row(&mut *writer_a, &ws, "from_a").await;
        feed_row(&pool, &ws, "from_b").await;
        let mut viewer =
            open_stream(&router, &stream_uri(&ws, ""), Auth::Session(&owner), None).await;
        writer_a.commit().await.unwrap();
        notify_feed(&pool, &ws).await;

        let frames = viewer.take(2).await;

        let names: Vec<&str> = frames
            .iter()
            .map(|frame| frame.data["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"from_a"), "{names:?}");
        assert!(names.contains(&"from_b"), "{names:?}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reconnect_after_the_feed_was_pruned_is_not_a_reset(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let mut first =
            open_stream(&router, &stream_uri(&ws, ""), Auth::Session(&owner), None).await;
        inv_store(
            &pool,
            &ws,
            (0..3)
                .map(|_| inv_record("a@example.com", "seen", None, 1, json!({})))
                .collect(),
        )
        .await;
        let frames = first.take(3).await;
        let last = frames[2].id.clone().unwrap();
        drop(first);
        sqlx::query("DELETE FROM debugger_feed")
            .execute(&pool)
            .await
            .unwrap();

        let mut again = open_stream(
            &router,
            &stream_uri(&ws, ""),
            Auth::Session(&owner),
            Some(&last),
        )
        .await;

        assert!(
            again
                .next(std::time::Duration::from_millis(500))
                .await
                .is_none(),
            "no gap frame: the feed was pruned, not reset"
        );
        inv_store(
            &pool,
            &ws,
            vec![inv_record("a@example.com", "seen", None, 1, json!({}))],
        )
        .await;
        let live = again.take(1).await;
        assert_eq!(live[0].event, "item");
    }

    // ---- The AI provider layer ----

    const AI_KEY: &str = "sk-live-hunter2-0123456789";
    const AI_PROMPT: &str = "PROMPT-MARKER-4f1c9a";

    fn ai_model(name: &str, context: u32) -> crate::AiModelConfig {
        crate::AiModelConfig {
            name: name.to_owned(),
            context_window_tokens: context,
            input_price_per_mtok: 3_000_000,
            output_price_per_mtok: 15_000_000,
        }
    }

    fn ai_config(local_port: Option<u16>, rate_limit: u32) -> crate::AiConfig {
        let mut providers = vec![crate::AiProviderConfig {
            id: "openai".into(),
            kind: zirv_flow_ai::ProviderKind::OpenAiCompatible,
            base_url: "https://api.openai.com/v1".into(),
            models: vec![ai_model("big", 128_000), ai_model("small", 8_192)],
        }];
        if let Some(port) = local_port {
            providers.push(crate::AiProviderConfig {
                id: "local".into(),
                kind: zirv_flow_ai::ProviderKind::OpenAiCompatible,
                base_url: format!("http://127.0.0.1:{port}/v1"),
                models: vec![ai_model("local-model", 65_536)],
            });
        }
        crate::AiConfig {
            providers,
            rate_limit_per_user_per_minute: rate_limit,
            max_repair_attempts: 2,
        }
    }

    fn probe_ok() -> Result<zirv_flow_ai::ChatResponse, zirv_flow_ai::AiError> {
        Ok(zirv_flow_ai::fake::tool_call_response(
            "probe",
            json!({"status": "ready"}),
            20,
            5,
        ))
    }

    /// A router and state whose AI calls go to `fake` (when given) or to the real adapters.
    async fn ai_state(
        pool: &PgPool,
        fake: Option<&Arc<zirv_flow_ai::fake::FakeProvider>>,
        config: crate::AiConfig,
        egress: &[&str],
    ) -> AppState {
        zirv_flow_store::migrate(pool).await.unwrap();
        let keys = Arc::new(
            zirv_flow_core::Keyring::new(zirv_flow_core::MasterKey::generate(), None).unwrap(),
        );
        let root = zirv_flow_core::HmacRoot::bootstrap_from(keys.current());
        let sealed = zirv_flow_core::seal(
            &keys,
            root.to_secret_bytes().expose(),
            &zirv_flow_core::deployment_key_aad(zirv_flow_store::secrets::HMAC_ROOT),
        );
        zirv_flow_store::secrets::insert_hmac_root_if_absent(pool, &sealed)
            .await
            .unwrap();
        let allow: Vec<String> = egress.iter().map(|entry| (*entry).to_owned()).collect();
        let mut state = AppState::new(pool.clone(), Arc::new(String::new))
            .with_hmac_root(&root)
            .with_keyring(keys)
            .with_egress_policy(Arc::new(
                zirv_flow_core::egress::EgressPolicy::parse(&allow).unwrap(),
            ))
            .with_ai_config(config);
        if let Some(fake) = fake {
            state.ai = state.ai.clone().with_replacement(Arc::clone(fake) as _);
        }
        state
    }

    fn ai_uri(ws: &str, tail: &str) -> String {
        format!("/api/v1/workspaces/{ws}/ai/{tail}")
    }

    fn ai_put(overrides: Value) -> Value {
        let mut body = json!({
            "enabled": true,
            "provider_id": "openai",
            "model": "big",
            "monthly_budget_micros": 1_000_000,
            "pii_opt_in": false,
            "transcript_days": null,
            "provider_key": AI_KEY,
        });
        for (key, value) in overrides.as_object().unwrap() {
            body[key] = value.clone();
        }
        body
    }

    async fn put_ai(router: &Router, owner: &Login, ws: &str, body: Value) -> Reply {
        call(
            router,
            "PUT",
            &ai_uri(ws, "settings"),
            Some(body),
            Auth::Session(owner),
        )
        .await
    }

    /// Every table of the schema as text, the way a dump would show it.
    async fn dump_text(pool: &PgPool) -> String {
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT tablename::text FROM pg_tables WHERE schemaname = current_schema()",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        let mut text = String::new();
        for table in tables {
            let rows: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT string_agg(t::text, E'\\n') FROM \"{table}\" t"
            )))
            .fetch_one(pool)
            .await
            .unwrap();
            text.push_str(&rows.unwrap_or_default());
        }
        text
    }

    async fn ai_calls(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM ai_calls")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn owner_id(pool: &PgPool) -> UserId {
        let email = zirv_flow_core::Email::parse(OWNER_EMAIL).unwrap();
        zirv_flow_store::find_user_by_email(pool, &email)
            .await
            .unwrap()
            .unwrap()
            .id
    }

    /// A workspace with the assistant enabled on `fake`, its probe consumed.
    async fn enabled_workspace(
        pool: &PgPool,
        fake: &Arc<zirv_flow_ai::fake::FakeProvider>,
        config: crate::AiConfig,
        overrides: Value,
    ) -> (AppState, Router, Login, String) {
        let state = ai_state(pool, Some(fake), config, &[]).await;
        let router = router(state.clone());
        let owner = owner(pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        fake.push(probe_ok());
        let reply = put_ai(&router, &owner, &ws, ai_put(overrides)).await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        (state, router, owner, ws)
    }

    fn mapping_request(
        privacy: zirv_flow_ai::Privacy,
        personal: bool,
    ) -> Result<zirv_flow_ai::ChatRequest, zirv_flow_ai::AiError> {
        let content = if personal {
            zirv_flow_ai::Content::Personal(format!("{AI_PROMPT} ada@example.com"))
        } else {
            zirv_flow_ai::Content::Schema(format!("{AI_PROMPT} columns: Email, Name"))
        };
        Ok(zirv_flow_ai::RequestBuilder::new(privacy)
            .user(vec![content])?
            .build())
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn ai_settings_start_off_and_are_admin_only_and_the_models_list_the_allowlist(
        pool: PgPool,
    ) {
        let state = ai_state(&pool, None, ai_config(None, 20), &[]).await;
        let router = router(state);
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;

        let defaults = call(
            &router,
            "GET",
            &ai_uri(&ws, "settings"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let models = call(
            &router,
            "GET",
            &ai_uri(&ws, "models"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let usage = call(
            &router,
            "GET",
            &ai_uri(&ws, "usage"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let mut member_replies = Vec::new();
        for (method, tail, body) in [
            ("GET", "settings", None),
            ("PUT", "settings", Some(ai_put(json!({})))),
            ("GET", "models", None),
            ("GET", "usage", None),
        ] {
            member_replies.push(
                call(
                    &router,
                    method,
                    &ai_uri(&ws, tail),
                    body,
                    Auth::Session(&member),
                )
                .await,
            );
        }
        let anonymous = call(
            &router,
            "GET",
            &ai_uri(&ws, "settings"),
            None,
            Auth::Anonymous,
        )
        .await;

        assert_eq!(defaults.status, StatusCode::OK);
        assert_eq!(defaults.body["enabled"], false);
        assert_eq!(defaults.body["provider_key_set"], false);
        assert_eq!(defaults.body["monthly_budget_micros"], 0);
        assert_eq!(defaults.body["pii_opt_in"], false);
        assert!(defaults.body["transcript_days"].is_null());
        assert_eq!(models.status, StatusCode::OK);
        assert_eq!(models.body["min_context_window_tokens"], 32768);
        let models_list = &models.body["providers"][0]["models"];
        assert_eq!(models_list[0]["name"], "big");
        assert_eq!(models_list[0]["meets_minimum_context"], true);
        assert_eq!(models_list[1]["meets_minimum_context"], false);
        assert_eq!(usage.status, StatusCode::OK);
        assert_eq!(usage.body["spent_micros"], 0);
        for reply in &member_replies {
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{:?}", reply.body);
        }
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn enabling_probes_the_model_and_the_key_is_never_echoed_logged_or_stored_in_plaintext(
        pool: PgPool,
    ) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (_state, router, owner, ws) = enabled_workspace(
            &pool,
            &fake,
            ai_config(None, 20),
            json!({"transcript_days": 7, "pii_opt_in": true}),
        )
        .await;

        let got = call(
            &router,
            "GET",
            &ai_uri(&ws, "settings"),
            None,
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(fake.call_count(), 1);
        let probe = &fake.requests()[0];
        assert_eq!(probe.tools()[0].name, "probe");
        assert_eq!(got.body["enabled"], true);
        assert_eq!(got.body["provider_key_set"], true);
        assert_eq!(got.body["transcript_days"], 7);
        assert_eq!(got.body["last_probe"]["ok"], true);
        assert!(got.body.get("provider_key").is_none());
        let dump = dump_text(&pool).await;
        assert!(!dump.contains(AI_KEY), "the key appears in plaintext");
        assert!(!got.body.to_string().contains(AI_KEY));
        let sealed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM encrypted_secrets WHERE purpose = 'ai_provider_key'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(sealed, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_model_without_tool_calls_usage_or_a_big_enough_window_is_refused_with_the_reason(
        pool: PgPool,
    ) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let state = ai_state(&pool, Some(&fake), ai_config(None, 20), &[]).await;
        let router = router(state);
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;

        fake.push(Ok(zirv_flow_ai::fake::text_response("ready", 20, 5)));
        let no_tools = put_ai(&router, &owner, &ws, ai_put(json!({}))).await;
        fake.push(Err(zirv_flow_ai::AiError::Capability("usage".into())));
        let no_usage = put_ai(&router, &owner, &ws, ai_put(json!({}))).await;
        let before = fake.call_count();
        let small = put_ai(&router, &owner, &ws, ai_put(json!({"model": "small"}))).await;
        let after = fake.call_count();
        let got = call(
            &router,
            "GET",
            &ai_uri(&ws, "settings"),
            None,
            Auth::Session(&owner),
        )
        .await;

        for (reply, field) in [
            (&no_tools, "tool_calls"),
            (&no_usage, "usage"),
            (&small, "context_window"),
        ] {
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{:?}",
                reply.body
            );
            assert_eq!(reply.body["code"], "ai_capability_missing");
            assert_eq!(reply.body["errors"][0]["field"], field, "{:?}", reply.body);
            assert!(
                !reply.body["errors"][0]["reason"]
                    .as_str()
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(
            small.body["errors"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("8192")
        );
        // A window below 32k is refused without calling the model at all.
        assert_eq!(before, after);
        // Nothing was saved, and no secret was stored for a refused enablement.
        assert_eq!(got.body["enabled"], false);
        assert_eq!(got.body["provider_key_set"], false);
        assert!(!dump_text(&pool).await.contains(AI_KEY));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn invalid_settings_are_422s_naming_the_field(pool: PgPool) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let state = ai_state(&pool, Some(&fake), ai_config(None, 20), &[]).await;
        let router = router(state);
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;

        let cases = [
            (json!({"model": "nope"}), "model"),
            (json!({"provider_id": "nope"}), "model"),
            (json!({"model": null}), "provider_id and model"),
            (json!({"provider_id": null, "model": null}), "model"),
            (
                json!({"monthly_budget_micros": -5}),
                "monthly_budget_micros",
            ),
            (json!({"transcript_days": 31}), "transcript_days"),
            (json!({"transcript_days": 0}), "transcript_days"),
            (json!({"provider_key": "  "}), "provider_key"),
            (json!({"provider_key": null}), "provider_key"),
        ];
        for (overrides, field) in cases {
            let reply = put_ai(&router, &owner, &ws, ai_put(overrides.clone())).await;
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{overrides}"
            );
            assert!(
                reply.body["detail"].as_str().unwrap().starts_with(field),
                "{overrides}: {:?}",
                reply.body
            );
        }
        assert_eq!(fake.call_count(), 0);
        let unknown = call(
            &router,
            "PUT",
            &ai_uri(&ws, "settings"),
            Some(json!({"enabled": false, "extra": 1})),
            Auth::Session(&owner),
        )
        .await;
        assert!(unknown.status.is_client_error());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn changing_settings_is_audited_with_the_changed_fields_and_never_the_key(pool: PgPool) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (_state, router, owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 20), json!({})).await;
        // A transcript window, a budget change and a new key; the model is probed again for the key.
        fake.push(probe_ok());
        let changed = put_ai(
            &router,
            &owner,
            &ws,
            ai_put(json!({
                "transcript_days": 14,
                "monthly_budget_micros": 2_000_000,
                "provider_key": "sk-new-key-0000-hunter3",
            })),
        )
        .await;
        // Unchanged settings without a key do not probe and are not audited again.
        let again = put_ai(
            &router,
            &owner,
            &ws,
            ai_put(json!({
                "transcript_days": 14,
                "monthly_budget_micros": 2_000_000,
                "provider_key": null,
            })),
        )
        .await;
        let off = put_ai(
            &router,
            &owner,
            &ws,
            ai_put(json!({
                "enabled": false,
                "transcript_days": null,
                "monthly_budget_micros": 2_000_000,
                "provider_key": null,
            })),
        )
        .await;

        assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.body);
        assert_eq!(again.status, StatusCode::OK, "{:?}", again.body);
        assert_eq!(off.status, StatusCode::OK, "{:?}", off.body);
        assert_eq!(fake.call_count(), 2, "only enabling and the new key probe");
        let entries = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        let updates: Vec<&zirv_flow_store::AuditEntry> = entries
            .iter()
            .filter(|entry| entry.action == "ai.settings.updated")
            .collect();
        assert_eq!(updates.len(), 3, "{updates:?}");
        // Newest first: switched off, then the transcript window and budget and key, then enabling.
        assert_eq!(
            updates[0].summary["changed"],
            json!(["enabled", "transcript_days"])
        );
        assert_eq!(updates[0].summary["values"]["enabled"], false);
        assert!(updates[0].summary["values"]["transcript_days"].is_null());
        let changed_fields = updates[1].summary["changed"].as_array().unwrap();
        for field in ["monthly_budget_micros", "transcript_days", "provider_key"] {
            assert!(changed_fields.contains(&json!(field)), "{field}");
        }
        assert_eq!(updates[1].summary["values"]["transcript_days"], 14);
        assert!(updates[1].summary["values"].get("provider_key").is_none());
        assert!(
            updates
                .iter()
                .all(|entry| entry.object_kind.as_deref() == Some("ai_settings"))
        );
        assert!(updates.iter().all(|entry| entry.actor_kind == "user"));
        let text = format!("{entries:?}");
        assert!(!text.contains("hunter"), "{text}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dispatched_call_leaves_one_content_free_row_with_tokens_and_cost(pool: PgPool) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, _router, _owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 20), json!({})).await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let user = owner_id(&pool).await;
        fake.push(Ok(zirv_flow_ai::fake::text_response(
            "RESPONSE-MARKER-77",
            1_000,
            500,
        )));

        let response = crate::dispatch(&state, Some(user), workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await
        .unwrap();

        assert_eq!(response.text(), "RESPONSE-MARKER-77");
        assert_eq!(ai_calls(&pool).await, 1);
        let (feature, provider, model, input, output, cost, outcome, user_id): (
            String,
            String,
            String,
            i32,
            i32,
            i64,
            String,
            Option<uuid::Uuid>,
        ) = sqlx::query_as(
            "SELECT feature, provider_id, model, input_tokens, output_tokens, \
                    estimated_cost_micros, outcome, user_id FROM ai_calls",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (feature.as_str(), provider.as_str(), model.as_str()),
            ("mapping", "openai", "big")
        );
        assert_eq!((input, output), (1_000, 500));
        // 1000 input tokens at 3.00 per million plus 500 output tokens at 15.00 per million.
        assert_eq!(cost, 3_000 + 7_500);
        assert_eq!(outcome, "ok");
        assert_eq!(user_id, Some(user.as_uuid()));
        let log: String = sqlx::query_scalar("SELECT string_agg(t::text, ' ') FROM ai_calls t")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            !log.contains(AI_PROMPT) && !log.contains("RESPONSE-MARKER"),
            "{log}"
        );
        assert!(!log.contains("Email, Name"), "{log}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failed_call_is_logged_with_its_outcome_and_no_cost_and_the_error_is_a_502(
        pool: PgPool,
    ) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, _router, _owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 20), json!({})).await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        fake.push(Err(zirv_flow_ai::AiError::Http(503)));

        let error = crate::dispatch(&state, None, workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await
        .unwrap_err();

        let problem = crate::problem::ApiError::from(error);
        assert_eq!(problem.status_code(), StatusCode::BAD_GATEWAY);
        let (outcome, cost, user_id): (String, i64, Option<uuid::Uuid>) =
            sqlx::query_as("SELECT outcome, estimated_cost_micros, user_id FROM ai_calls")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((outcome.as_str(), cost, user_id), ("http_error", 0, None));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn crossing_the_budget_stops_the_call_before_dispatch_and_records_nothing(pool: PgPool) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, router, owner, ws) = enabled_workspace(
            &pool,
            &fake,
            ai_config(None, 20),
            json!({"monthly_budget_micros": 38_000}),
        )
        .await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let user = owner_id(&pool).await;
        let sent = fake.call_count();
        // 10_500 per call, and a call is admitted only while the spend plus its worst case (about
        // 19_000) fits the budget: two calls fit 38_000, a third would not.
        for _ in 0..2 {
            fake.push(Ok(zirv_flow_ai::fake::text_response("ok", 1_000, 500)));
            crate::dispatch(&state, Some(user), workspace, "mapping", |privacy| {
                mapping_request(privacy, false)
            })
            .await
            .unwrap();
        }
        let rows = ai_calls(&pool).await;

        let refused = crate::dispatch(&state, Some(user), workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await
        .unwrap_err();

        assert_eq!(
            fake.call_count(),
            sent + 2,
            "no request was made once the budget was spent"
        );
        assert_eq!(ai_calls(&pool).await, rows, "a refusal records nothing");
        let problem = crate::problem::ApiError::from(refused);
        assert_eq!(problem.status_code(), StatusCode::PAYMENT_REQUIRED);
        assert_eq!(
            problem.detail(),
            "the workspace's monthly AI budget is spent"
        );
        // The usage page shows the spend against the budget.
        let usage = call(
            &router,
            "GET",
            &ai_uri(&ws, "usage"),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(usage.body["spent_micros"], 21_000);
        assert_eq!(usage.body["budget_micros"], 38_000);
        assert_eq!(usage.body["by_feature"][0]["feature"], "mapping");
        assert_eq!(usage.body["by_feature"][0]["calls"], 2);
        assert_eq!(usage.body["by_user"][0]["calls"], 2);
        assert_eq!(usage.body["recent"].as_array().unwrap().len(), 2);
        assert!(!usage.body.to_string().contains(AI_PROMPT));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_zero_budget_allows_no_calls(pool: PgPool) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, _router, _owner, ws) = enabled_workspace(
            &pool,
            &fake,
            ai_config(None, 20),
            json!({"monthly_budget_micros": 0}),
        )
        .await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let sent = fake.call_count();

        let refused = crate::dispatch(&state, None, workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await;

        assert!(matches!(
            refused,
            Err(crate::DispatchError::Ai(
                zirv_flow_ai::AiError::BudgetExceeded
            ))
        ));
        assert_eq!((fake.call_count(), ai_calls(&pool).await), (sent, 0));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_user_over_the_rate_limit_is_refused_before_dispatch_and_others_are_not(
        pool: PgPool,
    ) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, router, owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 2), json!({})).await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let (other_id, _other) = user(&pool, &router, "other@example.com").await;
        let _ = owner;
        let ada = owner_id(&pool).await;
        let sent = fake.call_count();
        for _ in 0..2 {
            fake.push(Ok(zirv_flow_ai::fake::text_response("ok", 10, 10)));
            crate::dispatch(&state, Some(ada), workspace, "mapping", |privacy| {
                mapping_request(privacy, false)
            })
            .await
            .unwrap();
        }

        let refused = crate::dispatch(&state, Some(ada), workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await
        .unwrap_err();
        fake.push(Ok(zirv_flow_ai::fake::text_response("ok", 10, 10)));
        let other = crate::dispatch(&state, Some(other_id), workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await;

        assert_eq!(fake.call_count(), sent + 3, "the refused call sent nothing");
        assert_eq!(ai_calls(&pool).await, 3);
        let problem = crate::problem::ApiError::from(refused);
        assert_eq!(problem.status_code(), StatusCode::TOO_MANY_REQUESTS);
        assert!(other.is_ok(), "the limit is per user");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn personal_content_is_refused_without_the_opt_in_and_sent_with_it(pool: PgPool) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, router, owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 20), json!({})).await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let sent = fake.call_count();

        let refused = crate::dispatch(&state, None, workspace, "mapping", |privacy| {
            mapping_request(privacy, true)
        })
        .await
        .unwrap_err();

        assert_eq!(fake.call_count(), sent, "no request was built or sent");
        assert_eq!(ai_calls(&pool).await, 0);
        let problem = crate::problem::ApiError::from(refused);
        assert_eq!(problem.status_code(), StatusCode::UNPROCESSABLE_ENTITY);

        // The admin opts in; the same request now goes out, labelled personal.
        let opted = put_ai(
            &router,
            &owner,
            &ws,
            ai_put(json!({"pii_opt_in": true, "provider_key": null})),
        )
        .await;
        assert_eq!(opted.status, StatusCode::OK, "{:?}", opted.body);
        fake.push(Ok(zirv_flow_ai::fake::text_response("ok", 10, 10)));
        crate::dispatch(&state, None, workspace, "mapping", |privacy| {
            mapping_request(privacy, true)
        })
        .await
        .unwrap();
        let request = fake.requests().pop().unwrap();
        assert!(request.contents().any(zirv_flow_ai::Content::is_personal));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_workspace_that_is_off_or_whose_model_left_the_allowlist_is_disabled(pool: PgPool) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, router, owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 20), json!({})).await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let sent = fake.call_count();
        let mut trimmed = (*state.ai.config).clone();
        trimmed.providers.clear();
        let mut gone = state.clone();
        gone.ai = crate::assistant::runtime::AiRuntime::new(trimmed)
            .with_replacement(Arc::clone(&fake) as _);

        let removed = crate::dispatch(&gone, None, workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await;
        let off = put_ai(
            &router,
            &owner,
            &ws,
            ai_put(json!({"enabled": false, "provider_key": null})),
        )
        .await;
        let disabled = crate::dispatch(&state, None, workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await;
        let unknown = crate::dispatch(
            &state,
            None,
            zirv_flow_core::WorkspaceId::new(),
            "mapping",
            |privacy| mapping_request(privacy, false),
        )
        .await;

        assert_eq!(off.status, StatusCode::OK);
        for refused in [removed, disabled, unknown] {
            assert!(matches!(
                refused,
                Err(crate::DispatchError::Ai(zirv_flow_ai::AiError::Disabled))
            ));
        }
        assert_eq!((fake.call_count(), ai_calls(&pool).await), (sent, 0));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn transcripts_are_stored_only_when_opted_in_and_expire_after_the_window(pool: PgPool) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, router, owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 20), json!({})).await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        fake.push(Ok(zirv_flow_ai::fake::text_response(
            "RESPONSE-MARKER-77",
            10,
            10,
        )));
        crate::dispatch(&state, None, workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await
        .unwrap();
        let without: i64 = sqlx::query_scalar("SELECT count(*) FROM ai_transcripts")
            .fetch_one(&pool)
            .await
            .unwrap();

        let opted = put_ai(
            &router,
            &owner,
            &ws,
            ai_put(json!({"transcript_days": 3, "provider_key": null})),
        )
        .await;
        assert_eq!(opted.status, StatusCode::OK, "{:?}", opted.body);
        fake.push(Ok(zirv_flow_ai::fake::text_response(
            "RESPONSE-MARKER-77",
            10,
            10,
        )));
        crate::dispatch(&state, None, workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await
        .unwrap();

        assert_eq!(without, 0);
        let (days, content): (f64, serde_json::Value) = sqlx::query_as(
            "SELECT (extract(epoch FROM expires_at - created_at) / 86400)::float8, content \
             FROM ai_transcripts",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!((days - 3.0).abs() < 0.01, "{days}");
        let text = content.to_string();
        assert!(
            text.contains(AI_PROMPT) && text.contains("RESPONSE-MARKER-77"),
            "{text}"
        );
        assert!(!text.contains(AI_KEY));
        // The call log still holds no content, with or without the transcript.
        let log: String = sqlx::query_scalar("SELECT string_agg(t::text, ' ') FROM ai_calls t")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!log.contains(AI_PROMPT));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_repair_attempt_is_its_own_dispatch_and_passes_the_gates(pool: PgPool) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, _router, _owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 2), json!({})).await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let user = owner_id(&pool).await;
        let dispatcher = crate::Dispatcher::open(&state, Some(user), workspace, "mapping")
            .await
            .unwrap();
        let request = mapping_request(dispatcher.privacy(), false).unwrap();
        fn wants_ok(
            response: &zirv_flow_ai::ChatResponse,
        ) -> zirv_flow_ai::BoxFuture<'_, zirv_flow_ai::Verdict<(), String, ()>> {
            let verdict = match response.text().as_str() {
                "ok" => zirv_flow_ai::Verdict::Valid(()),
                other => zirv_flow_ai::Verdict::Invalid(vec![format!("expected ok, got {other}")]),
            };
            Box::pin(async move { verdict })
        }
        let sent = fake.call_count();
        for word in ["bad", "worse", "worst"] {
            fake.push(Ok(zirv_flow_ai::fake::text_response(word, 10, 10)));
        }

        // Two calls fit the per-minute limit, so the third repair attempt is refused at the gate.
        let outcome = zirv_flow_ai::repair(5, &request, &dispatcher, wants_ok).await;

        assert_eq!(
            outcome,
            Err(zirv_flow_ai::RepairError::<()>::Dispatch(
                zirv_flow_ai::AiError::RateLimited
            ))
        );
        assert_eq!(fake.call_count(), sent + 2);
        assert_eq!(ai_calls(&pool).await, 2);
        let second = &fake.requests()[sent + 1];
        assert!(second.messages().len() > request.messages().len());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_local_model_server_on_a_private_address_is_blocked_until_it_is_allowlisted(
        pool: PgPool,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let server = Router::new().fallback(axum::routing::post(move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({
                    "model": "local-model",
                    "choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [
                        {"id": "c1", "type": "function",
                         "function": {"name": "probe", "arguments": "{\"status\":\"ready\"}"}}]}}],
                    "usage": {"prompt_tokens": 12, "completion_tokens": 4}
                }))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
        let body = ai_put(json!({"provider_id": "local", "model": "local-model"}));

        let blocked_state = ai_state(&pool, None, ai_config(Some(port), 20), &[]).await;
        let blocked_router = router(blocked_state.clone());
        let owner = owner(&pool, &blocked_router).await;
        let ws = workspace(&blocked_router, &owner, "Alpha").await;
        let blocked = put_ai(&blocked_router, &owner, &ws, body.clone()).await;
        let blocked_hits = hits.load(Ordering::SeqCst);

        // The same deployment with the loopback address allowlisted (and so a fresh client).
        let allow = zirv_flow_core::egress::EgressPolicy::parse(&["127.0.0.1/32".to_owned()]);
        let allowed_state = blocked_state
            .with_egress_policy(Arc::new(allow.unwrap()))
            .with_ai_config(ai_config(Some(port), 20));
        let allowed_router = router(allowed_state);
        let allowed = put_ai(&allowed_router, &owner, &ws, body).await;

        assert_eq!(
            blocked.status,
            StatusCode::BAD_GATEWAY,
            "{:?}",
            blocked.body
        );
        assert_eq!(blocked.body["code"], "ai_provider_error");
        assert!(
            blocked.body["detail"]
                .as_str()
                .unwrap()
                .contains("loopback"),
            "{:?}",
            blocked.body
        );
        assert_eq!(blocked_hits, 0, "the server was never contacted");
        let got = call(
            &blocked_router,
            "GET",
            &ai_uri(&ws, "settings"),
            None,
            Auth::Session(&owner),
        )
        .await;
        assert_eq!(
            got.body["enabled"], true,
            "the second router's save is visible"
        );
        assert_eq!(got.body["provider_id"], "local");
        assert_eq!(allowed.status, StatusCode::OK, "{:?}", allowed.body);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(!dump_text(&pool).await.contains(AI_KEY));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn ai_data_is_isolated_per_workspace(pool: PgPool) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, router, owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 20), json!({})).await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let second = self::workspace(&router, &owner, "Beta").await;
        fake.push(Ok(zirv_flow_ai::fake::text_response("ok", 100, 100)));
        crate::dispatch(&state, None, workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await
        .unwrap();

        let other_settings = call(
            &router,
            "GET",
            &ai_uri(&second, "settings"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let other_usage = call(
            &router,
            "GET",
            &ai_uri(&second, "usage"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let own_usage = call(
            &router,
            "GET",
            &ai_uri(&ws, "usage"),
            None,
            Auth::Session(&owner),
        )
        .await;
        let refused = crate::dispatch(
            &state,
            None,
            second.parse().unwrap(),
            "mapping",
            |privacy| mapping_request(privacy, false),
        )
        .await;

        assert_eq!(other_settings.body["enabled"], false);
        assert_eq!(other_settings.body["provider_key_set"], false);
        assert_eq!(other_usage.body["spent_micros"], 0);
        assert_eq!(other_usage.body["recent"].as_array().unwrap().len(), 0);
        assert_eq!(own_usage.body["recent"].as_array().unwrap().len(), 1);
        assert!(matches!(
            refused,
            Err(crate::DispatchError::Ai(zirv_flow_ai::AiError::Disabled))
        ));
        // A key of another workspace cannot read or write these.
        let (_id, key) = make_key(&router, &owner, &second, &["admin"]).await;
        let cross = call(
            &router,
            "GET",
            &ai_uri(&ws, "settings"),
            None,
            Auth::Key(&key),
        )
        .await;
        assert_eq!(cross.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn calls_and_refusals_are_counted_by_feature_outcome_and_reason(pool: PgPool) {
        use metrics_exporter_prometheus::PrometheusBuilder;

        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, _router, _owner, ws) = enabled_workspace(
            &pool,
            &fake,
            ai_config(None, 20),
            json!({"monthly_budget_micros": 25_000}),
        )
        .await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = metrics::set_default_local_recorder(&recorder);
        fake.push(Ok(zirv_flow_ai::fake::text_response("ok", 1_000, 500)));
        fake.push(Err(zirv_flow_ai::AiError::Timeout));

        let first = crate::dispatch(&state, None, workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await;
        let second = crate::dispatch(&state, None, workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await;

        assert!(first.is_ok());
        assert!(second.is_err());
        let output = handle.render();
        for expected in [
            r#"zirv_flow_ai_calls_total{feature="mapping",outcome="ok"} 1"#,
            r#"zirv_flow_ai_refusals_total{reason="budget_exceeded"} 1"#,
        ] {
            assert!(output.contains(expected), "{expected} in {output}");
        }
    }

    /// Runs `count` dispatches at once against a provider that takes a moment to answer, and
    /// returns how many went through and how many each gate refused.
    async fn dispatch_together(
        state: &AppState,
        user: Option<UserId>,
        workspace: zirv_flow_core::WorkspaceId,
        count: usize,
    ) -> (usize, usize, usize) {
        let results = futures_util::future::join_all((0..count).map(|_| {
            crate::dispatch(state, user, workspace, "mapping", |privacy| {
                mapping_request(privacy, false)
            })
        }))
        .await;
        let mut counts = (0, 0, 0);
        for result in results {
            match result {
                Ok(_) => counts.0 += 1,
                Err(crate::DispatchError::Ai(zirv_flow_ai::AiError::RateLimited)) => counts.1 += 1,
                Err(crate::DispatchError::Ai(zirv_flow_ai::AiError::BudgetExceeded)) => {
                    counts.2 += 1;
                }
                Err(other) => panic!("unexpected {other:?}"),
            }
        }
        counts
    }

    fn slow_fake(answers: usize) -> Arc<zirv_flow_ai::fake::FakeProvider> {
        let fake = Arc::new(
            zirv_flow_ai::fake::FakeProvider::new()
                .with_delay(std::time::Duration::from_millis(400)),
        );
        for _ in 0..answers {
            fake.push(Ok(zirv_flow_ai::fake::text_response("ok", 10, 10)));
        }
        fake
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn concurrent_calls_cannot_pass_the_rate_limit_together(pool: PgPool) {
        let fake = slow_fake(0);
        let (state, _router, _owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 2), json!({})).await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let user = owner_id(&pool).await;
        let sent = fake.call_count();
        for _ in 0..5 {
            fake.push(Ok(zirv_flow_ai::fake::text_response("ok", 10, 10)));
        }

        let (ok, rate_limited, over_budget) =
            dispatch_together(&state, Some(user), workspace, 5).await;

        assert_eq!(
            (ok, rate_limited, over_budget),
            (2, 3, 0),
            "exactly the limit reached the provider"
        );
        assert_eq!(fake.call_count(), sent + 2);
        assert_eq!(ai_calls(&pool).await, 2, "refusals record nothing");
        let pending: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ai_calls WHERE outcome = 'pending'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pending, 0, "every reservation was finished");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn concurrent_calls_cannot_spend_past_the_budget_together(pool: PgPool) {
        let fake = slow_fake(0);
        let config = ai_config(None, 20);
        let request = mapping_request(zirv_flow_ai::Privacy::SchemaOnly, false).unwrap();
        let model = config.model("openai", "big").unwrap().1.clone();
        let worst_case = model.cost_micros(
            request.input_tokens_upper_bound(),
            request.max_output_tokens(),
        );
        // A budget that three worst-case reservations spend exactly.
        let (state, _router, _owner, ws) = enabled_workspace(
            &pool,
            &fake,
            config,
            json!({"monthly_budget_micros": 3 * worst_case}),
        )
        .await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let sent = fake.call_count();
        for _ in 0..6 {
            fake.push(Ok(zirv_flow_ai::fake::text_response("ok", 10, 10)));
        }

        let (ok, rate_limited, over_budget) = dispatch_together(&state, None, workspace, 6).await;

        assert_eq!((ok, rate_limited, over_budget), (3, 0, 3));
        assert_eq!(fake.call_count(), sent + 3);
        let (calls, spent, pending): (i64, i64, i64) = sqlx::query_as(
            "SELECT count(*), COALESCE(sum(estimated_cost_micros), 0)::bigint, \
                    count(*) FILTER (WHERE outcome = 'pending') FROM ai_calls",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(calls, 3);
        assert_eq!(pending, 0);
        assert_eq!(
            spent,
            3 * model.cost_micros(10, 10),
            "the actual cost replaced the reserve"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_budget_below_one_worst_case_call_refuses_before_dispatch(pool: PgPool) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, _router, _owner, ws) = enabled_workspace(
            &pool,
            &fake,
            ai_config(None, 20),
            json!({"monthly_budget_micros": 1_000}),
        )
        .await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let sent = fake.call_count();

        let refused = crate::dispatch(&state, None, workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await;

        assert!(matches!(
            refused,
            Err(crate::DispatchError::Ai(
                zirv_flow_ai::AiError::BudgetExceeded
            ))
        ));
        assert_eq!(
            (fake.call_count(), ai_calls(&pool).await),
            (sent, 0),
            "nothing was spent, and still nothing was sent"
        );
    }

    /// Waits until no call of the log is pending.
    async fn settled(pool: &PgPool) {
        for _ in 0..100 {
            let pending: i64 =
                sqlx::query_scalar("SELECT count(*) FROM ai_calls WHERE outcome = 'pending'")
                    .fetch_one(pool)
                    .await
                    .unwrap();
            if pending == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("a call is still pending");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dispatch_dropped_mid_call_still_finishes_its_row_with_the_real_usage(pool: PgPool) {
        let fake = slow_fake(0);
        let (state, _router, _owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 20), json!({})).await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        fake.push(Ok(zirv_flow_ai::fake::text_response("ok", 10, 10)));

        let before = fake.call_count();
        // The client goes away once the provider has the request; it answers 400 ms later. A fixed
        // delay could instead drop the handler while it is still reserving the call.
        let mut handler = Box::pin(crate::dispatch(
            &state,
            None,
            workspace,
            "mapping",
            |privacy| mapping_request(privacy, false),
        ));
        let provider_has_it = async {
            while fake.call_count() == before {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        };
        tokio::select! {
            _ = &mut handler => panic!("the call finished before the handler was dropped"),
            () = provider_has_it => drop(handler),
        }
        settled(&pool).await;

        let (outcome, input, output, cost): (String, i32, i32, i64) = sqlx::query_as(
            "SELECT outcome, input_tokens, output_tokens, estimated_cost_micros FROM ai_calls",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((outcome.as_str(), input, output), ("ok", 10, 10));
        assert_eq!(cost, 3 * 10 + 15 * 10, "the real cost, not the reserve");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_transient_database_error_while_finishing_is_retried(pool: PgPool) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, _router, _owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 20), json!({})).await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        // The first update of a call row fails, as a dropped connection would.
        for statement in [
            "CREATE SEQUENCE fail_once",
            "CREATE FUNCTION fail_first() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN IF nextval('fail_once') = 1 THEN RAISE EXCEPTION 'transient'; END IF; \
             RETURN NEW; END $$",
            "CREATE TRIGGER fail_first BEFORE UPDATE ON ai_calls FOR EACH ROW \
             EXECUTE FUNCTION fail_first()",
        ] {
            sqlx::query(statement).execute(&pool).await.unwrap();
        }
        fake.push(Ok(zirv_flow_ai::fake::text_response("ok", 10, 10)));

        let response = crate::dispatch(&state, None, workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await;

        assert!(response.is_ok(), "{response:?}");
        let (outcome,): (String,) = sqlx::query_as("SELECT outcome FROM ai_calls")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(outcome, "ok");
        let tries: i64 = sqlx::query_scalar("SELECT last_value FROM fail_once")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tries, 2, "one failure, then the retry");
    }

    /// A provider that panics, as a bug in an adapter would.
    struct Panics;

    impl zirv_flow_ai::Provider for Panics {
        fn kind(&self) -> zirv_flow_ai::ProviderKind {
            zirv_flow_ai::ProviderKind::OpenAiCompatible
        }

        fn complete<'a>(
            &'a self,
            _http: &'a zirv_flow_ai::AiHttp,
            _endpoint: &'a zirv_flow_ai::Endpoint,
            _key: &'a zirv_flow_core::SecretString,
            _request: &'a zirv_flow_ai::ChatRequest,
        ) -> zirv_flow_ai::BoxFuture<'a, Result<zirv_flow_ai::ChatResponse, zirv_flow_ai::AiError>>
        {
            Box::pin(async { panic!("an adapter bug") })
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_panicking_provider_abandons_the_call_at_its_worst_case_cost(pool: PgPool) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, _router, _owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 20), json!({})).await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let mut broken = state.clone();
        broken.ai = crate::assistant::runtime::AiRuntime::new((*state.ai.config).clone())
            .with_replacement(Arc::new(Panics));
        let request = mapping_request(zirv_flow_ai::Privacy::SchemaOnly, false).unwrap();
        let model = state.ai.config.model("openai", "big").unwrap().1.clone();
        let worst_case = model.cost_micros(
            request.input_tokens_upper_bound(),
            request.max_output_tokens(),
        );

        let result = crate::dispatch(&broken, None, workspace, "mapping", |privacy| {
            mapping_request(privacy, false)
        })
        .await;

        assert!(result.is_err());
        let (outcome, cost): (String, i64) =
            sqlx::query_as("SELECT outcome, estimated_cost_micros FROM ai_calls")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((outcome.as_str(), cost), ("abandoned", worst_case));
    }

    /// A provider that takes the request, waits, and then panics, as an adapter bug would after
    /// the client has gone away.
    struct PanicsLater {
        received: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl zirv_flow_ai::Provider for PanicsLater {
        fn kind(&self) -> zirv_flow_ai::ProviderKind {
            zirv_flow_ai::ProviderKind::OpenAiCompatible
        }

        fn complete<'a>(
            &'a self,
            _http: &'a zirv_flow_ai::AiHttp,
            _endpoint: &'a zirv_flow_ai::Endpoint,
            _key: &'a zirv_flow_core::SecretString,
            _request: &'a zirv_flow_ai::ChatRequest,
        ) -> zirv_flow_ai::BoxFuture<'a, Result<zirv_flow_ai::ChatResponse, zirv_flow_ai::AiError>>
        {
            self.received
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                panic!("an adapter bug after the handler was dropped")
            })
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_handler_dropped_before_the_provider_panics_finishes_the_row_abandoned(pool: PgPool) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, _router, _owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 20), json!({})).await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let received = Arc::new(AtomicUsize::new(0));
        let mut broken = state.clone();
        broken.ai = crate::assistant::runtime::AiRuntime::new((*state.ai.config).clone())
            .with_replacement(Arc::new(PanicsLater {
                received: Arc::clone(&received),
            }));
        let request = mapping_request(zirv_flow_ai::Privacy::SchemaOnly, false).unwrap();
        let model = state.ai.config.model("openai", "big").unwrap().1.clone();
        let worst_case = model.cost_micros(
            request.input_tokens_upper_bound(),
            request.max_output_tokens(),
        );

        // The client goes away once the provider has the request; the provider panics after.
        let mut handler = Box::pin(crate::dispatch(
            &broken,
            None,
            workspace,
            "mapping",
            |privacy| mapping_request(privacy, false),
        ));
        let provider_has_it = async {
            while received.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        };
        tokio::select! {
            _ = &mut handler => panic!("the call finished before the handler was dropped"),
            () = provider_has_it => drop(handler),
        }
        settled(&pool).await;

        let (outcome, cost): (String, i64) =
            sqlx::query_as("SELECT outcome, estimated_cost_micros FROM ai_calls")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((outcome.as_str(), cost), ("abandoned", worst_case));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dispatch_dropped_at_any_early_poll_leaves_no_pending_row(pool: PgPool) {
        use std::future::Future;
        use std::task::Poll;

        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, _router, _owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 1000), json!({})).await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let dispatcher = crate::Dispatcher::open(&state, None, workspace, "mapping")
            .await
            .unwrap();
        let request = mapping_request(dispatcher.privacy(), false).unwrap();
        let draft = uuid::Uuid::now_v7();
        let dispatcher = dispatcher.for_draft(draft);
        let sent = fake.call_count();

        for polls in 0..=3 {
            fake.push(Ok(zirv_flow_ai::fake::text_response("ok", 10, 10)));
            let mut call = Box::pin(dispatcher.call(&request));
            for _ in 0..polls {
                std::future::poll_fn(|cx| {
                    let _ = call.as_mut().poll(cx);
                    Poll::Ready(())
                })
                .await;
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            drop(call);
        }
        // Let every started flight run to its end.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        settled(&pool).await;

        let rows = ai_calls(&pool).await;
        let provider_calls = i64::try_from(fake.call_count() - sent).unwrap();
        assert_eq!(rows, provider_calls, "every reserved row has its flight");
        assert_eq!(rows, 3, "a call never polled starts nothing");
        let linked: i64 = sqlx::query_scalar("SELECT count(*) FROM ai_calls WHERE draft_id = $1")
            .bind(draft)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(linked, rows, "for_draft links every call");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_key_that_cannot_be_opened_finishes_the_row_at_no_cost(pool: PgPool) {
        let fake = Arc::new(zirv_flow_ai::fake::FakeProvider::new());
        let (state, _router, _owner, ws) =
            enabled_workspace(&pool, &fake, ai_config(None, 20), json!({})).await;
        let workspace: zirv_flow_core::WorkspaceId = ws.parse().unwrap();
        let dispatcher = crate::Dispatcher::open(&state, None, workspace, "mapping")
            .await
            .unwrap();
        let request = mapping_request(dispatcher.privacy(), false).unwrap();
        let sent = fake.call_count();
        // The key vanishes after the settings were read.
        sqlx::query("DELETE FROM encrypted_secrets")
            .execute(&pool)
            .await
            .unwrap();

        let result = dispatcher.call(&request).await;

        assert!(matches!(result, Err(crate::DispatchError::KeyUnusable)));
        assert_eq!(fake.call_count(), sent, "nothing was sent");
        let (outcome, cost, pending): (String, i64, i64) = sqlx::query_as(
            "SELECT outcome, estimated_cost_micros, \
                    count(*) FILTER (WHERE outcome = 'pending') OVER () FROM ai_calls",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((outcome.as_str(), cost, pending), ("transport", 0, 0));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_csv_import_refuses_a_mapping_with_a_delete_signal(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (_, body) = upload_csv(
            &router,
            &ws,
            "name=Customers",
            CUSTOMERS,
            Auth::Session(&owner),
        )
        .await;
        let id = body["id"].as_str().unwrap();
        let uri = imports_uri(&ws, &format!("/{id}/mapping"));

        let refused = call(
            &router,
            "PUT",
            &uri,
            Some(json!({"columns": [
                {"column": "Email", "target": {"type": "identifier", "kind": "email"}}],
                "delete_when": {"column": "Email"}})),
            Auth::Session(&owner),
        )
        .await;
        let accepted = call(
            &router,
            "PUT",
            &uri,
            Some(json!({"columns": [
                {"column": "Email", "target": {"type": "identifier", "kind": "email"}}]})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(refused.body["code"], "delete_signal_unsupported");
        assert_eq!(accepted.status, StatusCode::OK);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_csv_import_mapping_with_an_event_is_refused(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        let (_, body) = upload_csv(
            &router,
            &ws,
            "name=Customers",
            CUSTOMERS,
            Auth::Session(&owner),
        )
        .await;
        let id = body["id"].as_str().unwrap();
        let uri = imports_uri(&ws, &format!("/{id}/mapping"));
        let email = json!({"column": "Email", "target": {"type": "identifier", "kind": "email"}});

        let refused = call(
            &router,
            "PUT",
            &uri,
            Some(json!({"columns": [email], "event": {"name": "order_placed"}})),
            Auth::Session(&owner),
        )
        .await;
        let plain = call(
            &router,
            "PUT",
            &uri,
            Some(json!({"columns": [email]})),
            Auth::Session(&owner),
        )
        .await;

        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(refused.body["detail"], "mapping: event_not_supported");
        assert_eq!(plain.status, StatusCode::OK, "{:?}", plain.body);
    }

    // ---- Segments with versions, cycle rejection and preview (#120) ----

    fn segments_uri(ws: &str) -> String {
        format!("/api/v1/workspaces/{ws}/segments")
    }

    fn plan_is(plan: &str) -> Value {
        json!({"v": 1, "trait": "plan", "op": "eq", "value": plan})
    }

    fn refers_to(id: &str) -> Value {
        json!({"v": 1, "segment": id})
    }

    async fn define_plan(pool: &PgPool, ws: &str) {
        zirv_flow_store::create_trait_definition(
            pool,
            ws.parse().unwrap(),
            &zirv_flow_store::NewTraitDefinition {
                key: "plan".into(),
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
    }

    async fn make_segment(
        router: &Router,
        who: Auth<'_>,
        ws: &str,
        name: &str,
        definition: Value,
    ) -> Reply {
        call(
            router,
            "POST",
            &segments_uri(ws),
            Some(json!({"name": name, "definition": definition})),
            who,
        )
        .await
    }

    async fn plan_people(pool: &PgPool, ws: &str, people: &[(&str, &str)]) {
        let records: Vec<_> = people
            .iter()
            .map(|(email, plan)| {
                let mut record = profile(email, &[("email", email)]);
                record.traits = json!({"plan": plan}).as_object().cloned().unwrap();
                record
            })
            .collect();
        zirv_flow_store::apply_profile_batch(pool, ws.parse().unwrap(), &records)
            .await
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_member_saves_versions_and_a_bad_definition_is_a_422_with_pointers_and_each_write_is_audited(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        define_plan(&pool, &ws).await;
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;

        let created =
            make_segment(&router, Auth::Session(&member), &ws, "Pro", plan_is("pro")).await;
        let id = created.body["id"].as_str().unwrap().to_owned();
        let uri = format!("{}/{id}", segments_uri(&ws));
        let renamed = call(
            &router,
            "PATCH",
            &uri,
            Some(json!({"name": "Pro plan", "cadence_minutes": 30})),
            Auth::Session(&member),
        )
        .await;
        let second = call(
            &router,
            "PATCH",
            &uri,
            Some(json!({"definition": plan_is("free")})),
            Auth::Session(&member),
        )
        .await;
        let unknown = call(
            &router,
            "PATCH",
            &uri,
            Some(json!({"definition": {"v": 1, "trait": "nope", "op": "eq", "value": "x"}})),
            Auth::Session(&member),
        )
        .await;
        let bad_cadence = call(
            &router,
            "PATCH",
            &uri,
            Some(json!({"cadence_minutes": 7})),
            Auth::Session(&member),
        )
        .await;
        let empty = call(
            &router,
            "PATCH",
            &uri,
            Some(json!({})),
            Auth::Session(&member),
        )
        .await;
        let duplicate = make_segment(
            &router,
            Auth::Session(&member),
            &ws,
            "pro PLAN",
            plan_is("x"),
        )
        .await;
        let versions = call(
            &router,
            "GET",
            &format!("{uri}/versions"),
            None,
            Auth::Session(&member),
        )
        .await;
        let first_version = call(
            &router,
            "GET",
            &format!("{uri}/versions/1"),
            None,
            Auth::Session(&member),
        )
        .await;
        let missing_version = call(
            &router,
            "GET",
            &format!("{uri}/versions/9"),
            None,
            Auth::Session(&member),
        )
        .await;
        let one = call(&router, "GET", &uri, None, Auth::Session(&member)).await;

        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        assert_eq!(created.body["current_version"], 1);
        assert_eq!(created.body["cadence_minutes"], 60);
        assert_eq!(created.body["created_by"], member_id.to_string().as_str());
        assert!(created.body["current"].get("trace_id").is_none());
        assert_eq!(renamed.body["name"], "Pro plan");
        assert_eq!(renamed.body["current_version"], 1);
        assert_eq!(second.body["current_version"], 2);
        assert_eq!(second.body["current"]["definition"], plan_is("free"));
        assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(unknown.body["code"], "segment_invalid");
        assert_eq!(unknown.body["issues"][0]["code"], "unknown_trait");
        assert!(
            unknown.body["issues"][0]["pointer"]
                .as_str()
                .unwrap()
                .starts_with('/')
        );
        assert_eq!(bad_cadence.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(duplicate.status, StatusCode::CONFLICT);
        let numbers: Vec<i64> = versions
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|version| version["version"].as_i64().unwrap())
            .collect();
        assert_eq!(numbers, [2, 1]);
        assert_eq!(first_version.body["definition"], plan_is("pro"));
        assert_eq!(missing_version.status, StatusCode::NOT_FOUND);
        assert_eq!(one.body["current_version"], 2);
        assert_eq!(one.body["current"]["deps"]["traits"][0], "plan");
        assert_eq!(one.body["current"]["time_sensitive"], false);

        let audited: Vec<(String, Value)> = sqlx::query_as(
            "SELECT action, summary FROM audit_log WHERE action LIKE 'segment.%' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let actions: Vec<&str> = audited.iter().map(|(action, _)| action.as_str()).collect();
        assert_eq!(
            actions,
            ["segment.created", "segment.updated", "segment.updated"]
        );
        assert_eq!(audited[2].1["version"], 2);
        assert!(
            !audited
                .iter()
                .any(|(_, summary)| summary.to_string().contains("free"))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cycle_is_a_422_that_names_it_and_an_archive_is_refused_while_referenced(
        pool: PgPool,
    ) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        define_plan(&pool, &ws).await;
        let who = || Auth::Session(&owner);
        let a = make_segment(&router, who(), &ws, "A", plan_is("a"))
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let b = make_segment(&router, who(), &ws, "B", refers_to(&a))
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let a_uri = format!("{}/{a}", segments_uri(&ws));

        let cycle = call(
            &router,
            "PATCH",
            &a_uri,
            Some(json!({"definition": refers_to(&b)})),
            who(),
        )
        .await;
        let own = call(
            &router,
            "PATCH",
            &a_uri,
            Some(json!({"definition": refers_to(&a)})),
            who(),
        )
        .await;
        let referenced = call(&router, "POST", &format!("{a_uri}/archive"), None, who()).await;
        let b_uri = format!("{}/{b}", segments_uri(&ws));
        call(
            &router,
            "PATCH",
            &b_uri,
            Some(json!({"definition": plan_is("b")})),
            who(),
        )
        .await;
        let archived = call(&router, "POST", &format!("{a_uri}/archive"), None, who()).await;
        let to_archived = make_segment(&router, who(), &ws, "C", refers_to(&a)).await;
        let edit_archived = call(&router, "PATCH", &a_uri, Some(json!({"name": "Z"})), who()).await;
        let live = call(&router, "GET", &segments_uri(&ws), None, who()).await;
        let all = call(
            &router,
            "GET",
            &format!("{}?archived=true", segments_uri(&ws)),
            None,
            who(),
        )
        .await;

        assert_eq!(
            cycle.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{:?}",
            cycle.body
        );
        assert_eq!(cycle.body["code"], "segment_cycle");
        assert_eq!(
            cycle.body["detail"],
            "segments cannot reference each other: A \u{2192} B \u{2192} A"
        );
        let path: Vec<&str> = cycle.body["cycle"]
            .as_array()
            .unwrap()
            .iter()
            .map(|segment| segment["name"].as_str().unwrap())
            .collect();
        assert_eq!(path, ["A", "B", "A"]);
        assert_eq!(cycle.body["cycle"][0]["id"], a.as_str());
        assert_eq!(own.body["code"], "segment_cycle");
        assert_eq!(own.body["cycle"].as_array().unwrap().len(), 2);
        assert_eq!(referenced.status, StatusCode::CONFLICT);
        assert_eq!(referenced.body["code"], "segment_referenced");
        assert_eq!(archived.status, StatusCode::OK, "{:?}", archived.body);
        assert!(archived.body["archived_at"].is_string());
        assert_eq!(to_archived.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(to_archived.body["issues"][0]["code"], "unknown_segment");
        assert_eq!(edit_archived.status, StatusCode::CONFLICT);
        assert_eq!(edit_archived.body["code"], "segment_archived");
        assert_eq!(live.body.as_array().unwrap().len(), 1);
        assert_eq!(all.body.as_array().unwrap().len(), 2);
        let archives: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'segment.archived'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(archives, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn segments_are_isolated_per_workspace_and_key_scopes_decide_who_saves(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let one = workspace(&router, &owner, "One").await;
        let two = workspace(&router, &owner, "Two").await;
        define_plan(&pool, &one).await;
        define_plan(&pool, &two).await;
        let (_, read_key) = make_key(&router, &owner, &one, &["read"]).await;
        let (_, write_key) = make_key(&router, &owner, &one, &["write"]).await;
        let mine = make_segment(&router, Auth::Session(&owner), &one, "Mine", plan_is("x"))
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let by_read_key =
            make_segment(&router, Auth::Key(&read_key), &one, "Nope", plan_is("x")).await;
        let by_write_key =
            make_segment(&router, Auth::Key(&write_key), &one, "Keyed", plan_is("x")).await;
        let write_key_reads = call(
            &router,
            "GET",
            &segments_uri(&one),
            None,
            Auth::Key(&write_key),
        )
        .await;
        let read_key_reads = call(
            &router,
            "GET",
            &format!("{}/{mine}", segments_uri(&one)),
            None,
            Auth::Key(&read_key),
        )
        .await;
        let read_key_archives = call(
            &router,
            "POST",
            &format!("{}/{mine}/archive", segments_uri(&one)),
            None,
            Auth::Key(&read_key),
        )
        .await;
        let foreign_get = call(
            &router,
            "GET",
            &format!("{}/{mine}", segments_uri(&two)),
            None,
            Auth::Session(&owner),
        )
        .await;
        let foreign_patch = call(
            &router,
            "PATCH",
            &format!("{}/{mine}", segments_uri(&two)),
            Some(json!({"name": "Taken"})),
            Auth::Session(&owner),
        )
        .await;
        let foreign_versions = call(
            &router,
            "GET",
            &format!("{}/{mine}/versions", segments_uri(&two)),
            None,
            Auth::Session(&owner),
        )
        .await;
        let foreign_reference = make_segment(
            &router,
            Auth::Session(&owner),
            &two,
            "Ref",
            refers_to(&mine),
        )
        .await;
        let key_other_workspace = call(
            &router,
            "GET",
            &segments_uri(&two),
            None,
            Auth::Key(&read_key),
        )
        .await;

        assert_eq!(by_read_key.status, StatusCode::FORBIDDEN);
        assert_eq!(
            by_write_key.status,
            StatusCode::CREATED,
            "{:?}",
            by_write_key.body
        );
        assert!(by_write_key.body["created_by"].is_null());
        assert_eq!(write_key_reads.status, StatusCode::FORBIDDEN);
        assert_eq!(read_key_reads.status, StatusCode::OK);
        assert_eq!(read_key_archives.status, StatusCode::FORBIDDEN);
        assert_eq!(foreign_get.status, StatusCode::NOT_FOUND);
        assert_eq!(foreign_patch.status, StatusCode::NOT_FOUND);
        assert_eq!(foreign_versions.status, StatusCode::NOT_FOUND);
        assert_eq!(foreign_reference.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            foreign_reference.body["issues"][0]["code"],
            "unknown_segment"
        );
        assert_eq!(key_other_workspace.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_counts_samples_masks_by_role_and_reports_the_data_age(pool: PgPool) {
        let router = app(&pool).await;
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        define_plan(&pool, &ws).await;
        plan_people(
            &pool,
            &ws,
            &[
                ("a@example.com", "pro"),
                ("b@example.com", "pro"),
                ("c@example.com", "free"),
            ],
        )
        .await;
        let last_success = time::macros::datetime!(2026-03-01 12:00 UTC);
        let source = uuid::Uuid::now_v7();
        sqlx::query("INSERT INTO sources (id, workspace_id, kind, name, config) VALUES ($1, $2, 'csv', 'CRM', '{}')")
            .bind(source)
            .bind(ws.parse::<uuid::Uuid>().unwrap())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO streams (id, workspace_id, source_id, name, enabled, sync_mode, mapping, \
                discovered, enabled_at, last_success_at) \
             VALUES ($1, $2, $3, 'contacts', true, 'full_refresh', $4, '{}', now(), $5)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(ws.parse::<uuid::Uuid>().unwrap())
        .bind(source)
        .bind(json!({"columns": [{"column": "p", "target": {"type": "trait", "key": "plan"}}]}))
        .bind(last_success)
        .execute(&pool)
        .await
        .unwrap();
        let (member_id, member) = user(&pool, &router, "member@example.com").await;
        add_member(&router, &owner, &ws, member_id, "member").await;
        let (_, read_key) = make_key(&router, &owner, &ws, &["read"]).await;
        let uri = format!("{}/preview", segments_uri(&ws));
        let body = json!({"definition": plan_is("pro"), "sample_size": 1});

        let as_owner = call(
            &router,
            "POST",
            &uri,
            Some(body.clone()),
            Auth::Session(&owner),
        )
        .await;
        let as_member = call(
            &router,
            "POST",
            &uri,
            Some(body.clone()),
            Auth::Session(&member),
        )
        .await;
        let as_read_key = call(&router, "POST", &uri, Some(body), Auth::Key(&read_key)).await;
        let default_size = call(
            &router,
            "POST",
            &uri,
            Some(json!({"definition": plan_is("pro")})),
            Auth::Session(&owner),
        )
        .await;
        let too_big = call(
            &router,
            "POST",
            &uri,
            Some(json!({"definition": plan_is("pro"), "sample_size": 101})),
            Auth::Session(&owner),
        )
        .await;
        let invalid = call(
            &router,
            "POST",
            &uri,
            Some(json!({"definition": {"v": 1, "trait": "nope", "op": "eq", "value": "x"}})),
            Auth::Session(&owner),
        )
        .await;
        let live = call(
            &router,
            "POST",
            &uri,
            Some(json!({"definition": {"v": 1, "identifier": "email"}})),
            Auth::Session(&owner),
        )
        .await;
        let saved: i64 = sqlx::query_scalar("SELECT count(*) FROM segments")
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(as_owner.status, StatusCode::OK, "{:?}", as_owner.body);
        assert_eq!(as_owner.body["count"], 2);
        assert_eq!(as_owner.body["sample"].as_array().unwrap().len(), 1);
        assert_eq!(as_owner.body["sample"][0]["display"]["masked"], false);
        assert!(
            as_owner.body["sample"][0]["display"]["value"]
                .as_str()
                .unwrap()
                .ends_with("@example.com")
        );
        assert!(as_owner.body["compiled_at"].is_string());
        assert_eq!(as_owner.body["data_age"]["as_of"], "2026-03-01T12:00:00Z");
        assert_eq!(as_owner.body["data_age"]["sources"][0]["name"], "CRM");
        assert_eq!(
            as_owner.body["data_age"]["sources"][0]["source_id"],
            source.to_string().as_str()
        );
        for masked in [&as_member, &as_read_key] {
            assert_eq!(masked.status, StatusCode::OK, "{:?}", masked.body);
            assert_eq!(masked.body["count"], 2);
            assert_eq!(masked.body["sample"][0]["display"]["masked"], true);
            assert!(masked.body["sample"][0]["display"]["value"].is_null());
        }
        assert_eq!(default_size.body["sample"].as_array().unwrap().len(), 2);
        assert_eq!(too_big.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(invalid.body["code"], "segment_invalid");
        assert_eq!(live.body["count"], 3);
        assert!(live.body["data_age"]["as_of"].is_null());
        assert_eq!(live.body["data_age"]["live"], true);
        assert_eq!(as_owner.body["data_age"]["live"], false);
        assert_eq!(live.body["data_age"]["sources"], json!([]));
        assert_eq!(saved, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_past_its_time_limit_is_a_503_with_its_own_code(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let router = router(
            AppState::new(pool.clone(), Arc::new(String::new))
                .with_hmac_root(&zirv_flow_core::HmacRoot::bootstrap_from(
                    &zirv_flow_core::MasterKey::generate(),
                ))
                .with_segment_preview_timeout(std::time::Duration::from_millis(100)),
        );
        let owner = owner(&pool, &router).await;
        let ws = workspace(&router, &owner, "Alpha").await;
        define_plan(&pool, &ws).await;
        plan_people(&pool, &ws, &[("a@example.com", "pro")]).await;
        // A lock on the people table holds the count query until the statement timeout cancels it.
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE people IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .unwrap();

        let reply = call(
            &router,
            "POST",
            &format!("{}/preview", segments_uri(&ws)),
            Some(json!({"definition": plan_is("pro")})),
            Auth::Session(&owner),
        )
        .await;
        drop(blocker);

        assert_eq!(
            reply.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{:?}",
            reply.body
        );
        assert_eq!(reply.body["code"], "segment_preview_timeout");
    }
}
