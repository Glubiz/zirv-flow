//! The live event debugger (architecture section 12.2): a server-sent-events stream of what
//! the event path decides, as it decides it.
//!
//! The store writes a feed row per outcome only while some `serve` process keeps the
//! workspace watched. Here, one shared `LISTEN` connection per process hears a batch commit
//! and wakes the viewers of that workspace; a heartbeat every 10 seconds keeps the watch
//! alive for 30. Each viewer reads the feed after its own cursor with its own filters,
//! masks each row for its own viewer, and sends it through a bounded channel: when the
//! channel is full the viewer skips ahead and says so with an `event: gap` frame, so a slow
//! viewer never blocks ingestion or another viewer.
//!
//! The feed holds redacted values only; `reveal` shows them as stored, never as received.
//! Nothing here logs a name, an identifier or a value.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use serde::Serialize;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use tokio::sync::{mpsc, watch};
use tracing::warn;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::events::{mask_event, mask_sample};
use zirv_flow_core::{TraceId, WorkspaceId};
use zirv_flow_store::{
    FeedFilter, FeedItem, PgPool, debugger_feed_head, listen_for_debugger, read_debugger_feed,
    touch_debugger_watch,
};

use crate::AppState;
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::consent::bool_param;
use crate::problem::{ApiError, Problem};

/// Frames a viewer may have queued before it is skipped ahead.
const CHANNEL_FRAMES: usize = 64;

/// Feed rows read per query.
const BATCH: i64 = 200;

/// A viewer that hears nothing still polls this often, as a safety net for a missed
/// notification.
const POLL: Duration = Duration::from_secs(5);

/// How often the watch is renewed for workspaces with a viewer.
const HEARTBEAT: Duration = Duration::from_secs(10);

/// How often an idle stream sends a comment, so proxies keep the connection.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

const OUTCOMES: [&str; 5] = ["accepted", "flagged", "unplanned", "rejected", "duplicate"];

/// The process-wide link between feed notifications and the viewers waiting for them.
#[derive(Clone)]
pub(crate) struct DebuggerHub {
    inner: Arc<HubInner>,
}

struct HubInner {
    pool: PgPool,
    slots: Mutex<HashMap<WorkspaceId, Slot>>,
    started: AtomicBool,
}

struct Slot {
    viewers: usize,
    tick: watch::Sender<u64>,
}

/// A viewer's place in the hub; dropping it leaves.
pub(crate) struct Subscription {
    hub: Arc<HubInner>,
    workspace: WorkspaceId,
    ticks: watch::Receiver<u64>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut slots = self
            .hub
            .slots
            .lock()
            .expect("the hub lock is never poisoned");
        if let Some(slot) = slots.get_mut(&self.workspace) {
            slot.viewers = slot.viewers.saturating_sub(1);
            if slot.viewers == 0 {
                slots.remove(&self.workspace);
            }
        }
    }
}

impl DebuggerHub {
    pub(crate) fn new(pool: PgPool) -> Self {
        Self {
            inner: Arc::new(HubInner {
                pool,
                slots: Mutex::new(HashMap::new()),
                started: AtomicBool::new(false),
            }),
        }
    }

    /// Joins the viewers of `workspace`, starting the shared listener on the first call.
    pub(crate) fn subscribe(&self, workspace: WorkspaceId) -> Subscription {
        if !self.inner.started.swap(true, Ordering::SeqCst) {
            tokio::spawn(run(Arc::downgrade(&self.inner)));
        }
        let mut slots = self
            .inner
            .slots
            .lock()
            .expect("the hub lock is never poisoned");
        let slot = slots.entry(workspace).or_insert_with(|| Slot {
            viewers: 0,
            tick: watch::channel(0).0,
        });
        slot.viewers += 1;
        Subscription {
            hub: Arc::clone(&self.inner),
            workspace,
            ticks: slot.tick.subscribe(),
        }
    }

