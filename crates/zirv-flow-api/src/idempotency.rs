//! `Idempotency-Key` on the creating requests that no unique name or draft revision guards
//! (architecture section 15). A request with the header runs inside the key's advisory lock, so
//! a retry that overlaps the original waits for it. The same key with the same request replays
//! the first answer; the same key with another request is `422 idempotency_key_reused`. A request
//! without the header behaves as it always did.
//!
//! A route either stores its response body, or stores none and answers a replay with
//! `409 idempotency_key_replayed` and the id of what the first request created. A route stores
//! no body when its response holds a secret or a person's value.

use axum::Json;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::token::{Sha256Stream, sha256_bytes};
use zirv_flow_store::{NewRememberedRequest, Tx};

use crate::problem::ApiError;
use crate::{request_id, trace};

const MAX_KEY_CHARS: usize = 255;
const REPLAYED: HeaderName = HeaderName::from_static("idempotent-replayed");

/// The key of a request, if it sent one: 1 to 255 characters without control characters.
pub(crate) fn key_header(headers: &HeaderMap) -> Result<Option<&str>, ApiError> {
    let Some(value) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    value
        .to_str()
        .ok()
        .filter(|key| {
            (1..=MAX_KEY_CHARS).contains(&key.chars().count()) && !key.chars().any(char::is_control)
        })
        .map(Some)
        .ok_or_else(|| {
            ApiError::unprocessable(
                "Idempotency-Key: must be 1 to 255 characters without control characters",
            )
        })
}

/// The `409` of a replay on a route that stores no body.
#[derive(Serialize, ToSchema)]
pub struct ReplayedProblem {
    /// Problem type URI; `about:blank`.
    #[serde(rename = "type")]
    #[schema(rename = "type", example = "about:blank")]
    pub kind: String,
    pub title: String,
    pub status: u16,
    pub detail: String,
    /// The `x-request-id` of the request.
    pub request_id: Option<String>,
    /// The `x-trace-id` of the request.
    pub trace_id: Option<String>,
    /// Always `idempotency_key_replayed`.
    pub code: String,
    /// The id of the resource the first request created.
    pub resource_id: Uuid,
}

/// The key a request sent, for one route.
pub(crate) struct Keyed {
    route: &'static str,
    key_hash: [u8; 32],
}

impl Keyed {
    /// The key of the request on `route`, or `None` when it sent no header.
    pub(crate) fn of(headers: &HeaderMap, route: &'static str) -> Result<Option<Self>, ApiError> {
        Ok(key_header(headers)?.map(|key| Self {
            route,
            key_hash: sha256_bytes(key.as_bytes()),
        }))
    }
}

/// The hash of a request: its method, path and query, and its body.
pub(crate) struct RequestHash(Sha256Stream);

impl RequestHash {
    pub(crate) fn new(method: &Method, uri: &Uri) -> Self {
        let mut hash = Sha256Stream::new();
        hash.update(
            format!(
                "{method}\n{}\n{}\n",
                uri.path(),
                uri.query().unwrap_or_default()
            )
            .as_bytes(),
        );
        Self(hash)
    }

    /// The hash of a request with a JSON body.
    pub(crate) fn of_json(
        method: &Method,
        uri: &Uri,
        body: &impl Serialize,
    ) -> Result<[u8; 32], ApiError> {
        let mut hash = Self::new(method, uri);
        let body = serde_json::to_vec(body).map_err(|_| ApiError::internal())?;
        hash.update(&body);
        Ok(hash.finish())
    }

    pub(crate) fn update(&mut self, piece: &[u8]) {
        self.0.update(piece);
    }

    pub(crate) fn finish(self) -> [u8; 32] {
        self.0.finish()
    }
}

/// A key that no earlier request used. Holds the key's lock until [`Guard::finish`] or the drop.
pub(crate) struct Guard {
    tx: Tx,
    workspace: WorkspaceId,
    route: &'static str,
    key_hash: [u8; 32],
}

impl Guard {
    /// Remembers the answer of the request that ran under the key. `body` is stored only for a
    /// route whose response holds no secret and no person's value.
    pub(crate) async fn finish(
        mut self,
        request_hash: [u8; 32],
        status: StatusCode,
        resource_id: Uuid,
        body: Option<&Value>,
    ) -> Result<(), ApiError> {
        zirv_flow_store::remember_api_idempotency(
            &mut self.tx,
            &NewRememberedRequest {
                workspace: self.workspace,
                route: self.route,
                key_hash: &self.key_hash,
                request_hash: &request_hash,
                status: status.as_u16(),
                resource_id,
                response: body,
            },
        )
        .await?;
        zirv_flow_store::commit(self.tx).await?;
        Ok(())
    }
}

