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
        router(AppState::new(pool.clone(), Arc::new(String::new)))
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
        match auth {
            Auth::Anonymous => {}
            Auth::Session(login) => {
                request = request
                    .header(header::COOKIE, &login.cookie)
                    .header("x-csrf-token", &login.csrf);
            }
            Auth::SessionWithoutCsrf(login) => {
                request = request.header(header::COOKIE, &login.cookie);
            }
            Auth::SessionWithCsrf(login, csrf) => {
                request = request
                    .header(header::COOKIE, &login.cookie)
                    .header("x-csrf-token", csrf);
            }
            Auth::Key(key) => {
                request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
            }
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
}