    #[cfg(test)]
    fn viewers(&self, workspace: WorkspaceId) -> usize {
        let slots = self
            .inner
            .slots
            .lock()
            .expect("the hub lock is never poisoned");
        slots.get(&workspace).map_or(0, |slot| slot.viewers)
    }
}

impl HubInner {
    fn wake(&self, workspace: Option<WorkspaceId>) {
        let slots = self.slots.lock().expect("the hub lock is never poisoned");
        for (id, slot) in slots.iter() {
            if workspace.is_none_or(|only| only == *id) {
                slot.tick.send_modify(|ticks| *ticks += 1);
            }
        }
    }

    fn watched(&self) -> Vec<WorkspaceId> {
        let slots = self.slots.lock().expect("the hub lock is never poisoned");
        slots.keys().copied().collect()
    }
}

/// The shared listener: wakes viewers on a notification, renews the watch on a timer, and
/// ends when the hub is gone. A lost connection wakes every viewer to poll.
async fn run(hub: Weak<HubInner>) {
    loop {
        let Some(strong) = hub.upgrade() else {
            return;
        };
        let pool = strong.pool.clone();
        drop(strong);
        let mut listener = match listen_for_debugger(&pool).await {
            Ok(listener) => listener,
            Err(error) => {
                warn!(%error, "the debugger listener could not connect");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let mut beat = tokio::time::interval(HEARTBEAT);
        loop {
            tokio::select! {
                heard = listener.wait() => {
                    let Some(strong) = hub.upgrade() else {
                        return;
                    };
                    match heard {
                        Ok(Some(workspace)) => strong.wake(Some(workspace)),
                        Ok(None) => strong.wake(None),
                        Err(error) => {
                            warn!(%error, "the debugger listener lost its connection");
                            strong.wake(None);
                            break;
                        }
                    }
                }
                _ = beat.tick() => {
                    let Some(strong) = hub.upgrade() else {
                        return;
                    };
                    for workspace in strong.watched() {
                        if let Err(error) = touch_debugger_watch(&pool, workspace).await {
                            warn!(%error, "the debugger watch could not be renewed");
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ReasonItem {
    /// The property that failed; never its value.
    property: String,
    /// `missing_required`, `wrong_type`, `not_in_enum` or `unplanned_property`.
    reason: String,
}

/// The data of an `item` frame.
#[derive(Serialize, ToSchema)]
pub(crate) struct DebuggerItemBody {
    /// The feed position; the frame's `id`, and the `Last-Event-ID` to resume from.
    seq: i64,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    at: OffsetDateTime,
    source: String,
    /// Null when the name itself was invalid.
    name: Option<String>,
    /// `accepted`, `flagged`, `unplanned`, `rejected` or `duplicate`.
    outcome: String,
    reasons: Vec<ReasonItem>,
    /// The reject code of a rejected event, such as `validation_failed`.
    reason: Option<String>,
    /// The stored (redacted) properties. Values of PII-flagged or undeclared properties are
    /// null unless the viewer asked to reveal and may see values; every value of a rejected
    /// event is null for a viewer who does not reveal, except those the schema declares
    /// non-PII with the declared type.
    #[schema(value_type = Object)]
    properties: Map<String, Value>,
    /// The names whose values were replaced by null.
    masked: Vec<String>,
    event_id: Option<Uuid>,
    reject_id: Option<Uuid>,
    /// 32 lowercase hex characters.
    trace_id: String,
}

/// An item as one viewer may see it. `reveal` is true only for a viewer who may see values
/// and asked for them.
fn item_body(item: FeedItem, reveal: bool) -> DebuggerItemBody {
    let masked = if item.outcome == "rejected" && !reveal {
        mask_sample(&item.properties, item.schema.as_deref())
    } else {
        mask_event(&item.properties, item.schema.as_deref(), reveal)
    };
    DebuggerItemBody {
        seq: item.seq,
        at: item.at,
        source: item.source,
        name: item.name,
        outcome: item.outcome,
        reasons: item
            .reasons
            .into_iter()
            .map(|reason| ReasonItem {
                property: reason.property,
                reason: reason.reason.as_str().to_owned(),
            })
            .collect(),
        reason: item.reason,
        properties: masked.properties,
        masked: masked.masked,
        event_id: item.event_id,
        reject_id: item.reject_id,
        trace_id: TraceId::from_uuid(item.trace_id).to_string(),
    }
}

fn item_frame(item: FeedItem, reveal: bool) -> Event {
    let seq = item.seq;
    let body = item_body(item, reveal);
    Event::default()
        .event("item")
        .id(seq.to_string())
        .json_data(&body)
        .unwrap_or_else(|_| {
            Event::default()
                .event("item")
                .id(seq.to_string())
                .data("{}")
        })
}

fn gap_frame(skipped: u64) -> Event {
    Event::default()
        .event("gap")
        .json_data(json!({ "skipped": skipped }))
        .unwrap_or_else(|_| Event::default().event("gap").data("{}"))
}

struct Filter {
    source: Option<String>,
    name: Option<String>,
    outcome: Option<String>,
}

impl Filter {
    fn as_store(&self) -> FeedFilter<'_> {
        FeedFilter {
            source: self.source.as_deref(),
            name: self.name.as_deref(),
            outcome: self.outcome.as_deref(),
        }
    }
}

fn parse_filter(query: &str) -> Result<Filter, ApiError> {
    let mut filter = Filter {
        source: None,
        name: None,
        outcome: None,
    };
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "source" => filter.source = Some(value.into_owned()),
            "name" => filter.name = Some(value.into_owned()),
            "outcome" => {
                if !OUTCOMES.contains(&value.as_ref()) {
                    return Err(ApiError::bad_request(
                        "outcome must be accepted, flagged, unplanned, rejected or duplicate",
                    ));
                }
                filter.outcome = Some(value.into_owned());
            }
            _ => {}
        }
    }
    Ok(filter)
}

/// The `Last-Event-ID` the browser sends when it reconnects, if it is a feed position.
fn resume_from(headers: &HeaderMap) -> Option<i64> {
    headers
        .get("last-event-id")?
        .to_str()
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|seq| *seq >= 0)
}

/// One viewer's reader: sends the rows after its cursor whenever woken, and ends when the
/// client is gone.
async fn pump(
    pool: PgPool,
    workspace: WorkspaceId,
    filter: Filter,
    reveal: bool,
    mut cursor: i64,
    mut subscription: Subscription,
    frames: mpsc::Sender<Event>,
) {
    let mut skipped = 0_u64;
    loop {
        if skipped > 0 {
            match frames.try_send(gap_frame(skipped)) {
                Ok(()) => skipped = 0,
                Err(mpsc::error::TrySendError::Closed(_)) => return,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if let Ok(head) = debugger_feed_head(&pool, workspace).await {
                        skipped += u64::try_from(head - cursor).unwrap_or(0);
                        cursor = cursor.max(head);
                    }
                }
            }
        }
        'read: loop {
            let rows = match read_debugger_feed(&pool, workspace, cursor, filter.as_store(), BATCH)
                .await
            {
                Ok(rows) => rows,
                Err(error) => {
                    warn!(%error, "the debugger feed could not be read");
                    break;
                }
            };
            let count = rows.len();
            for item in rows {
                cursor = item.seq;
                match frames.try_send(item_frame(item, reveal)) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // Skip ahead rather than wait: a slow viewer must not hold anything.
                        skipped += 1;
                        if let Ok(head) = debugger_feed_head(&pool, workspace).await {
                            skipped += u64::try_from(head - cursor).unwrap_or(0);
                            cursor = cursor.max(head);
                        }
                        break 'read;
                    }
                }
            }
            if i64::try_from(count).is_ok_and(|count| count < BATCH) {
                break;
            }
        }
        tokio::select! {
            changed = subscription.ticks.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = tokio::time::sleep(POLL) => {}
            () = frames.closed() => return,
        }
    }
}

/// Watches the workspace's events as they are decided: one `item` frame per outcome
/// (`accepted`, `flagged`, `unplanned`, `rejected` or `duplicate`), live only, with no
/// history. The stream resumes from the `Last-Event-ID` header, the `seq` of the last frame
/// seen, if the feed still holds the rows after it. A viewer that cannot keep up gets an
/// `event: gap` frame with the number of rows skipped (an upper bound when filters apply).
///
/// While at least one viewer watches a workspace, ingestion writes a feed row per event;
/// when none does, it writes nothing extra. Values are masked per viewer as in the person
/// timeline; `reveal=true` shows the stored (redacted) values to a viewer who may see values
/// (owner, workspace admin or `admin` key) and is ignored for anyone else.
#[utoipa::path(
    get,
    operation_id = "stream_event_debugger",
    path = "/api/v1/workspaces/{ws}/debugger/stream",
    tag = "events",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("source" = Option<String>, Query, description = "Only events from this source"),
        ("name" = Option<String>, Query, description = "Only events of this name"),
        ("outcome" = Option<String>, Query, description = "`accepted`, `flagged`, `unplanned`, `rejected` or `duplicate`"),
        ("reveal" = Option<bool>, Query, description = "Show stored values to a viewer who may see values; default false"),
        ("Last-Event-ID" = Option<String>, Header, description = "The `id` of the last frame received")
    ),
    responses(
        (status = 200, description = "A text/event-stream of `item` and `gap` frames", body = DebuggerItemBody, content_type = "text/event-stream"),
        (status = 400, description = "outcome or reveal is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn stream(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let query = query.unwrap_or_default();
    let filter = parse_filter(&query)?;
    let asked = bool_param(&query, "reveal")?;
    let reveal = asked && may_see_values(&state, &principal, workspace.id).await;

    // Watch first, so every event decided from now on is fed; then start at the head.
    let subscription = state.debugger.subscribe(workspace.id);
    touch_debugger_watch(&state.pool, workspace.id).await?;
    let cursor = match resume_from(&headers) {
        Some(cursor) => cursor,
        None => debugger_feed_head(&state.pool, workspace.id).await?,
    };

    let (frames, received) = mpsc::channel::<Event>(CHANNEL_FRAMES);
    tokio::spawn(pump(
        state.pool.clone(),
        workspace.id,
        filter,
        reveal,
        cursor,
        subscription,
        frames,
    ));
    let body = stream::unfold(received, |mut received| async {
        received
            .recv()
            .await
            .map(|event| (Ok::<Event, Infallible>(event), received))
    });
    let mut response = Sse::new(body)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE).text("keep-alive"))
        .into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use serde_json::json;
    use zirv_flow_core::events::{PropertyType, Reason, ReasonCode, SchemaProperty};

    use super::*;

    fn declared(name: &str, pii: bool) -> SchemaProperty {
        SchemaProperty {
            name: name.into(),
            property_type: PropertyType::String,
            required: false,
            enum_values: None,
            description: String::new(),
            pii,
        }
    }

    fn item(outcome: &str, schema: Option<Vec<SchemaProperty>>) -> FeedItem {
        FeedItem {
            seq: 7,
            at: OffsetDateTime::UNIX_EPOCH,
            source: "web".into(),
            name: Some("signup".into()),
            outcome: outcome.into(),
            reasons: vec![Reason {
                property: "plan".into(),
                reason: ReasonCode::WrongType,
            }],
            reason: None,
            properties: json!({"email": "h4sh", "plan": "pro"})
                .as_object()
                .cloned()
                .unwrap(),
            schema,
            event_id: None,
            reject_id: None,
            trace_id: Uuid::nil(),
        }
    }

    #[test]
    fn a_viewer_without_reveal_sees_pii_and_undeclared_values_nulled() {
        let schema = vec![declared("email", true), declared("plan", false)];

        let hidden = item_body(item("accepted", Some(schema.clone())), false);
        let shown = item_body(item("accepted", Some(schema)), true);

        assert_eq!(hidden.properties["email"], Value::Null);
        assert_eq!(hidden.properties["plan"], "pro");
        assert_eq!(hidden.masked, ["email"]);
        assert_eq!(shown.properties["email"], "h4sh");
        assert!(shown.masked.is_empty());
        assert_eq!(hidden.trace_id.len(), 32);
        assert_eq!(hidden.reasons[0].reason, "wrong_type");
    }

    #[test]
    fn a_rejected_event_follows_the_stricter_sample_rule_unless_revealed() {
        let schema = vec![declared("email", true), declared("plan", false)];
        let mut wrong = item("rejected", Some(schema));
        wrong.properties = json!({"plan": {"card": "4111"}, "email": "h4sh"})
            .as_object()
            .cloned()
            .unwrap();

        let hidden = item_body(wrong.clone(), false);
        let shown = item_body(wrong, true);

        assert!(hidden.properties.values().all(Value::is_null));
        assert_eq!(hidden.masked, ["email", "plan"]);
        assert_eq!(shown.properties["plan"], json!({"card": "4111"}));
    }

    #[test]
    fn an_event_without_a_schema_is_fully_masked() {
        let hidden = item_body(item("unplanned", None), false);

        assert!(hidden.properties.values().all(Value::is_null));
    }

    #[test]
    fn the_filter_takes_source_name_and_a_known_outcome() {
        let filter = parse_filter("source=web&name=Order%20Done&outcome=rejected").unwrap();

        assert_eq!(filter.source.as_deref(), Some("web"));
        assert_eq!(filter.name.as_deref(), Some("Order Done"));
        assert_eq!(filter.outcome.as_deref(), Some("rejected"));
        assert!(parse_filter("outcome=lost").is_err());
        assert!(parse_filter("").unwrap().source.is_none());
    }

    #[test]
    fn resuming_reads_a_feed_position_from_last_event_id() {
        let mut headers = HeaderMap::new();
        assert_eq!(resume_from(&headers), None);
        headers.insert("last-event-id", HeaderValue::from_static("42"));
        assert_eq!(resume_from(&headers), Some(42));
        headers.insert("last-event-id", HeaderValue::from_static("abc"));
        assert_eq!(resume_from(&headers), None);
        headers.insert("last-event-id", HeaderValue::from_static("-3"));
        assert_eq!(resume_from(&headers), None);
    }

    #[tokio::test]
    async fn the_hub_counts_viewers_and_forgets_a_workspace_when_the_last_leaves() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap();
        let hub = DebuggerHub::new(pool);
        let ws = WorkspaceId::new();

        let first = hub.subscribe(ws);
        let second = hub.subscribe(ws);
        assert_eq!(hub.viewers(ws), 2);
        drop(first);
        assert_eq!(hub.viewers(ws), 1);
        drop(second);

        assert_eq!(hub.viewers(ws), 0);
        assert!(hub.inner.watched().is_empty());
    }

    #[tokio::test]
    async fn a_wake_reaches_only_the_named_workspace() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap();
        let hub = DebuggerHub::new(pool);
        let (one, two) = (WorkspaceId::new(), WorkspaceId::new());
        let first = hub.subscribe(one);
        let second = hub.subscribe(two);

        hub.inner.wake(Some(one));

        assert!(first.ticks.has_changed().unwrap());
        assert!(!second.ticks.has_changed().unwrap());
        hub.inner.wake(None);
        assert!(second.ticks.has_changed().unwrap());
    }
}
