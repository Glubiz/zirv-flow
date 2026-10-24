//! The public routes of a sent message (architecture 10.2): the open pixel, the click redirect,
//! the unsubscribe page, RFC 8058 one-click unsubscribe and the preference center.
//!
//! - `GET /t/o/{token}` returns a transparent GIF and records `email.opened`.
//! - `GET /t/c/{token}` redirects to the recorded link and records `email.clicked`.
//! - `GET /u/{token}` shows a confirmation; `POST /u/{token}` unsubscribes (the form button and the
//!   one-click post of a mail client).
//! - `GET` and `POST /u/{token}/preferences` show and save the person's choices.
//!
//! **Authorization.** There is no session, no `Principal` and no CSRF check: the signed token
//! authorises the one message it names, and nothing else. A token of the wrong kind, with a bad MAC
//! or a bad length is a 404 page, as is a token for a message that is gone. A prefetcher that
//! follows an unsubscribe link changes nothing, because only a POST changes consent.
//!
//! **Rate limit.** Per message, after the token verified, so an unsigned guess costs no bucket.
//!
//! **Privacy.** Neither the IP address nor the user agent is stored or logged. The user agent is
//! read only to decide whether an open looks automated; the verdict is what is stored.

mod machine;
mod pages;
mod preferences;

use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};
use tracing::warn;
use zirv_flow_core::tracking::{Claims, TokenKind, verify};
use zirv_flow_store::{
    Engagement, Unsubscribed, find_message_link, find_tracked_message, record_engagement,
    unsubscribe_message,
};

use crate::AppState;
use crate::ingest::RateLimiter;
use crate::trace::request_trace;

pub(crate) use preferences::{preferences_page, preferences_save};

/// Requests per second one message may make, sustained.
const RATE_PER_MESSAGE: u32 = 5;
/// Requests one message may make at once after a quiet period.
const BURST_PER_MESSAGE: u32 = 30;
/// The largest body of an unsubscribe post: the one-click form is 26 bytes.
pub(crate) const UNSUBSCRIBE_BODY_LIMIT: usize = 4 * 1024;
/// The largest body of a preference post.
pub(crate) const PREFERENCES_BODY_LIMIT: usize = 64 * 1024;

/// A 1x1 transparent GIF.
const PIXEL: [u8; 43] = [
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];

/// The limiter of the tracking routes.
pub(crate) fn limiter() -> RateLimiter {
    RateLimiter::new(RATE_PER_MESSAGE, BURST_PER_MESSAGE, Instant::now())
}

/// The claims of a token of `kind` that may be used now, or the page that answers instead: a 404
/// for anything that does not verify, a 429 past the message's rate.
pub(super) fn admit(
    state: &AppState,
    kind: TokenKind,
    token: &str,
) -> Result<Claims, Box<Response>> {
    let refuse = |response: Response| Box::new(response);
    let key = state
        .tracking_key
        .as_deref()
        .ok_or_else(|| refuse(pages::not_found()))?;
    let claims = verify(key, kind, token).ok_or_else(|| refuse(pages::not_found()))?;
    // Only a click names a link; a token for the others carries 0.
    if kind != TokenKind::Click && claims.link != 0 {
        return Err(refuse(pages::not_found()));
    }
    state
        .tracking_limiter
        .take(claims.message, 1, Instant::now())
        .map_err(|retry| refuse(pages::rate_limited(retry)))?;
    Ok(claims)
}

fn gif() -> Response {
    let mut response = (StatusCode::OK, PIXEL.to_vec()).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/gif"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate, max-age=0"),
    );
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