/// Remembers the answer when the request had a key.
pub(crate) async fn finish(
    guard: Option<Guard>,
    request_hash: [u8; 32],
    status: StatusCode,
    resource_id: Uuid,
    body: Option<&Value>,
) -> Result<(), ApiError> {
    match guard {
        Some(guard) => guard.finish(request_hash, status, resource_id, body).await,
        None => Ok(()),
    }
}

/// A key that an earlier request used.
pub(crate) struct Seen(zirv_flow_store::RememberedRequest);

impl Seen {
    /// The answer to a request with `request_hash` under this key: the first answer, or
    /// `422 idempotency_key_reused` when the request is another one.
    pub(crate) fn answer(self, request_hash: [u8; 32]) -> Response {
        let first = self.0;
        if first.request_hash != request_hash {
            return ApiError::idempotency_key_reused().into_response();
        }
        let status = StatusCode::from_u16(first.status).unwrap_or(StatusCode::OK);
        let mut response = match first.response {
            Some(body) => (status, Json(body)).into_response(),
            None => replayed_without_body(first.resource_id),
        };
        response
            .headers_mut()
            .insert(REPLAYED, HeaderValue::from_static("true"));
        response
    }
}

fn replayed_without_body(resource_id: Uuid) -> Response {
    let body = json!({
        "type": "about:blank",
        "title": "Conflict",
        "status": 409,
        "detail": "this Idempotency-Key already created a resource; its response is not kept",
        "request_id": request_id::current(),
        "trace_id": trace::current().map(|trace| trace.trace_id.to_string()),
        "code": "idempotency_key_replayed",
        "resource_id": resource_id,
    });
    (
        StatusCode::CONFLICT,
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        )],
        body.to_string(),
    )
        .into_response()
}

/// What opening a key found.
pub(crate) enum Opened {
    /// No earlier request used the key (or the request sent none): run the request. The guard is
    /// present when there is a key.
    Free(Option<Guard>),
    /// An earlier request used it.
    Seen(Seen),
}

/// Takes the key's lock and looks it up. Without a key, the request runs as it always did.
pub(crate) async fn open(
    pool: &zirv_flow_store::PgPool,
    workspace: WorkspaceId,
    keyed: Option<Keyed>,
) -> Result<Opened, ApiError> {
    let Some(keyed) = keyed else {
        return Ok(Opened::Free(None));
    };
    let mut tx = zirv_flow_store::begin(pool).await?;
    let remembered =
        zirv_flow_store::lock_api_idempotency(&mut tx, workspace, keyed.route, &keyed.key_hash)
            .await?;
    Ok(match remembered {
        Some(first) => Opened::Seen(Seen(first)),
        None => Opened::Free(Some(Guard {
            tx,
            workspace,
            route: keyed.route,
            key_hash: keyed.key_hash,
        })),
    })
}

