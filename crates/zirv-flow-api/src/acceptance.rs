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
            sqlx::query(
                "INSERT INTO identity_conflicts \
                    (id, workspace_id, person_id, other_person_id, kind, source_record_id, reason) \
                 SELECT $1, workspace_id, person_id, other_person_id, kind, source_record_id, \
                        reason FROM identity_conflicts WHERE id = $2",
            )
            .bind(uuid::Uuid::now_v7())
            .bind(uuid::Uuid::parse_str(&id).unwrap())
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
}
