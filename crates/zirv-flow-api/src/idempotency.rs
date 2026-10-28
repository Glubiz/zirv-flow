//! `Idempotency-Key` on the creating requests that no unique name or draft revision guards
//! (architecture section 15).
//!
//! A request with the header first reserves its key in a short transaction and then holds nothing:
//! no connection and no lock while the work runs. The create marks the reservation done inside its
//! own transaction, so it commits only while it still owns the key. Another request with the key
//! meanwhile gets `409 idempotency_key_in_flight` with `Retry-After`; once the create is done the
//! same key and request answer `409 idempotency_key_replayed` with the created resource's id; the
//! same key with another request is `422 idempotency_key_reused`. No response body is stored. The
//! key belongs to its caller: the user or the API key that sent it. A request without the header
//! behaves as it always did.

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::token::{Sha256Stream, sha256_bytes};
use zirv_flow_store::{ApiIdempotencyKey, PgConnection, PgPool, Reserved};

use crate::auth::Principal;
use crate::problem::{ApiError, Problem};
use crate::{request_id, trace};

const MAX_KEY_CHARS: usize = 255;
const REPLAYED: HeaderName = HeaderName::from_static("idempotent-replayed");

/// How long a reservation stays valid, per route, by what bounds the route's duration. A retry
/// that arrives after the lease may take the key over; the original then fails to complete and
/// rolls back.
pub(crate) mod lease {
    /// A short write: an API key, an erasure or an export request. Seconds.
    pub(crate) const QUICK: i64 = 30;
    /// An image upload is reserved after its body (at most `assets.max_file_bytes`, 5 MiB) has been
    /// read, so only the store, the quota check and the audit entry remain. Seconds.
    pub(crate) const ASSET: i64 = 60;
    /// A CSV upload is reserved before its body streams. A stall is cut after 60 seconds, and
    /// `imports.max_file_bytes` (512 MiB) at 1 MiB/s takes under 9 minutes, so 15 minutes. Seconds.
    pub(crate) const IMPORT: i64 = 900;

    /// An assistant draft: `1 + ai.max_repair_attempts` model calls of at most 60 seconds each and
    /// 30 seconds for the rest, which is the read timeout the draft route documents. Seconds.
    pub(crate) fn draft(max_repair_attempts: u32) -> i64 {
        (1 + i64::from(max_repair_attempts)) * 60 + 30
    }
}

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

/// The `409` of a replay: the key already created a resource. The response is not kept.
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

/// The two shapes a `409` of an idempotent create may have: a problem of the route, or the
/// replay of its key.
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
pub enum ConflictProblem {
    Problem(Box<Problem>),
    Replayed(ReplayedProblem),
}

impl From<Problem> for ConflictProblem {
    fn from(problem: Problem) -> Self {
        Self::Problem(Box::new(problem))
    }
}

impl From<ReplayedProblem> for ConflictProblem {
    fn from(problem: ReplayedProblem) -> Self {
        Self::Replayed(problem)
    }
}

/// Declares `ConflictProblem` as `anyOf` its two shapes. A replay body is a problem with one
/// more field, and a problem allows extra properties, so it matches both: `oneOf` would reject it.
pub(crate) struct ConflictSchema;

impl utoipa::Modify for ConflictSchema {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::schema::{AnyOfBuilder, Schema};
        use utoipa::openapi::{Ref, RefOr};
        let components = openapi.components.get_or_insert_with(Default::default);
        let any_of = AnyOfBuilder::new()
            .description(Some(
                "The two shapes a `409` of an idempotent create may have: a problem of the route, or the replay of its key, which is a problem with `resource_id`.",
            ))
            .item(Ref::from_schema_name("Problem"))
            .item(Ref::from_schema_name("ReplayedProblem"))
            .build();
        components
            .schemas
            .insert("ConflictProblem".into(), RefOr::T(Schema::AnyOf(any_of)));
    }
}

/// The key a request sent, for one route and one caller.
pub(crate) struct Keyed {
    route: &'static str,
    key_hash: [u8; 32],
}

impl Keyed {
    /// The key of the request on `route`, or `None` when it sent no header. The caller is part
    /// of the key's identity, so another member's identical key is another key.
    pub(crate) fn of(
        headers: &HeaderMap,
        route: &'static str,
        principal: &Principal,
    ) -> Result<Option<Self>, ApiError> {
        let scope = match principal {
            Principal::Session(session) => format!("user:{}", session.user.id.as_uuid()),
            Principal::Key(key) => format!("key:{}", key.key.id.as_uuid()),
        };
        Ok(key_header(headers)?.map(|key| Self::scoped(route, &scope, key)))
    }