/// [`open`] for a request whose hash is known up front: a replay or reuse is the answer.
pub(crate) async fn enter(
    pool: &zirv_flow_store::PgPool,
    workspace: WorkspaceId,
    keyed: Option<Keyed>,
    request_hash: [u8; 32],
) -> Result<Result<Option<Guard>, Response>, ApiError> {
    Ok(match open(pool, workspace, keyed).await? {
        Opened::Free(guard) => Ok(guard),
        Opened::Seen(seen) => Err(seen.answer(request_hash)),
    })
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", HeaderValue::from_str(key).unwrap());
        headers
    }

    #[test]
    fn a_missing_header_is_no_key_and_a_valid_one_is_taken_as_sent() {
        assert!(key_header(&HeaderMap::new()).unwrap().is_none());
        assert_eq!(key_header(&headers("k-1")).unwrap(), Some("k-1"));
        assert!(key_header(&headers(&"k".repeat(255))).is_ok());
    }

    #[test]
    fn a_key_over_255_characters_is_refused_with_422() {
        let error = key_header(&headers(&"k".repeat(256))).unwrap_err();

        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn the_hash_covers_the_method_path_query_and_body() {
        let uri: Uri = "/a/b?x=1".parse().unwrap();
        let other: Uri = "/a/b?x=2".parse().unwrap();
        let same = RequestHash::of_json(&Method::POST, &uri, &json!({"n": 1})).unwrap();

        assert_eq!(
            same,
            RequestHash::of_json(&Method::POST, &uri, &json!({"n": 1})).unwrap()
        );
        assert_ne!(
            same,
            RequestHash::of_json(&Method::POST, &uri, &json!({"n": 2})).unwrap()
        );
        assert_ne!(
            same,
            RequestHash::of_json(&Method::POST, &other, &json!({"n": 1})).unwrap()
        );
    }

    #[tokio::test]
    async fn a_replay_without_a_body_is_409_with_the_resource_id_and_never_a_body() {
        let id = Uuid::now_v7();
        let seen = Seen(zirv_flow_store::RememberedRequest {
            request_hash: vec![1; 32],
            status: 201,
            resource_id: id,
            response: None,
        });

        let response = seen.answer([1; 32]);

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(response.headers()["idempotent-replayed"], "true");
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["code"], "idempotency_key_replayed");
        assert_eq!(body["resource_id"], id.to_string());
    }

    #[tokio::test]
    async fn a_replay_with_a_stored_body_repeats_status_and_body() {
        let seen = Seen(zirv_flow_store::RememberedRequest {
            request_hash: vec![1; 32],
            status: 201,
            resource_id: Uuid::nil(),
            response: Some(json!({"id": "x"})),
        });

        let response = seen.answer([1; 32]);

        assert_eq!(response.status(), StatusCode::CREATED);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"id": "x"})
        );
    }

    #[test]
    fn another_request_under_the_key_is_422_idempotency_key_reused() {
        let seen = Seen(zirv_flow_store::RememberedRequest {
            request_hash: vec![1; 32],
            status: 201,
            resource_id: Uuid::nil(),
            response: None,
        });

        let response = seen.answer([2; 32]);

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}