/// `GET /t/o/{token}`. Always the pixel for a valid token, even when nothing is recorded: the
/// message is not `sent` (an open of a message that never went out is not an open), the person is
/// gone, or the write failed.
pub(crate) async fn open(
    State(state): State<AppState>,
    Path(token): Path<String>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let claims = match admit(&state, TokenKind::Open, &token) {
        Ok(claims) => claims,
        Err(response) => return *response,
    };
    if method != Method::GET {
        return gif();
    }
    let message = match find_tracked_message(&state.pool, claims.message).await {
        Ok(found) => found,
        Err(error) => {
            warn!(%error, "an open could not be recorded");
            return gif();
        }
    };
    let Some(message) = message.filter(|message| message.status == "sent") else {
        return gif();
    };
    let now = state.clock.now_in_process();
    let agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok());
    let reason = machine::machine_reason(
        agent,
        &state.prefetch_user_agents,
        message.completed_at,
        now,
    );
    let mut properties = Map::new();
    properties.insert("machine".to_owned(), Value::Bool(reason.is_some()));
    properties.insert(
        "machine_reason".to_owned(),
        reason.map_or(Value::Null, Value::from),
    );
    let trace = request_trace();
    let engagement = Engagement {
        name: zirv_flow_core::events::EMAIL_OPENED_EVENT,
        properties,
        now,
        fallback_trace: &trace,
    };
    if let Err(error) = record_engagement(&state.pool, &message, engagement).await {
        warn!(%error, "an open could not be recorded");
    }
    gif()
}