    fn scoped(route: &'static str, scope: &str, key: &str) -> Self {
        Self {
            route,
            key_hash: sha256_bytes(format!("{scope}\n{key}").as_bytes()),
        }
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

/// A key this request owns. Dropped without [`finish`], the reservation is deleted, best effort:
/// the lease covers a crash.
pub(crate) struct Reservation {
    pool: PgPool,
    workspace: WorkspaceId,
    route: &'static str,
    key_hash: [u8; 32],
    id: Uuid,
    finished: bool,
}

impl Reservation {
    fn key(&self) -> ApiIdempotencyKey<'_> {
        ApiIdempotencyKey {
            workspace: self.workspace,
            route: self.route,
            key_hash: &self.key_hash,
        }
    }

    /// Sets the hash of an upload that was hashed while it streamed.
    pub(crate) async fn bind_hash(&self, request_hash: [u8; 32]) -> Result<(), ApiError> {
        if zirv_flow_store::bind_api_idempotency_hash(
            &self.pool,
            self.key(),
            self.id,
            &request_hash,
        )
        .await?
        {
            Ok(())
        } else {
            Err(in_flight(1))
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (pool, workspace, route, key_hash, id) = (
            self.pool.clone(),
            self.workspace,
            self.route,
            self.key_hash,
            self.id,
        );
        runtime.spawn(async move {
            let key = ApiIdempotencyKey {
                workspace,
                route,
                key_hash: &key_hash,
            };
            if let Err(error) = zirv_flow_store::release_api_idempotency(&pool, key, id).await {
                tracing::debug!("could not release an idempotency reservation: {error}");
            }
        });
    }
}

/// Marks the key done inside the create's own transaction. When the reservation was taken
/// over, this answers `409 idempotency_key_in_flight` and the caller drops its transaction, which
/// rolls the create back.
pub(crate) async fn complete(
    reservation: Option<&Reservation>,
    connection: &mut PgConnection,
    status: StatusCode,
    resource_id: Uuid,
) -> Result<(), ApiError> {
    let Some(reservation) = reservation else {
        return Ok(());
    };
    if zirv_flow_store::complete_api_idempotency(
        connection,
        reservation.key(),
        reservation.id,
        status.as_u16(),
        resource_id,
    )
    .await?
    {
        Ok(())
    } else {
        Err(in_flight(1))
    }
}

/// Ends the reservation after the create committed: nothing is left to release.
pub(crate) fn finish(reservation: Option<Reservation>) {
    if let Some(mut reservation) = reservation {
        reservation.finished = true;
    }
}

fn in_flight(retry_after: u64) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "another request with this Idempotency-Key is still running; retry after the delay",
    )
    .with_code("idempotency_key_in_flight")
    .with_retry_after(retry_after)
}

fn replayed(resource_id: Uuid) -> Response {
    let body = ConflictProblem::from(ReplayedProblem {
        kind: "about:blank".into(),
        title: "Conflict".into(),
        status: 409,
        detail: "this Idempotency-Key already created a resource; its response is not kept".into(),
        request_id: request_id::current(),
        trace_id: trace::current().map(|trace| trace.trace_id.to_string()),
        code: "idempotency_key_replayed".into(),
        resource_id,
    });
    let mut response = (
        StatusCode::CONFLICT,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        )],
        serde_json::to_vec(&body).unwrap_or_default(),
    )
        .into_response();
    response
        .headers_mut()
        .insert(REPLAYED, HeaderValue::from_static("true"));
    response
}

/// What reserving a key decided for a request whose hash is known.
pub(crate) enum Entry {
    /// Run the request. The reservation is present when there is a key.
    Go(Option<Reservation>),
    /// Answer with this: a replay, a reuse or a request in flight.
    Answer(Response),
}

/// Reserves the key of a request whose hash is known up front. Without a key the request runs
/// as it always did.
pub(crate) async fn reserve(
    pool: &PgPool,
    workspace: WorkspaceId,
    keyed: Option<Keyed>,
    request_hash: [u8; 32],
    lease_seconds: i64,
) -> Result<Entry, ApiError> {
    Ok(
        match open(pool, workspace, keyed, Some(request_hash), lease_seconds).await? {
            Opened::Go(reservation) => Entry::Go(reservation),
            Opened::Answer(response) => Entry::Answer(response),
            Opened::Done(done) => Entry::Answer(done.answer(request_hash)),
        },
    )
}