#[cfg(test)]
mod acceptance {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, header};
    use http_body_util::BodyExt;
    use sqlx::PgPool;
    use tower::ServiceExt;
    use zirv_flow_core::Scope;

    use super::*;
    use crate::admin::{self, Context};
    use crate::assets::sniff::fixtures::png;
    use crate::{AppState, router};

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        text: String,
    }

    impl Reply {
        fn json(&self) -> Value {
            serde_json::from_str(&self.text).unwrap_or(Value::Null)
        }
    }

    struct Fixture {
        pool: PgPool,
        router: Router,
        ws: WorkspaceId,
        admin: String,
        write: String,
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        zirv_flow_store::migrate(pool).await.unwrap();
        let name = zirv_flow_core::WorkspaceName::parse("ours").unwrap();
        let slug = zirv_flow_core::Slug::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let key = |scope: Scope| {
            let pool = pool.clone();
            async move {
                admin::create_api_key(&pool, &Context::system(), ws, "seed", &[scope])
                    .await
                    .unwrap()
                    .plaintext
            }
        };
        Fixture {
            pool: pool.clone(),
            router: router(AppState::new(pool.clone(), Arc::new(String::new))),
            ws,
            admin: key(Scope::Admin).await,
            write: key(Scope::Write).await,
        }
    }

    impl Fixture {
        async fn send(
            &self,
            token: &str,
            path: &str,
            content_type: &str,
            idempotency_key: Option<&str>,
            body: Body,
        ) -> Reply {
            let mut request = Request::builder()
                .method("POST")
                .uri(format!("/api/v1/workspaces/{}/{path}", self.ws.as_uuid()))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, content_type);
            if let Some(key) = idempotency_key {
                request = request.header("idempotency-key", key);
            }
            let response = self
                .router
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
                text: String::from_utf8_lossy(&bytes).into_owned(),
            }
        }

        async fn api_key(&self, name: &str, key: Option<&str>) -> Reply {
            let body = json!({"name": name, "scopes": ["read"]}).to_string();
            self.send(
                &self.admin,
                "api-keys",
                "application/json",
                key,
                Body::from(body),
            )
            .await
        }

        async fn asset(&self, name: &str, key: Option<&str>, bytes: Vec<u8>) -> Reply {
            self.send(
                &self.write,
                &format!("assets?name={name}"),
                "image/png",
                key,
                Body::from(bytes),
            )
            .await
        }

        async fn import(&self, csv: &str, key: Option<&str>) -> Reply {
            self.send(
                &self.write,
                "imports?name=people",
                "text/csv",
                key,
                Body::from(csv.to_owned()),
            )
            .await
        }

        async fn count(&self, table: &str) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_upload_with_the_same_key_and_file_replays_its_status_body_and_id(pool: PgPool) {
        let f = fixture(&pool).await;
        let first = f.asset("banner", Some("k-1"), png(64, 32)).await;
        let again = f.asset("banner", Some("k-1"), png(64, 32)).await;

        assert_eq!(first.status, StatusCode::CREATED, "{}", first.text);
        assert_eq!(again.status, StatusCode::CREATED, "{}", again.text);
        assert_eq!(again.json(), first.json());
        assert_eq!(again.json()["id"], first.json()["id"]);
        assert!(first.headers.get("idempotent-replayed").is_none());
        assert_eq!(again.headers["idempotent-replayed"], "true");
        assert_eq!(f.count("assets").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_upload_with_the_same_key_and_another_file_is_422_idempotency_key_reused(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        f.asset("banner", Some("k-1"), png(64, 32)).await;

        let other = f.asset("banner", Some("k-1"), png(65, 32)).await;

        assert_eq!(other.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(other.json()["code"], "idempotency_key_reused");
        assert_eq!(f.count("assets").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_upload_without_a_key_behaves_as_before(pool: PgPool) {
        let f = fixture(&pool).await;

        let first = f.asset("banner", None, png(64, 32)).await;
        let again = f.asset("banner", None, png(64, 32)).await;

        assert_eq!(first.status, StatusCode::CREATED);
        assert_eq!(again.status, StatusCode::OK);
        assert_eq!(f.count("api_idempotency").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replayed_api_key_creation_is_409_with_the_key_id_and_never_the_secret(pool: PgPool) {
        let f = fixture(&pool).await;
        let first = f.api_key("ci", Some("k-1")).await;
        let secret = first.json()["key"].as_str().unwrap().to_owned();
        let id = first.json()["id"].as_str().unwrap().to_owned();

        let again = f.api_key("ci", Some("k-1")).await;

        assert_eq!(first.status, StatusCode::CREATED, "{}", first.text);
        assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.text);
        assert_eq!(again.json()["code"], "idempotency_key_replayed");
        assert_eq!(again.json()["resource_id"], id);
        assert!(!again.text.contains(&secret));
        let stored: Vec<String> =
            sqlx::query_scalar("SELECT to_jsonb(t)::text FROM api_idempotency t")
                .fetch_all(&f.pool)
                .await
                .unwrap();
        assert!(stored.iter().all(|row| !row.contains(&secret)));
        let created: i64 = sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE name = $1")
            .bind("ci")
            .fetch_one(&f.pool)
            .await
            .unwrap();
        assert_eq!(created, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_api_key_with_the_same_key_and_another_body_is_422_idempotency_key_reused(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        f.api_key("ci", Some("k-1")).await;

        let other = f.api_key("deploy", Some("k-1")).await;

        assert_eq!(other.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(other.json()["code"], "idempotency_key_reused");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_same_key_on_another_route_is_a_different_key(pool: PgPool) {
        let f = fixture(&pool).await;
        let key = f.api_key("ci", Some("k-1")).await;
        let asset = f.asset("banner", Some("k-1"), png(64, 32)).await;

        assert_eq!(key.status, StatusCode::CREATED);
        assert_eq!(asset.status, StatusCode::CREATED);
        assert_eq!(f.count("api_idempotency").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_streamed_import_replays_as_409_and_another_file_under_the_key_is_422(pool: PgPool) {
        let f = fixture(&pool).await;
        let first = f.import("email\nzed@example.com\n", Some("k-1")).await;
        let id = first.json()["id"].as_str().unwrap().to_owned();

        let again = f.import("email\nzed@example.com\n", Some("k-1")).await;
        let other = f.import("email\nann@example.com\n", Some("k-1")).await;

        assert_eq!(first.status, StatusCode::CREATED, "{}", first.text);
        assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.text);
        assert_eq!(again.json()["resource_id"], id);
        assert!(!again.text.contains("zed@example.com"));
        assert_eq!(other.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(other.json()["code"], "idempotency_key_reused");
        assert_eq!(f.count("imports").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_table_holds_the_key_only_as_a_hash(pool: PgPool) {
        let f = fixture(&pool).await;
        f.api_key("ci", Some("zed@example.com")).await;

        let rows: Vec<String> =
            sqlx::query_scalar("SELECT to_jsonb(t)::text FROM api_idempotency t")
                .fetch_all(&f.pool)
                .await
                .unwrap();

        assert_eq!(rows.len(), 1);
        assert!(!rows[0].contains("zed@example.com"));
    }
}
