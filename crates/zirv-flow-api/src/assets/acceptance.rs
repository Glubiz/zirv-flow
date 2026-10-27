//! The assets API end to end, driven through the router: uploads and their refusals, the limits,
//! the reference rule on delete, the role matrix and the public route.

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;
    use zirv_flow_core::{AssetId, Scope, WorkspaceId};
    use zirv_flow_store::{AssetOwnerKind, record_references};

    use crate::admin::{self, Context};
    use crate::assets::AssetLimits;
    use crate::assets::sniff::fixtures::{
        gif, jpeg, png, webp_extended, webp_lossless, webp_lossy,
    };
    use crate::{AppState, router};

    struct Reply {
        status: StatusCode,
        headers: axum::http::HeaderMap,
        bytes: Vec<u8>,
    }

    impl Reply {
        fn json(&self) -> Value {
            serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
        }

        fn code(&self) -> Option<String> {
            self.json()["code"].as_str().map(str::to_owned)
        }
    }

    struct Fixture {
        pool: PgPool,
        router: Router,
        ws: WorkspaceId,
        other_ws: WorkspaceId,
        read: String,
        write: String,
        admin: String,
        other_admin: String,
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = zirv_flow_core::WorkspaceName::parse(slug).unwrap();
        let slug = zirv_flow_core::Slug::parse(slug).unwrap();
        zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id
    }

    async fn fixture_with(pool: &PgPool, limits: AssetLimits, public_url: Option<&str>) -> Fixture {
        zirv_flow_store::migrate(pool).await.unwrap();
        let ws = workspace(pool, "ours").await;
        let other_ws = workspace(pool, "theirs").await;
        let state = AppState::new(pool.clone(), Arc::new(String::new))
            .with_asset_limits(limits)
            .with_public_url(public_url.map(str::to_owned));
        let router = router(state);
        let key = |workspace: WorkspaceId, scope: Scope| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), workspace, "assets", &[scope])
                    .await
                    .unwrap()
                    .plaintext
            }
        };
        Fixture {
            pool: pool.clone(),
            router,
            ws,
            other_ws,
            read: key(ws, Scope::Read).await,
            write: key(ws, Scope::Write).await,
            admin: key(ws, Scope::Admin).await,
            other_admin: key(other_ws, Scope::Admin).await,
        }
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        fixture_with(pool, AssetLimits::default(), None).await
    }

    async fn reply(router: &Router, request: Request<Body>) -> Reply {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        Reply {
            status,
            headers,
            bytes,
        }
    }

    impl Fixture {
        fn assets(&self, ws: WorkspaceId, tail: &str) -> String {
            format!("/api/v1/workspaces/{}/assets{tail}", ws.as_uuid())
        }

        async fn post(
            &self,
            key: Option<&str>,
            query: &str,
            content_type: &str,
            body: Body,
        ) -> Reply {
            let mut request = Request::builder()
                .method("POST")
                .uri(format!("{}{query}", self.assets(self.ws, "")))
                .header(header::CONTENT_TYPE, content_type);
            if let Some(key) = key {
                request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
            }
            reply(&self.router, request.body(body).unwrap()).await
        }

        async fn upload(&self, name: &str, content_type: &str, bytes: &[u8]) -> Reply {
            self.post(
                Some(&self.write),
                &format!("?name={name}"),
                content_type,
                Body::from(bytes.to_vec()),
            )
            .await
        }

        async fn upload_png(&self, name: &str, width: u32, height: u32) -> Value {
            let reply = self.upload(name, "image/png", &png(width, height)).await;
            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
            reply.json()
        }

        async fn call(&self, method: &str, uri: &str, key: Option<&str>) -> Reply {
            let mut request = Request::builder().method(method).uri(uri);
            if let Some(key) = key {
                request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
            }
            reply(&self.router, request.body(Body::empty()).unwrap()).await
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }
    }

    fn id_of(body: &Value) -> Uuid {
        body["id"].as_str().unwrap().parse().unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn png_jpeg_gif_and_webp_upload_and_report_their_dimensions(pool: PgPool) {
        let f = fixture(&pool).await;
        let cases: [(&str, Vec<u8>, &str, i64, i64); 6] = [
            ("image/png", png(640, 480), "png", 640, 480),
            ("image/jpeg", jpeg(300, 200), "jpg", 300, 200),
            ("image/gif", gif(10, 20), "gif", 10, 20),
            ("image/webp", webp_lossy(33, 44), "webp", 33, 44),
            ("image/webp", webp_lossless(500, 100), "webp", 500, 100),
            ("image/webp", webp_extended(1000, 2000), "webp", 1000, 2000),
        ];
        for (content_type, bytes, ext, width, height) in cases {
            let reply = f.upload("pic", content_type, &bytes).await;

            assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
            let body = reply.json();
            assert_eq!(body["width"], width);
            assert_eq!(body["height"], height);
            assert_eq!(body["content_type"], content_type);
            assert_eq!(body["size_bytes"], bytes.len());
            assert_eq!(body["references"], 0);
            let url = body["url"].as_str().unwrap();
            assert!(url.starts_with("/a/"), "{url}");
            assert!(url.ends_with(&format!(".{ext}")), "{url}");
            assert_eq!(url.len(), 3 + 64 + 1 + ext.len());
        }
        assert_eq!(
            f.count("SELECT count(*) FROM audit_log WHERE action = 'asset.uploaded'")
                .await,
            6
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn svg_and_other_types_are_refused_before_the_body_is_read(pool: PgPool) {
        let f = fixture(&pool).await;
        let read = Arc::new(AtomicUsize::new(0));
        let counter = read.clone();
        let stream = futures_util::stream::iter((0..10).map(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok::<_, std::convert::Infallible>(Bytes::from_static(b"<svg/>"))
        }));

        let svg = f
            .post(
                Some(&f.write),
                "?name=logo",
                "image/svg+xml",
                Body::from_stream(stream),
            )
            .await;
        let html = f.upload("page", "text/html", b"<p>").await;

        assert_eq!(svg.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(svg.code().as_deref(), Some("asset_type_unsupported"));
        assert_eq!(read.load(Ordering::SeqCst), 0, "the body was read");
        assert_eq!(html.code().as_deref(), Some("asset_type_unsupported"));
        assert_eq!(f.count("SELECT count(*) FROM assets").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_mismatched_truncated_or_non_image_body_is_refused(pool: PgPool) {
        let f = fixture(&pool).await;
        let truncated = png(8, 8)[..14].to_vec();
        let cases: [(&str, Vec<u8>); 5] = [
            ("image/jpeg", png(8, 8)),
            ("image/png", gif(8, 8)),
            ("image/png", truncated),
            (
                "image/png",
                b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec(),
            ),
            ("image/webp", Vec::new()),
        ];
        for (content_type, bytes) in cases {
            let reply = f.upload("pic", content_type, &bytes).await;

            assert_eq!(
                reply.status,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{content_type}"
            );
            assert_eq!(reply.code().as_deref(), Some("asset_type_mismatch"));
        }
        assert_eq!(f.count("SELECT count(*) FROM assets").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_image_over_the_file_limit_is_refused_by_length_and_while_streaming(pool: PgPool) {
        let limits = AssetLimits {
            max_file_bytes: 1000,
            ..AssetLimits::default()
        };
        let f = fixture_with(&pool, limits, None).await;

        let declared = f.upload("big", "image/png", &vec![0_u8; 2000]).await;
        let pulled = Arc::new(AtomicUsize::new(0));
        let counter = pulled.clone();
        let stream = futures_util::stream::iter((0..1000).map(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok::<_, std::convert::Infallible>(Bytes::from(vec![0_u8; 400]))
        }));
        let streamed = f
            .post(
                Some(&f.write),
                "?name=big",
                "image/png",
                Body::from_stream(stream),
            )
            .await;

        assert_eq!(declared.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(declared.code().as_deref(), Some("asset_too_large"));
        assert_eq!(streamed.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(streamed.code().as_deref(), Some("asset_too_large"));
        let chunks = pulled.load(Ordering::SeqCst);
        assert!(chunks <= 4, "{chunks} chunks were pulled");
        assert_eq!(f.count("SELECT count(*) FROM assets").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn dimensions_over_the_limits_are_refused_from_the_header_alone(pool: PgPool) {
        let f = fixture(&pool).await;
        for (width, height) in [(8193, 1), (1, 8193), (8000, 5125)] {
            let reply = f.upload("huge", "image/png", &png(width, height)).await;

            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{width}x{height}"
            );
            assert_eq!(reply.code().as_deref(), Some("asset_dimensions_exceeded"));
        }
        let edge = f.upload("edge", "image/png", &png(8192, 4882)).await;
        assert_eq!(edge.status, StatusCode::CREATED, "{:?}", edge.json());
        assert_eq!(f.count("SELECT count(*) FROM assets").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_bad_name_is_refused(pool: PgPool) {
        let f = fixture(&pool).await;
        let bytes = png(2, 2);
        for query in ["", "?name=", "?name=a%2Fb", "?name=a%00b"] {
            let reply = f
                .post(
                    Some(&f.write),
                    query,
                    "image/png",
                    Body::from(bytes.clone()),
                )
                .await;

            assert!(reply.status.is_client_error(), "{query}: {}", reply.status);
        }
        assert_eq!(f.count("SELECT count(*) FROM assets").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_parallel_uploads_at_the_workspace_quota_edge_admit_only_one(pool: PgPool) {
        let size = png(1, 1).len() as u64;
        let limits = AssetLimits {
            max_workspace_bytes: size + size / 2,
            ..AssetLimits::default()
        };
        let f = fixture_with(&pool, limits, None).await;

        let (first, second) = (png(1, 1), png(1, 2));
        let (one, two) = tokio::join!(
            f.upload("one", "image/png", &first),
            f.upload("two", "image/png", &second),
        );

        let mut statuses = [one.status, two.status];
        statuses.sort();
        assert_eq!(
            statuses,
            [StatusCode::CREATED, StatusCode::PAYLOAD_TOO_LARGE]
        );
        let refused = if one.status == StatusCode::CREATED {
            &two
        } else {
            &one
        };
        assert_eq!(refused.code().as_deref(), Some("asset_quota_exceeded"));
        assert_eq!(f.count("SELECT count(*) FROM assets").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_same_bytes_again_return_the_existing_asset_with_200(pool: PgPool) {
        let f = fixture(&pool).await;
        let first = f.upload_png("first", 5, 5).await;

        let again = f.upload("second", "image/png", &png(5, 5)).await;

        assert_eq!(again.status, StatusCode::OK);
        assert_eq!(again.json()["id"], first["id"]);
        assert_eq!(again.json()["name"], "first");
        assert_eq!(f.count("SELECT count(*) FROM assets").await, 1);
        assert_eq!(
            f.count("SELECT count(*) FROM audit_log WHERE action = 'asset.uploaded'")
                .await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_public_route_serves_the_bytes_with_the_ruled_headers(pool: PgPool) {
        let f = fixture(&pool).await;
        let bytes = png(7, 9);
        let uploaded = f.upload_png("pic", 7, 9).await;
        let url = uploaded["url"].as_str().unwrap().to_owned();

        let served = f.call("GET", &url, None).await;

        assert_eq!(served.status, StatusCode::OK);
        assert_eq!(served.bytes, bytes);
        let header = |name: &str| {
            served
                .headers
                .get(name)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(header("content-type"), "image/png");
        assert_eq!(
            header("cache-control"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(header("x-content-type-options"), "nosniff");
        assert_eq!(
            header("content-security-policy"),
            "default-src 'none'; sandbox"
        );
        assert_eq!(header("content-disposition"), "inline");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_workspace_row_with_the_same_hash_serves_after_the_first_is_deleted(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let ours = f.upload_png("pic", 3, 3).await;
        let theirs = f
            .post(
                Some(&f.other_admin),
                "?name=pic",
                "image/png",
                Body::from(png(3, 3)),
            )
            .await;
        // The other workspace's key lacks write scope here; give it a write key instead.
        assert_eq!(theirs.status, StatusCode::NOT_FOUND);
        let key =
            admin::create_api_key(&pool, &Context::system(), f.other_ws, "w", &[Scope::Write])
                .await
                .unwrap()
                .plaintext;
        let request = Request::builder()
            .method("POST")
            .uri(format!("{}?name=pic", f.assets(f.other_ws, "")))
            .header(header::CONTENT_TYPE, "image/png")
            .header(header::AUTHORIZATION, format!("Bearer {key}"))
            .body(Body::from(png(3, 3)))
            .unwrap();
        let theirs = reply(&f.router, request).await;
        assert_eq!(theirs.status, StatusCode::CREATED);
        let url = ours["url"].as_str().unwrap().to_owned();
        assert_eq!(theirs.json()["url"], ours["url"]);

        let deleted = f
            .call(
                "DELETE",
                &f.assets(f.ws, &format!("/{}", id_of(&ours))),
                Some(&f.admin),
            )
            .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        assert_eq!(f.call("GET", &url, None).await.status, StatusCode::OK);

        let gone = f
            .call(
                "DELETE",
                &f.assets(f.other_ws, &format!("/{}", id_of(&theirs.json()))),
                Some(&f.other_admin),
            )
            .await;
        assert_eq!(gone.status, StatusCode::NO_CONTENT);
        assert_eq!(
            f.call("GET", &url, None).await.status,
            StatusCode::NOT_FOUND
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn anything_else_under_a_is_a_plain_404_and_never_the_web_app(pool: PgPool) {
        let f = fixture(&pool).await;
        let uploaded = f.upload_png("pic", 4, 4).await;
        let url = uploaded["url"].as_str().unwrap().to_owned();
        let hash = url
            .trim_start_matches("/a/")
            .trim_end_matches(".png")
            .to_owned();

        for path in [
            "/a/unknown".to_owned(),
            "/a/".to_owned(),
            "/a/x/y".to_owned(),
            format!("/a/{hash}.jpg"),
            format!("/a/{hash}.svg"),
            format!("/a/{hash}"),
            format!("/a/{}.png", "0".repeat(64)),
            format!("/a/{}.png", hash.to_uppercase()),
        ] {
            let reply = f.call("GET", &path, None).await;

            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}");
            let content_type = reply
                .headers
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            assert!(
                !content_type.starts_with("text/html"),
                "{path}: {content_type}"
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_list_has_newest_first_pages_the_quota_and_the_public_url(pool: PgPool) {
        let limits = AssetLimits {
            max_workspace_bytes: 10_000,
            ..AssetLimits::default()
        };
        let f = fixture_with(&pool, limits, Some("https://flow.example.com/")).await;
        let a = f.upload_png("a", 1, 1).await;
        let b = f.upload_png("b", 1, 2).await;
        let c = f.upload_png("c", 1, 3).await;
        let other = f
            .post(
                Some(&f.other_admin),
                "?name=x",
                "image/png",
                Body::from(png(9, 9)),
            )
            .await;
        assert_eq!(other.status, StatusCode::NOT_FOUND);
        let used = png(1, 1).len() * 3;

        let list = f.call("GET", &f.assets(f.ws, ""), Some(&f.read)).await;

        assert_eq!(list.status, StatusCode::OK);
        let body = list.json();
        let names: Vec<&str> = body["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|asset| asset["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["c", "b", "a"]);
        assert_eq!(body["used_bytes"], used);
        assert_eq!(body["limit_bytes"], 10_000);
        assert_eq!(body["max_file_bytes"], 5 * 1024 * 1024);
        assert_eq!(body["next_before"], Value::Null);
        let url = body["assets"][0]["url"].as_str().unwrap();
        assert!(url.starts_with("https://flow.example.com/a/"), "{url}");

        let first = f
            .call("GET", &f.assets(f.ws, "?limit=2"), Some(&f.read))
            .await
            .json();
        assert_eq!(first["assets"].as_array().unwrap().len(), 2);
        assert_eq!(first["next_before"], b["id"]);
        let rest = f
            .call(
                "GET",
                &f.assets(
                    f.ws,
                    &format!("?limit=2&before={}", first["next_before"].as_str().unwrap()),
                ),
                Some(&f.read),
            )
            .await
            .json();
        assert_eq!(rest["assets"][0]["id"], a["id"]);
        assert_eq!(rest["assets"].as_array().unwrap().len(), 1);
        assert_eq!(c["name"], "c");
        let bad = f
            .call("GET", &f.assets(f.ws, "?limit=0"), Some(&f.read))
            .await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn one_asset_shows_the_versions_that_use_it(pool: PgPool) {
        let f = fixture(&pool).await;
        let asset = f.upload_png("pic", 2, 2).await;
        let owner = Uuid::now_v7();
        let mut connection = pool.acquire().await.unwrap();
        record_references(
            &mut connection,
            f.ws,
            AssetOwnerKind::Template,
            owner,
            4,
            &[AssetId::from_uuid(id_of(&asset))],
        )
        .await
        .unwrap();

        let got = f
            .call(
                "GET",
                &f.assets(f.ws, &format!("/{}", id_of(&asset))),
                Some(&f.read),
            )
            .await;

        assert_eq!(got.status, StatusCode::OK);
        let body = got.json();
        assert_eq!(body["references"], 1);
        assert_eq!(body["used_by"][0]["kind"], "template");
        assert_eq!(body["used_by"][0]["id"], owner.to_string());
        assert_eq!(body["used_by"][0]["version"], 4);
        assert_eq!(body["used_by"][0]["name"], Value::Null);
        assert_eq!(body["used_by"][0]["archived"], false);
        let list = f
            .call("GET", &f.assets(f.ws, ""), Some(&f.read))
            .await
            .json();
        assert_eq!(list["assets"][0]["references"], 1);
        let foreign = f
            .call(
                "GET",
                &f.assets(f.other_ws, &format!("/{}", id_of(&asset))),
                Some(&f.other_admin),
            )
            .await;
        assert_eq!(foreign.status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn used_by_names_a_template_theme_and_snippet_owner_and_flags_an_archived_one(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let asset = f.upload_png("pic", 2, 2).await;
        let (template, theme, snippet) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let ws = f.ws.as_uuid();
        sqlx::query(
            "INSERT INTO templates (id, workspace_id, name, channel, category, archived_at) \
             VALUES ($1, $2, 'Order shipped', 'email', 'transactional', now())",
        )
        .bind(template)
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO themes (id, workspace_id, name) VALUES ($1, $2, 'Plain')")
            .bind(theme)
            .bind(ws)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO snippets (id, workspace_id, name) VALUES ($1, $2, 'Footer')")
            .bind(snippet)
            .bind(ws)
            .execute(&pool)
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        for (kind, owner) in [
            (AssetOwnerKind::Template, template),
            (AssetOwnerKind::Theme, theme),
            (AssetOwnerKind::Snippet, snippet),
        ] {
            record_references(
                &mut connection,
                f.ws,
                kind,
                owner,
                1,
                &[AssetId::from_uuid(id_of(&asset))],
            )
            .await
            .unwrap();
        }

        let body = f
            .call(
                "GET",
                &f.assets(f.ws, &format!("/{}", id_of(&asset))),
                Some(&f.read),
            )
            .await
            .json();

        let used: Vec<(&str, &str, bool)> = body["used_by"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| {
                (
                    o["kind"].as_str().unwrap(),
                    o["name"].as_str().unwrap(),
                    o["archived"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            used,
            [
                ("snippet", "Footer", false),
                ("template", "Order shipped", true),
                ("theme", "Plain", false),
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn delete_is_refused_while_a_reference_exists_and_allowed_without(pool: PgPool) {
        let f = fixture(&pool).await;
        let used = f.upload_png("used", 2, 2).await;
        let free = f.upload_png("free", 2, 3).await;
        let owner = Uuid::now_v7();
        let mut connection = pool.acquire().await.unwrap();
        record_references(
            &mut connection,
            f.ws,
            AssetOwnerKind::Snippet,
            owner,
            1,
            &[AssetId::from_uuid(id_of(&used))],
        )
        .await
        .unwrap();
        let uri = |asset: &Value| f.assets(f.ws, &format!("/{}", id_of(asset)));

        let refused = f.call("DELETE", &uri(&used), Some(&f.admin)).await;
        let allowed = f.call("DELETE", &uri(&free), Some(&f.admin)).await;

        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert_eq!(refused.code().as_deref(), Some("asset_in_use"));
        assert!(
            refused.json()["detail"]
                .as_str()
                .unwrap()
                .contains(&owner.to_string())
        );
        assert_eq!(allowed.status, StatusCode::NO_CONTENT);
        assert_eq!(
            f.call("GET", &uri(&free), Some(&f.read)).await.status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            f.call("GET", &uri(&used), Some(&f.read)).await.status,
            StatusCode::OK
        );
        assert_eq!(
            f.count("SELECT count(*) FROM audit_log WHERE action = 'asset.deleted'")
                .await,
            1
        );
        assert_eq!(
            f.count("SELECT count(*) FROM assets WHERE deleted_at IS NOT NULL AND octet_length(bytes) = 0")
                .await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_role_matrix_reader_cannot_upload_and_writer_cannot_delete(pool: PgPool) {
        let f = fixture(&pool).await;
        let asset = f.upload_png("pic", 2, 2).await;
        let item = f.assets(f.ws, &format!("/{}", id_of(&asset)));
        let collection = f.assets(f.ws, "");

        let reader_upload = f
            .post(Some(&f.read), "?name=x", "image/png", Body::from(png(3, 3)))
            .await;
        let anonymous_upload = f
            .post(None, "?name=x", "image/png", Body::from(png(3, 3)))
            .await;
        let writer_delete = f.call("DELETE", &item, Some(&f.write)).await;
        let reader_delete = f.call("DELETE", &item, Some(&f.read)).await;
        let writer_list = f.call("GET", &collection, Some(&f.write)).await;
        let anonymous_list = f.call("GET", &collection, None).await;
        let stranger_list = f.call("GET", &collection, Some(&f.other_admin)).await;
        let reader_list = f.call("GET", &collection, Some(&f.read)).await;
        let reader_get = f.call("GET", &item, Some(&f.read)).await;
        let admin_delete = f.call("DELETE", &item, Some(&f.admin)).await;

        assert_eq!(reader_upload.status, StatusCode::FORBIDDEN);
        assert_eq!(anonymous_upload.status, StatusCode::UNAUTHORIZED);
        assert_eq!(writer_delete.status, StatusCode::FORBIDDEN);
        assert_eq!(reader_delete.status, StatusCode::FORBIDDEN);
        assert_eq!(writer_list.status, StatusCode::FORBIDDEN);
        assert_eq!(anonymous_list.status, StatusCode::UNAUTHORIZED);
        assert_eq!(stranger_list.status, StatusCode::NOT_FOUND);
        assert_eq!(reader_list.status, StatusCode::OK);
        assert_eq!(reader_get.status, StatusCode::OK);
        assert_eq!(admin_delete.status, StatusCode::NO_CONTENT);
        assert_eq!(
            f.count("SELECT count(*) FROM assets WHERE deleted_at IS NULL")
                .await,
            0
        );
    }
}