/// A key whose request finished earlier.
pub(crate) struct Done {
    request_hash: Vec<u8>,
    resource_id: Uuid,
}

impl Done {
    /// The answer to a request with `request_hash`: the replay, or `422 idempotency_key_reused`
    /// when the request is another one.
    pub(crate) fn answer(self, request_hash: [u8; 32]) -> Response {
        if self.request_hash == request_hash {
            replayed(self.resource_id)
        } else {
            ApiError::idempotency_key_reused().into_response()
        }
    }
}

/// What reserving a key decided for an upload whose hash is known only after its body streamed.
pub(crate) enum UnhashedEntry {
    /// Run the request, then [`Reservation::bind_hash`] before completing.
    Go(Option<Reservation>),
    /// Answer with this: a request in flight.
    Answer(Response),
    /// The key's request finished earlier: read the body only to hash it, then [`Done::answer`].
    Done(Done),
}

/// Reserves the key of an upload that is hashed while it streams.
pub(crate) async fn reserve_unhashed(
    pool: &PgPool,
    workspace: WorkspaceId,
    keyed: Option<Keyed>,
    lease_seconds: i64,
) -> Result<UnhashedEntry, ApiError> {
    Ok(
        match open(pool, workspace, keyed, None, lease_seconds).await? {
            Opened::Go(reservation) => UnhashedEntry::Go(reservation),
            Opened::Answer(response) => UnhashedEntry::Answer(response),
            Opened::Done(done) => UnhashedEntry::Done(done),
        },
    )
}

enum Opened {
    Go(Option<Reservation>),
    Answer(Response),
    Done(Done),
}