/// `GET /t/c/{token}`. Redirects to the URL recorded for this message and link, and nowhere else.
pub(crate) async fn click(
    State(state): State<AppState>,
    Path(token): Path<String>,
    method: Method,
) -> Response {
    let claims = match admit(&state, TokenKind::Click, &token) {
        Ok(claims) => claims,
        Err(response) => return *response,
    };
    let url = match find_message_link(&state.pool, claims.message, claims.link).await {
        Ok(Some(url)) => url,
        Ok(None) => return pages::not_found(),
        Err(error) => {
            warn!(%error, "a click could not be followed");
            return pages::unavailable();
        }
    };
    let Ok(location) = HeaderValue::from_str(&url) else {
        return pages::not_found();
    };
    if method == Method::GET {
        record_click(&state, &claims).await;
    }
    let mut response = StatusCode::FOUND.into_response();
    let headers = response.headers_mut();
    headers.insert(header::LOCATION, location);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

/// Records `email.clicked` for the link; the redirect does not wait on, or fail with, the write.
async fn record_click(state: &AppState, claims: &Claims) {
    let message = match find_tracked_message(&state.pool, claims.message).await {
        Ok(Some(message)) => message,
        Ok(None) => return,
        Err(error) => {
            warn!(%error, "a click could not be recorded");
            return;
        }
    };
    let mut properties = Map::new();
    properties.insert("link_index".to_owned(), Value::from(claims.link));
    let trace = request_trace();
    let engagement = Engagement {
        name: zirv_flow_core::events::EMAIL_CLICKED_EVENT,
        properties,
        now: state.clock.now_in_process(),
        fallback_trace: &trace,
    };
    if let Err(error) = record_engagement(&state.pool, &message, engagement).await {
        warn!(%error, "a click could not be recorded");
    }
}

/// `GET /u/{token}`: asks for confirmation and changes nothing.
pub(crate) async fn unsubscribe_page(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Response {
    let claims = match admit(&state, TokenKind::Unsubscribe, &token) {
        Ok(claims) => claims,
        Err(response) => return *response,
    };
    match find_tracked_message(&state.pool, claims.message).await {
        Ok(Some(_)) => pages::confirm(&token),
        Ok(None) => pages::not_found(),
        Err(error) => {
            warn!(%error, "an unsubscribe page could not be shown");
            pages::unavailable()
        }
    }
}

/// Whether a form body is the RFC 8058 one-click post.
fn is_one_click(body: &[u8]) -> bool {
    url::form_urlencoded::parse(body)
        .any(|(name, value)| name == "List-Unsubscribe" && value == "One-Click")
}

/// `POST /u/{token}`: withdraws consent for the message's channel, topic and dimension values.
/// The form button and a mail client's one-click post both land here; the client gets an empty
/// 200, a person the page. A repeat changes nothing and answers the same.
pub(crate) async fn unsubscribe(
    State(state): State<AppState>,
    Path(token): Path<String>,
    body: Bytes,
) -> Response {
    let claims = match admit(&state, TokenKind::Unsubscribe, &token) {
        Ok(claims) => claims,
        Err(response) => return *response,
    };
    let message = match find_tracked_message(&state.pool, claims.message).await {
        Ok(Some(message)) => message,
        Ok(None) => return pages::not_found(),
        Err(error) => {
            warn!(%error, "an unsubscribe could not be recorded");
            return pages::unavailable();
        }
    };
    let trace = request_trace();
    match unsubscribe_message(&state.pool, &message, state.clock.now_in_process(), &trace).await {
        Ok(Unsubscribed::Changed | Unsubscribed::Unchanged) => {
            if is_one_click(&body) {
                StatusCode::OK.into_response()
            } else {
                pages::done(&token)
            }
        }
        Ok(Unsubscribed::Unavailable) => pages::not_found(),
        Err(error) => {
            warn!(%error, "an unsubscribe could not be recorded");
            pages::unavailable()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;
    use zirv_flow_core::consent::{Channel, ConsentStatus, Dimensions, TopicPolicy};
    use zirv_flow_core::tracking::sign;
    use zirv_flow_core::{
        HmacKey, HmacPurpose, HmacRoot, MasterKey, Slug, WorkspaceId, WorkspaceName,
    };
    use zirv_flow_store::{
        NewDimension, NewTopic, ReadOutcome, consent_read, create_dimension,
        create_dimension_value, create_topic, create_workspace, record_message_links,
    };

    use super::*;
    use crate::router;
    use crate::test_support::tables_hold_no;

    #[test]
    fn the_pixel_is_a_43_byte_gif() {
        assert_eq!(PIXEL.len(), 43);
        assert_eq!(&PIXEL[..6], b"GIF89a");
        assert_eq!(PIXEL[42], 0x3b);
    }

    #[test]
    fn only_the_rfc_8058_body_is_a_one_click_post() {
        assert!(is_one_click(b"List-Unsubscribe=One-Click"));
        assert!(is_one_click(b"x=1&List-Unsubscribe=One-Click"));
        assert!(!is_one_click(b""));
        assert!(!is_one_click(b"List-Unsubscribe=Other"));
        assert!(!is_one_click(b"list-unsubscribe=one-click"));
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    impl Reply {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }
    }

    struct Fixture {
        pool: PgPool,
        router: Router,
        key: HmacKey,
        ws: WorkspaceId,
        person: Uuid,
        message: Uuid,
    }

    impl Fixture {
        /// A workspace with one person and one message in `status`, completed `age` seconds ago.
        async fn new(pool: PgPool, slug: &str, status: &str, age: i64) -> Self {
            Self::with_agents(pool, slug, status, age, Vec::new()).await
        }

        async fn with_agents(
            pool: PgPool,
            slug: &str,
            status: &str,
            age: i64,
            agents: Vec<String>,
        ) -> Self {
            zirv_flow_store::migrate(&pool).await.unwrap();
            let root = HmacRoot::bootstrap_from(&MasterKey::generate());
            let state = AppState::new(pool.clone(), Arc::new(String::new))
                .with_hmac_root(&root)
                .with_prefetch_user_agents(agents);
            let ws = create_workspace(
                &pool,
                &WorkspaceName::parse(slug).unwrap(),
                &Slug::parse(slug).unwrap(),
            )
            .await
            .unwrap()
            .id;
            let person = Uuid::now_v7();
            sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
                .bind(person)
                .bind(ws.as_uuid())
                .execute(&pool)
                .await
                .unwrap();
            let fixture = Self {
                router: router(state),
                key: root.hmac_key(HmacPurpose::Tracking),
                pool,
                ws,
                person,
                message: Uuid::now_v7(),
            };
            fixture
                .insert_message(fixture.message, status, age, None, "{}")
                .await;
            fixture
        }

        async fn insert_message(
            &self,
            id: Uuid,
            status: &str,
            age: i64,
            topic: Option<Uuid>,
            dimensions: &str,
        ) {
            sqlx::query(
                "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, \
                    template_id, destination_id, status, topic_id, dimensions, completed_at) \
                 VALUES ($1, $2, $3, gen_random_uuid(), 'n', gen_random_uuid(), \
                    gen_random_uuid(), $4, $5, $6::jsonb, \
                    CASE WHEN $4 IN ('pending', 'sending') THEN NULL \
                         ELSE now() - make_interval(secs => $7) END)",
            )
            .bind(id)
            .bind(self.ws.as_uuid())
            .bind(self.person)
            .bind(status)
            .bind(topic)
            .bind(dimensions)
            .bind(age as f64)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        fn token(&self, kind: TokenKind, link: u16) -> String {
            sign(&self.key, kind, self.message, link)
        }

        async fn call(&self, method: &str, uri: &str, agent: Option<&str>, body: &str) -> Reply {
            let mut request = Request::builder().method(method).uri(uri);
            if let Some(agent) = agent {
                request = request.header(header::USER_AGENT, agent);
            }
            let request = request
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body.to_owned()))
                .unwrap();
            let response = self.router.clone().oneshot(request).await.unwrap();
            let (parts, body) = response.into_parts();
            Reply {
                status: parts.status,
                headers: parts.headers,
                body: body.collect().await.unwrap().to_bytes().to_vec(),
            }
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        async fn events(&self, name: &str) -> Vec<Value> {
            sqlx::query_scalar(
                "SELECT properties FROM events WHERE name = $1 ORDER BY occurred_at, id",
            )
            .bind(name)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        async fn allowed(&self, topic: Option<Uuid>, dimensions: &Dimensions) -> bool {
            let read = consent_read(
                &self.pool,
                self.ws,
                self.person,
                Channel::Email,
                topic,
                dimensions,
            )
            .await
            .unwrap();
            matches!(read, ReadOutcome::Read { verdict, .. } if verdict.allowed)
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_bad_token_is_the_404_page_never_the_web_app(pool: PgPool) {
        let fixture = Fixture::new(pool, "notfound", "sent", 3600).await;
        let mut tampered = fixture.token(TokenKind::Unsubscribe, 0);
        tampered.replace_range(10..11, if &tampered[10..11] == "A" { "B" } else { "A" });
        let open = fixture.token(TokenKind::Open, 0);

        for (method, uri) in [
            ("GET", "/t/c/x".to_owned()),
            ("GET", "/t/o/x".to_owned()),
            ("GET", "/u/x".to_owned()),
            ("POST", "/u/x".to_owned()),
            ("GET", "/u/x/preferences".to_owned()),
            ("POST", "/u/x/preferences".to_owned()),
            ("GET", format!("/u/{tampered}")),
            ("GET", format!("/t/c/{open}")),
            ("GET", format!("/u/{open}")),
        ] {
            let reply = fixture.call(method, &uri, None, "").await;

            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {uri}");
            assert_eq!(
                reply.headers[header::CONTENT_TYPE],
                "text/html; charset=utf-8"
            );
            assert!(reply.text().contains("Link not found"), "{method} {uri}");
            assert!(!reply.text().contains("<script"), "{method} {uri}");
        }
        assert_eq!(fixture.count("SELECT count(*) FROM events").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_get_never_unsubscribes_and_the_form_and_the_one_click_post_do_once(pool: PgPool) {
        let fixture = Fixture::new(pool, "unsub", "sent", 3600).await;
        let uri = format!("/u/{}", fixture.token(TokenKind::Unsubscribe, 0));
        let consent = "SELECT count(*) FROM consent";

        let page = fixture.call("GET", &uri, Some("scanner/1"), "").await;
        assert_eq!(page.status, StatusCode::OK);
        assert!(page.text().contains("method=\"post\""), "{}", page.text());
        assert_eq!(fixture.count(consent).await, 0, "a GET changes nothing");
        assert_eq!(fixture.count("SELECT count(*) FROM events").await, 0);

        let form = fixture.call("POST", &uri, None, "").await;
        assert_eq!(form.status, StatusCode::OK);
        assert!(form.text().contains("You are unsubscribed"));
        assert!(!fixture.allowed(None, &Dimensions::new()).await);
        assert_eq!(fixture.events("email.unsubscribed").await.len(), 1);

        let one_click = fixture
            .call("POST", &uri, None, "List-Unsubscribe=One-Click")
            .await;
        assert_eq!(one_click.status, StatusCode::OK);
        assert!(one_click.body.is_empty(), "a one-click post gets no body");
        assert_eq!(
            fixture.events("email.unsubscribed").await.len(),
            1,
            "a repeat writes no second event"
        );
        let sources: Vec<String> = sqlx::query_scalar("SELECT source FROM consent_changes")
            .fetch_all(&fixture.pool)
            .await
            .unwrap();
        assert_eq!(sources, ["unsubscribe_link"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_one_click_post_alone_unsubscribes(pool: PgPool) {
        let fixture = Fixture::new(pool, "oneclick", "sent", 3600).await;
        let uri = format!("/u/{}", fixture.token(TokenKind::Unsubscribe, 0));

        let reply = fixture
            .call("POST", &uri, None, "List-Unsubscribe=One-Click")
            .await;

        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.body.is_empty());
        assert!(!fixture.allowed(None, &Dimensions::new()).await);
        assert_eq!(fixture.events("email.unsubscribed").await.len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_open_is_a_machine_open_soon_after_delivery_or_from_a_listed_agent(pool: PgPool) {
        let fixture = Fixture::with_agents(
            pool.clone(),
            "opens",
            "sent",
            3600,
            vec!["Listed-Fetcher".to_owned()],
        )
        .await;
        let uri = format!("/t/o/{}", fixture.token(TokenKind::Open, 0));
        let agent = "Listed-Fetcher/9.9 (unique-agent-marker)";

        let human = fixture.call("GET", &uri, Some("Mozilla/5.0"), "").await;
        assert_eq!(human.status, StatusCode::OK);
        assert_eq!(human.body.len(), 43);
        assert_eq!(human.headers[header::CONTENT_TYPE], "image/gif");
        assert!(
            human.headers[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .contains("no-store")
        );
        fixture.call("GET", &uri, Some(agent), "").await;
        fixture.call("GET", &uri, None, "").await;
        fixture.call("HEAD", &uri, Some(agent), "").await;
        sqlx::query("UPDATE messages SET completed_at = now() - interval '2 seconds'")
            .execute(&pool)
            .await
            .unwrap();
        fixture.call("GET", &uri, Some("Mozilla/5.0"), "").await;

        let opens = fixture.events("email.opened").await;
        let verdicts: Vec<(bool, Value)> = opens
            .iter()
            .map(|open| {
                (
                    open["machine"].as_bool().unwrap(),
                    open["machine_reason"].clone(),
                )
            })
            .collect();
        assert_eq!(
            verdicts,
            [
                (false, Value::Null),
                (true, Value::from("user_agent")),
                (false, Value::Null),
                (true, Value::from("delivery_delay")),
            ],
            "a HEAD is not an open"
        );
        tables_hold_no(
            &pool,
            &["unique-agent-marker", "Listed-Fetcher", "127.0.0.1"],
        )
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_open_of_a_message_that_did_not_go_out_returns_the_pixel_and_records_nothing(
        pool: PgPool,
    ) {
        let fixture = Fixture::new(pool, "unsent", "pending", 0).await;

        let reply = fixture
            .call(
                "GET",
                &format!("/t/o/{}", fixture.token(TokenKind::Open, 0)),
                None,
                "",
            )
            .await;

        assert_eq!((reply.status, reply.body.len()), (StatusCode::OK, 43));
        assert_eq!(fixture.count("SELECT count(*) FROM events").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_click_redirects_to_the_recorded_link_only_and_records_its_index(pool: PgPool) {
        let fixture = Fixture::new(pool, "clicks", "sent", 3600).await;
        record_message_links(
            &fixture.pool,
            fixture.ws,
            fixture.message,
            &[
                (0, "https://shop.example.test/a?x=1".to_owned()),
                (1, "https://shop.example.test/b".to_owned()),
            ],
        )
        .await
        .unwrap();
        let other = Uuid::now_v7();
        fixture
            .insert_message(other, "sent", 3600, None, "{}")
            .await;
        let click = |link: u16| format!("/t/c/{}", fixture.token(TokenKind::Click, link));

        let first = fixture.call("GET", &click(1), None, "").await;
        assert_eq!(first.status, StatusCode::FOUND);
        assert_eq!(
            first.headers[header::LOCATION],
            "https://shop.example.test/b"
        );

        let unrecorded = fixture.call("GET", &click(7), None, "").await;
        assert_eq!(unrecorded.status, StatusCode::NOT_FOUND);
        assert!(unrecorded.headers.get(header::LOCATION).is_none());

        let for_other = sign(&fixture.key, TokenKind::Click, other, 0);
        let foreign = fixture
            .call("GET", &format!("/t/c/{for_other}"), None, "")
            .await;
        assert_eq!(
            foreign.status,
            StatusCode::NOT_FOUND,
            "the other message has no link 0"
        );

        fixture.call("HEAD", &click(0), None, "").await;
        let clicks = fixture.events("email.clicked").await;
        assert_eq!(clicks.len(), 1, "a HEAD is not a click");
        assert_eq!(clicks[0]["link_index"], 1);
        assert!(!clicks[0].to_string().contains("shop.example.test"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_click_on_a_message_whose_person_was_merged_away_still_redirects_and_writes_no_event(
        pool: PgPool,
    ) {
        let fixture = Fixture::new(pool, "merged", "sent", 3600).await;
        record_message_links(
            &fixture.pool,
            fixture.ws,
            fixture.message,
            &[(0, "https://shop.example.test/a".to_owned())],
        )
        .await
        .unwrap();
        let survivor = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(survivor)
            .bind(fixture.ws.as_uuid())
            .execute(&fixture.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE people SET merged_into = $1 WHERE id = $2")
            .bind(survivor)
            .bind(fixture.person)
            .execute(&fixture.pool)
            .await
            .unwrap();

        let reply = fixture
            .call(
                "GET",
                &format!("/t/c/{}", fixture.token(TokenKind::Click, 0)),
                None,
                "",
            )
            .await;

        assert_eq!(reply.status, StatusCode::FOUND);
        assert_eq!(fixture.count("SELECT count(*) FROM events").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_preference_center_saves_a_vertical_change_that_blocks_only_that_vertical(
        pool: PgPool,
    ) {
        let mut fixture = Fixture::new(pool, "prefs", "sent", 3600).await;
        let topic = create_topic(
            &fixture.pool,
            fixture.ws,
            &NewTopic {
                key: "news".to_owned(),
                name: "News & updates".to_owned(),
                description: String::new(),
                channel: Channel::Email,
                policy: TopicPolicy::OptOut,
            },
        )
        .await
        .unwrap();
        create_dimension(
            &fixture.pool,
            fixture.ws,
            &NewDimension {
                key: "vertical".to_owned(),
                name: "Vertical".to_owned(),
            },
        )
        .await
        .unwrap();
        for (key, name) in [("b2b", "Business"), ("b2c", "Consumer")] {
            create_dimension_value(&fixture.pool, fixture.ws, "vertical", key, name)
                .await
                .unwrap();
        }
        fixture.message = Uuid::now_v7();
        fixture
            .insert_message(
                fixture.message,
                "sent",
                3600,
                Some(topic.id),
                r#"{"vertical":"b2b"}"#,
            )
            .await;
        let uri = format!(
            "/u/{}/preferences",
            fixture.token(TokenKind::Unsubscribe, 0)
        );
        let scope = |value: &str| Dimensions::from([("vertical".to_owned(), value.to_owned())]);

        let page = fixture.call("GET", &uri, None, "").await;
        assert_eq!(page.status, StatusCode::OK);
        let html = page.text();
        assert!(html.contains("News &amp; updates"), "{html}");
        assert!(
            html.contains("name=\"dim\" value=\"vertical/b2b\" checked"),
            "{html}"
        );
        assert!(
            html.contains("name=\"dim\" value=\"vertical/b2c\" checked"),
            "{html}"
        );

        let saved = fixture
            .call("POST", &uri, None, "topic=news&dim=vertical%2Fb2c")
            .await;

        assert_eq!(saved.status, StatusCode::OK);
        assert!(saved.text().contains("Your preferences are saved."));
        assert!(!saved.text().contains("value=\"vertical/b2b\" checked"));
        assert!(!fixture.allowed(Some(topic.id), &scope("b2b")).await);
        assert!(fixture.allowed(Some(topic.id), &scope("b2c")).await);
        assert!(fixture.allowed(Some(topic.id), &Dimensions::new()).await);
        let changes: Vec<(String, String)> =
            sqlx::query_as("SELECT source, to_status FROM consent_changes")
                .fetch_all(&fixture.pool)
                .await
                .unwrap();
        assert_eq!(
            changes,
            [(
                "unsubscribe_link".to_owned(),
                ConsentStatus::Unsubscribed.as_str().to_owned()
            )],
            "only the changed choice is recorded"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_message_is_rate_limited_after_a_burst(pool: PgPool) {
        let fixture = Fixture::new(pool, "limited", "sent", 3600).await;
        let uri = format!("/t/o/{}", fixture.token(TokenKind::Open, 0));
        let mut last = StatusCode::OK;

        for _ in 0..(BURST_PER_MESSAGE + 5) {
            last = fixture.call("HEAD", &uri, None, "").await.status;
        }

        assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    }
}