async fn open(
    pool: &PgPool,
    workspace: WorkspaceId,
    keyed: Option<Keyed>,
    request_hash: Option<[u8; 32]>,
    lease_seconds: i64,
) -> Result<Opened, ApiError> {
    let Some(keyed) = keyed else {
        return Ok(Opened::Go(None));
    };
    let key = ApiIdempotencyKey {
        workspace,
        route: keyed.route,
        key_hash: &keyed.key_hash,
    };
    let reserved = zirv_flow_store::reserve_api_idempotency(
        pool,
        key,
        request_hash.as_ref().map(<[u8; 32]>::as_slice),
        lease_seconds,
    )
    .await?;
    Ok(match reserved {
        Reserved::Fresh(id) => Opened::Go(Some(Reservation {
            pool: pool.clone(),
            workspace,
            route: keyed.route,
            key_hash: keyed.key_hash,
            id,
            finished: false,
        })),
        Reserved::Done {
            request_hash,
            resource_id,
        } => Opened::Done(Done {
            request_hash,
            resource_id,
        }),
        Reserved::InFlight { retry_after } => {
            Opened::Answer(in_flight(retry_after).into_response())
        }
        Reserved::Reused => Opened::Answer(ApiError::idempotency_key_reused().into_response()),
    })
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use serde_json::{Value, json};

    use super::*;

    fn headers(key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", HeaderValue::from_str(key).unwrap());
        headers
    }

    async fn body_of(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
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
    fn the_key_is_hashed_with_its_caller_and_its_route() {
        let one = Keyed::scoped("create_x", "user:1", "k");

        assert_eq!(
            one.key_hash,
            Keyed::scoped("create_x", "user:1", "k").key_hash
        );
        assert_ne!(
            one.key_hash,
            Keyed::scoped("create_x", "user:2", "k").key_hash
        );
        assert_ne!(
            one.key_hash,
            Keyed::scoped("create_x", "key:1", "k").key_hash
        );
        assert_ne!(
            one.key_hash,
            Keyed::scoped("create_x", "user:1", "j").key_hash
        );
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

    #[test]
    fn a_draft_lease_covers_every_model_call_and_the_rest() {
        assert_eq!(lease::draft(0), 90);
        assert_eq!(lease::draft(2), 210);
    }

    #[tokio::test]
    async fn a_replay_is_409_with_the_resource_id_and_no_other_body() {
        let id = Uuid::now_v7();
        let done = Done {
            request_hash: vec![1; 32],
            resource_id: id,
        };

        let response = done.answer([1; 32]);

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(response.headers()["idempotent-replayed"], "true");
        let body = body_of(response).await;
        assert_eq!(body["code"], "idempotency_key_replayed");
        assert_eq!(body["resource_id"], id.to_string());
        assert_eq!(body["status"], 409);
    }

    #[test]
    fn another_request_under_a_finished_key_is_422_idempotency_key_reused() {
        let done = Done {
            request_hash: vec![1; 32],
            resource_id: Uuid::nil(),
        };

        assert_eq!(
            done.answer([2; 32]).status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[tokio::test]
    async fn a_request_in_flight_is_409_with_a_code_and_retry_after() {
        let response = in_flight(7).into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(response.headers()[header::RETRY_AFTER], "7");
        assert_eq!(body_of(response).await["code"], "idempotency_key_in_flight");
    }
}

#[cfg(test)]
mod acceptance {
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::postgres::PgPoolOptions;
    use tokio::sync::mpsc;
    use tower::ServiceExt;
    use zirv_flow_core::{ApiKeyId, Scope};

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
        admin_id: ApiKeyId,
        other_admin: String,
        write: String,
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        fixture_on(pool, pool).await
    }

    /// `serving` is the pool the router uses; the seed data goes through `pool`.
    async fn fixture_on(pool: &PgPool, serving: &PgPool) -> Fixture {
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
            }
        };
        let admin = key(Scope::Admin).await;
        Fixture {
            pool: pool.clone(),
            router: router(
                AppState::new(serving.clone(), Arc::new(String::new)).with_hmac_root(
                    &zirv_flow_core::HmacRoot::bootstrap_from(
                        &zirv_flow_core::MasterKey::generate(),
                    ),
                ),
            ),
            ws,
            admin_id: admin.key.id,
            admin: admin.plaintext,
            other_admin: key(Scope::Admin).await.plaintext,
            write: key(Scope::Write).await.plaintext,
        }
    }

    impl Fixture {
        fn request(
            &self,
            token: &str,
            path: &str,
            content_type: &str,
            idempotency_key: Option<&str>,
            body: Body,
        ) -> Request<Body> {
            let mut request = Request::builder()
                .method("POST")
                .uri(format!("/api/v1/workspaces/{}/{path}", self.ws.as_uuid()))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, content_type);
            if let Some(key) = idempotency_key {
                request = request.header("idempotency-key", key);
            }
            request.body(body).unwrap()
        }

        async fn send(&self, request: Request<Body>) -> Reply {
            reply(&self.router, request).await
        }

        async fn api_key_as(&self, token: &str, name: &str, key: Option<&str>) -> Reply {
            let body = json!({"name": name, "scopes": ["read"]}).to_string();
            self.send(self.request(token, "api-keys", "application/json", key, Body::from(body)))
                .await
        }

        async fn api_key(&self, name: &str, key: Option<&str>) -> Reply {
            self.api_key_as(&self.admin, name, key).await
        }

        async fn asset(&self, name: &str, key: Option<&str>, bytes: Vec<u8>) -> Reply {
            self.send(self.request(
                &self.write,
                &format!("assets?name={name}"),
                "image/png",
                key,
                Body::from(bytes),
            ))
            .await
        }

        async fn import(&self, csv: &str, key: Option<&str>) -> Reply {
            self.send(self.request(
                &self.write,
                "imports?name=people",
                "text/csv",
                key,
                Body::from(csv.to_owned()),
            ))
            .await
        }

        /// An import whose body the test feeds piece by piece.
        fn stalled_import(
            &self,
            key: &str,
            first: &'static str,
        ) -> (mpsc::UnboundedSender<Bytes>, tokio::task::JoinHandle<Reply>) {
            let (sender, mut receiver) = mpsc::unbounded_channel::<Bytes>();
            sender.send(Bytes::from_static(first.as_bytes())).unwrap();
            let stream = futures_util::stream::poll_fn(move |context| {
                receiver
                    .poll_recv(context)
                    .map(|item| item.map(Ok::<_, Infallible>))
            });
            let request = self.request(
                &self.write,
                "imports?name=people",
                "text/csv",
                Some(key),
                Body::from_stream(stream),
            );
            let router = self.router.clone();
            (
                sender,
                tokio::spawn(async move { reply(&router, request).await }),
            )
        }

        /// A person the privacy requests can name.
        async fn person(&self) -> Uuid {
            let outcome = zirv_flow_store::apply_profile_batch(
                &self.pool,
                self.ws,
                &[zirv_flow_store::ProfileRecord {
                    source: "crm".into(),
                    stream: "contacts".into(),
                    record_key: "p-1".into(),
                    identifiers: vec![("external:crm".into(), "p-1-ext".into())],
                    traits: serde_json::Map::new(),
                    observed_at: time::macros::datetime!(2026-01-01 00:00 UTC),
                    trace: zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("test")),
                }],
            )
            .await
            .unwrap();
            match &outcome[0] {
                zirv_flow_store::ProfileOutcome::Applied { person_id, .. } => *person_id,
                other => panic!("not applied: {other:?}"),
            }
        }

        async fn privacy(&self, path: &str, person: Uuid, key: Option<&str>) -> Reply {
            let body = json!({ "person_id": person }).to_string();
            self.send(self.request(&self.admin, path, "application/json", key, Body::from(body)))
                .await
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        /// Waits for `sql` to count `wanted` rows: a reservation is released in the background.
        async fn wait_for(&self, sql: &str, wanted: i64) {
            for _ in 0..100 {
                if self.count(sql).await == wanted {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("{sql} never counted {wanted}");
        }

        /// A pending reservation for the key-creating request the test is about to send.
        async fn reserve_api_key(&self, name: &str, key: &str, lease: i64) -> Uuid {
            let uri: Uri = format!("/api/v1/workspaces/{}/api-keys", self.ws.as_uuid())
                .parse()
                .unwrap();
            let hash = RequestHash::of_json(
                &Method::POST,
                &uri,
                &json!({"name": name, "scopes": ["read"]}),
            )
            .unwrap();
            let keyed = Keyed::scoped(
                "create_api_key",
                &format!("key:{}", self.admin_id.as_uuid()),
                key,
            );
            match zirv_flow_store::reserve_api_idempotency(
                &self.pool,
                ApiIdempotencyKey {
                    workspace: self.ws,
                    route: keyed.route,
                    key_hash: &keyed.key_hash,
                },
                Some(&hash),
                lease,
            )
            .await
            .unwrap()
            {
                Reserved::Fresh(id) => id,
                other => panic!("expected a fresh reservation, got {other:?}"),
            }
        }
    }

    async fn reply(router: &Router, request: Request<Body>) -> Reply {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            text: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_upload_with_the_same_key_and_file_replays_as_409_with_the_id_and_no_body(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let first = f.asset("banner", Some("k-1"), png(64, 32)).await;
        let again = f.asset("banner", Some("k-1"), png(64, 32)).await;

        assert_eq!(first.status, StatusCode::CREATED, "{}", first.text);
        assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.text);
        assert_eq!(again.json()["code"], "idempotency_key_replayed");
        assert_eq!(again.json()["resource_id"], first.json()["id"]);
        assert_eq!(again.headers["idempotent-replayed"], "true");
        assert!(!again.text.contains("banner"));
        assert!(first.headers.get("idempotent-replayed").is_none());
        assert_eq!(f.count("SELECT count(*) FROM assets").await, 1);
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
        assert_eq!(f.count("SELECT count(*) FROM assets").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_upload_without_a_key_behaves_as_before(pool: PgPool) {
        let f = fixture(&pool).await;

        let first = f.asset("banner", None, png(64, 32)).await;
        let again = f.asset("banner", None, png(64, 32)).await;

        assert_eq!(first.status, StatusCode::CREATED);
        assert_eq!(again.status, StatusCode::OK);
        assert_eq!(f.count("SELECT count(*) FROM api_idempotency").await, 0);
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
        for field in ["type", "title", "status", "detail", "request_id"] {
            assert!(again.json().get(field).is_some(), "{field}");
        }
        assert!(!again.text.contains(&secret));
        let stored: Vec<String> =
            sqlx::query_scalar("SELECT to_jsonb(t)::text FROM api_idempotency t")
                .fetch_all(&f.pool)
                .await
                .unwrap();
        assert!(stored.iter().all(|row| !row.contains(&secret)));
        assert_eq!(
            f.count("SELECT count(*) FROM api_keys WHERE name = 'ci'")
                .await,
            1
        );
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
        assert_eq!(f.count("SELECT count(*) FROM api_idempotency").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn another_principal_with_the_same_key_and_body_creates_its_own_resource(pool: PgPool) {
        let f = fixture(&pool).await;
        let mine = f.api_key("ci", Some("k-1")).await;

        let theirs = f.api_key_as(&f.other_admin, "ci", Some("k-1")).await;
        let mine_again = f.api_key("ci", Some("k-1")).await;

        assert_eq!(mine.status, StatusCode::CREATED);
        assert_eq!(theirs.status, StatusCode::CREATED, "{}", theirs.text);
        assert_ne!(theirs.json()["id"], mine.json()["id"]);
        assert_eq!(mine_again.status, StatusCode::CONFLICT);
        assert_eq!(
            f.count("SELECT count(*) FROM api_keys WHERE name = 'ci'")
                .await,
            2
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_overlapping_request_with_the_same_key_is_409_in_flight_and_creates_nothing(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        f.reserve_api_key("ci", "k-1", 60).await;

        let overlap = f.api_key("ci", Some("k-1")).await;

        assert_eq!(overlap.status, StatusCode::CONFLICT, "{}", overlap.text);
        assert_eq!(overlap.json()["code"], "idempotency_key_in_flight");
        let retry: u64 = overlap.headers[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((1..=60).contains(&retry));
        assert_eq!(
            f.count("SELECT count(*) FROM api_keys WHERE name = 'ci'")
                .await,
            0
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_expired_pending_reservation_is_taken_over_and_the_retry_creates_exactly_one(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        f.reserve_api_key("ci", "k-1", 60).await;
        sqlx::query("UPDATE api_idempotency SET lease_until = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();

        let retry = f.api_key("ci", Some("k-1")).await;
        let again = f.api_key("ci", Some("k-1")).await;

        assert_eq!(retry.status, StatusCode::CREATED, "{}", retry.text);
        assert_eq!(again.status, StatusCode::CONFLICT);
        assert_eq!(again.json()["resource_id"], retry.json()["id"]);
        assert_eq!(
            f.count("SELECT count(*) FROM api_keys WHERE name = 'ci'")
                .await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failed_first_attempt_leaves_no_reservation_and_the_retry_creates(pool: PgPool) {
        let f = fixture(&pool).await;

        let refused = f.api_key("", Some("k-1")).await;
        f.wait_for("SELECT count(*) FROM api_idempotency", 0).await;
        let retry = f.api_key("ci", Some("k-1")).await;

        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(retry.status, StatusCode::CREATED, "{}", retry.text);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_create_whose_reservation_was_taken_over_rolls_back_and_answers_409(pool: PgPool) {
        let f = fixture(&pool).await;
        let reservation = f.reserve_api_key("ci", "k-1", 60).await;
        let reservation = Reservation {
            pool: pool.clone(),
            workspace: f.ws,
            route: "create_api_key",
            key_hash: Keyed::scoped(
                "create_api_key",
                &format!("key:{}", f.admin_id.as_uuid()),
                "k-1",
            )
            .key_hash,
            id: reservation,
            finished: true,
        };
        // Another request takes the key over while this one is creating.
        sqlx::query("UPDATE api_idempotency SET reservation = gen_random_uuid()")
            .execute(&pool)
            .await
            .unwrap();

        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let created =
            admin::create_api_key_in(&mut tx, &Context::system(), f.ws, "taken", &[Scope::Read])
                .await
                .unwrap();
        let outcome = complete(
            Some(&reservation),
            &mut tx,
            StatusCode::CREATED,
            created.key.id.as_uuid(),
        )
        .await;
        drop(tx);

        let error = outcome.unwrap_err();
        assert_eq!(error.status(), StatusCode::CONFLICT);
        assert_eq!(error.code(), Some("idempotency_key_in_flight"));
        assert_eq!(
            f.count("SELECT count(*) FROM api_keys WHERE name = 'taken'")
                .await,
            0
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_import_taken_over_while_it_streams_answers_409_and_leaves_no_import(pool: PgPool) {
        let f = fixture(&pool).await;
        let (sender, slow) = f.stalled_import("k-1", "email\n");
        f.wait_for("SELECT count(*) FROM api_idempotency", 1).await;
        sqlx::query("UPDATE api_idempotency SET lease_until = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();

        let taker = f.import("email\nzed@example.com\n", Some("k-1")).await;
        sender
            .send(Bytes::from_static(b"zed@example.com\n"))
            .unwrap();
        drop(sender);
        let slow = slow.await.unwrap();

        assert_eq!(taker.status, StatusCode::CREATED, "{}", taker.text);
        assert_eq!(slow.status, StatusCode::CONFLICT, "{}", slow.text);
        assert_eq!(slow.json()["code"], "idempotency_key_in_flight");
        f.wait_for("SELECT count(*) FROM imports", 1).await;
        assert_eq!(
            f.count("SELECT count(*) FROM imports WHERE status = 'uploaded'")
                .await,
            1
        );
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
        assert_eq!(f.count("SELECT count(*) FROM imports").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replayed_erasure_request_is_409_with_the_same_id_and_adds_no_request_or_job(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let person = f.person().await;
        let first = f.privacy("privacy/erasures", person, Some("k-1")).await;
        let requests = f.count("SELECT count(*) FROM erasure_requests").await;
        let jobs = f.count("SELECT count(*) FROM jobs").await;

        let again = f.privacy("privacy/erasures", person, Some("k-1")).await;

        assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.text);
        assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.text);
        assert_eq!(again.json()["code"], "idempotency_key_replayed");
        assert_eq!(again.json()["resource_id"], first.json()["id"]);
        assert!(!again.text.contains(&person.to_string()));
        assert_eq!(
            f.count("SELECT count(*) FROM erasure_requests").await,
            requests
        );
        assert_eq!(f.count("SELECT count(*) FROM jobs").await, jobs);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erasure_request_with_another_body_under_the_key_is_422_idempotency_key_reused(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let person = f.person().await;
        f.privacy("privacy/erasures", person, Some("k-1")).await;

        let other = f
            .privacy("privacy/erasures", Uuid::now_v7(), Some("k-1"))
            .await;

        assert_eq!(
            other.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            other.text
        );
        assert_eq!(other.json()["code"], "idempotency_key_reused");
        assert_eq!(f.count("SELECT count(*) FROM erasure_requests").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replayed_export_request_is_409_with_the_same_id_and_adds_no_export_or_job(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let person = f.person().await;
        let first = f.privacy("privacy/exports", person, Some("k-1")).await;
        let exports = f.count("SELECT count(*) FROM privacy_exports").await;
        let jobs = f.count("SELECT count(*) FROM jobs").await;

        let again = f.privacy("privacy/exports", person, Some("k-1")).await;

        assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.text);
        assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.text);
        assert_eq!(again.json()["code"], "idempotency_key_replayed");
        assert_eq!(again.json()["resource_id"], first.json()["id"]);
        assert_eq!(
            f.count("SELECT count(*) FROM privacy_exports").await,
            exports
        );
        assert_eq!(f.count("SELECT count(*) FROM jobs").await, jobs);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_export_request_with_another_body_under_the_key_is_422_idempotency_key_reused(
        pool: PgPool,
    ) {
        let f = fixture(&pool).await;
        let person = f.person().await;
        f.privacy("privacy/exports", person, Some("k-1")).await;

        let other = f
            .privacy("privacy/exports", Uuid::now_v7(), Some("k-1"))
            .await;

        assert_eq!(
            other.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            other.text
        );
        assert_eq!(other.json()["code"], "idempotency_key_reused");
        assert_eq!(f.count("SELECT count(*) FROM privacy_exports").await, 1);
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

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn keyed_requests_in_flight_do_not_hold_pool_connections(pool: PgPool) {
        let small = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with((*pool.connect_options()).clone())
            .await
            .unwrap();
        let f = fixture_on(&pool, &small).await;
        let (first, first_reply) = f.stalled_import("k-1", "email\n");
        let (second, second_reply) = f.stalled_import("k-2", "email\n");
        f.wait_for("SELECT count(*) FROM api_idempotency", 2).await;

        let unkeyed = f.api_key("ci", None).await;

        first.send(Bytes::from_static(b"a@example.com\n")).unwrap();
        second.send(Bytes::from_static(b"b@example.com\n")).unwrap();
        drop((first, second));
        assert_eq!(unkeyed.status, StatusCode::CREATED, "{}", unkeyed.text);
        assert_eq!(first_reply.await.unwrap().status, StatusCode::CREATED);
        assert_eq!(second_reply.await.unwrap().status, StatusCode::CREATED);
    }
}
