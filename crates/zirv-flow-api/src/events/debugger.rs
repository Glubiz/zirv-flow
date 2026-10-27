//! The live event debugger (architecture section 12.2): a server-sent-events stream of what
//! the event path decides, as it decides it.
//!
//! The store writes a feed row per outcome only while some `serve` process keeps the
//! workspace watched. Here, one shared `LISTEN` connection per process hears a batch commit
//! and wakes the viewers of that workspace, and a timer of its own renews the watch every 10
//! seconds for 30, whether or not the listener is up. While the listener is down the viewers
//! fall back to polling and tell the client with an `event: status` frame. Each viewer reads
//! the feed after its own cursor with its own filters, masks each row for its own viewer, and
//! sends it through a bounded channel: when the channel is full the viewer skips ahead and
//! says so with an `event: gap` frame, so a slow viewer never blocks ingestion or another
//! viewer.
//!
//! The feed's `seq` is assigned when a row is inserted, not when its transaction commits, so
//! rows can become visible out of order, and the identity is shared by every workspace. A
//! viewer therefore keeps a low watermark (every `seq` up to it is settled) and the set of
//! rows it sent above it. A number another workspace holds is settled; a number nobody holds
//! is a hole, and the viewer waits a few seconds for it before giving up on it (its writer
//! rolled back). A viewer without a position starts at the newest row older than that wait,
//! so it also gets the rows of writers that were still open when it connected.
//!
//! Giving up on a hole is not final. Each read also looks back over a window below the
//! watermark ([`LOOKBACK_FACTOR`] times the patience in age, at most [`MAX_BEHIND`] numbers)
//! and sends any row there that the viewer has not sent, so a writer that commits after its
//! number was passed is delivered, out of order, instead of lost. The viewer remembers what it
//! sent across that window, so no row is sent twice, and a connection seeds that memory with
//! the rows already committed in the window, so it does not replay them. A row that commits
//! after it left the window can still be missed.
//!
//! A frame's `id` is `<epoch>:<watermark>`: the watermark after it, so resuming from it never
//! skips a row (it may repeat rows above it), and the epoch names the life of the unlogged
//! feed. A crash empties the feed and starts `seq` over; a position from another epoch, or in
//! the older bare-number format, is answered with a reset `gap` frame.
//!
//! The feed holds redacted values only; `reveal` shows them as stored, never as received.
//! Nothing here logs a name, an identifier or a value.

use hashbrown::HashMap;
use std::collections::BTreeSet;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
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
    FeedFilter, FeedItem, PgPool, debugger_epoch, debugger_feed_position, debugger_feed_seqs,
    debugger_feed_start, listen_for_debugger, read_debugger_feed, touch_debugger_watch,
};

use crate::AppState;
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::consent::bool_param;
use crate::problem::{ApiError, Problem};

/// Frames a viewer may have queued before it is skipped ahead.
const CHANNEL_FRAMES: usize = 64;

/// Feed rows read per query.
const BATCH: i64 = 200;

/// How many `seq` numbers a viewer looks at when settling its watermark.
const WINDOW: i64 = 1000;

/// How long a viewer waits for a missing `seq` before it gives up on it.
const PATIENCE: Duration = Duration::from_secs(10);

/// The most rows a viewer holds above its watermark; beyond it, holes are given up at once.
const MAX_AHEAD: usize = 5000;

/// How far below the watermark a viewer keeps looking for late rows, as a multiple of the
/// patience. A hole is given up after one patience, by when its writer may already have been
/// open for about as long, so twice the patience still catches a writer that commits up to one
/// patience after its number was passed. The age is that of the row's `at`, the start of its
/// writer's transaction.
const LOOKBACK_FACTOR: u32 = 2;

/// The most `seq` numbers the lookback reaches below the watermark, and so the most rows a
/// viewer remembers having sent below it. It bounds the memory and the exclusion list of a
/// read when the feed is fast enough that the age window would hold more.
const MAX_BEHIND: i64 = 5000;

/// A viewer that hears nothing still polls this often, as a safety net for a missed
/// notification and the only way it learns of new rows while the listener is down.
const POLL: Duration = Duration::from_secs(5);

/// How often the watch is renewed for workspaces with a viewer.
const HEARTBEAT: Duration = Duration::from_secs(10);

/// How often an idle stream sends a comment, so proxies keep the connection.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// The most streams one workspace may have open in one process.
pub(crate) const MAX_VIEWERS: usize = 20;

/// The most streams one server process may have open, across every workspace.
pub(crate) const MAX_VIEWERS_TOTAL: usize = 500;

/// What a refused viewer is told to wait, in seconds.
const RETRY_AFTER: u64 = 5;

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
    /// The shared listener is not connected: viewers poll instead of being woken.
    degraded: AtomicBool,
    /// [`PATIENCE`] in milliseconds; tests shorten it.
    patience_ms: AtomicU64,
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

impl Subscription {
    /// Whether the shared listener is down, so the viewer only polls.
    fn degraded(&self) -> bool {
        self.hub.degraded.load(Ordering::Relaxed)
    }
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
                degraded: AtomicBool::new(false),
                patience_ms: AtomicU64::new(
                    u64::try_from(PATIENCE.as_millis()).unwrap_or(u64::MAX),
                ),
            }),
        }
    }

    /// Joins the viewers of `workspace`, starting the shared tasks on the first call. `None`
    /// when the workspace already has [`MAX_VIEWERS`] streams in this process, or the process
    /// has [`MAX_VIEWERS_TOTAL`].
    pub(crate) fn subscribe(&self, workspace: WorkspaceId) -> Option<Subscription> {
        if !self.inner.started.swap(true, Ordering::SeqCst) {
            tokio::spawn(listen(Arc::downgrade(&self.inner)));
            tokio::spawn(renew(Arc::downgrade(&self.inner)));
        }
        let mut slots = self
            .inner
            .slots
            .lock()
            .expect("the hub lock is never poisoned");
        if slots.values().map(|slot| slot.viewers).sum::<usize>() >= MAX_VIEWERS_TOTAL {
            return None;
        }
        let slot = slots.entry(workspace).or_insert_with(|| Slot {
            viewers: 0,
            tick: watch::channel(0).0,
        });
        if slot.viewers >= MAX_VIEWERS {
            return None;
        }
        slot.viewers += 1;
        Some(Subscription {
            hub: Arc::clone(&self.inner),
            workspace,
            ticks: slot.tick.subscribe(),
        })
    }

    fn patience(&self) -> Duration {
        Duration::from_millis(self.inner.patience_ms.load(Ordering::Relaxed))
    }

    /// Shortens the patience, and with it the lookback, so a test need not wait ten seconds.
    #[cfg(test)]
    pub(crate) fn set_patience_for_test(&self, patience: Duration) {
        let millis = u64::try_from(patience.as_millis()).unwrap_or(u64::MAX);
        self.inner.patience_ms.store(millis, Ordering::Relaxed);
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

    /// Pretends the shared listener went down or came back, and wakes every viewer.
    #[cfg(test)]
    pub(crate) fn set_degraded_for_test(&self, degraded: bool) {
        self.inner.degraded.store(degraded, Ordering::Relaxed);
        self.inner.wake(None);
    }

    /// Renews the watch of every workspace with a viewer, once, as the timer does.
    #[cfg(test)]
    pub(crate) async fn renew_once_for_test(&self) {
        self.inner.renew_watches().await;
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

    async fn renew_watches(&self) {
        for workspace in self.watched() {
            if let Err(error) = touch_debugger_watch(&self.pool, workspace).await {
                warn!(%error, "the debugger watch could not be renewed");
            }
        }
    }

    fn set_degraded(&self, degraded: bool) {
        if self.degraded.swap(degraded, Ordering::Relaxed) != degraded {
            self.wake(None);
        }
    }
}

/// Keeps the watch alive on a timer of its own, so a failing listener cannot let it expire.
/// Ends when the hub is gone.
async fn renew(hub: Weak<HubInner>) {
    let mut beat = tokio::time::interval(HEARTBEAT);
    loop {
        beat.tick().await;
        let Some(strong) = hub.upgrade() else {
            return;
        };
        strong.renew_watches().await;
    }
}

/// The shared listener: wakes the viewers of a workspace on a notification. While it is not
/// connected the hub is degraded and every viewer is woken so it can say so and poll. Ends
/// when the hub is gone.
async fn listen(hub: Weak<HubInner>) {
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
                if let Some(strong) = hub.upgrade() {
                    strong.set_degraded(true);
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        if let Some(strong) = hub.upgrade() {
            // Anything sent while it was down was missed: wake everyone to catch up.
            strong.set_degraded(false);
            strong.wake(None);
        }
        loop {
            let heard = listener.wait().await;
            let Some(strong) = hub.upgrade() else {
                return;
            };
            match heard {
                Ok(Some(workspace)) => strong.wake(Some(workspace)),
                Ok(None) => strong.wake(None),
                Err(error) => {
                    warn!(%error, "the debugger listener lost its connection");
                    strong.set_degraded(true);
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// One viewer's place in the feed: every `seq` up to `watermark` is settled, and `sent` holds
/// the rows already delivered above it and in the lookback window below it. See the module
/// documentation.
struct Cursor {
    watermark: i64,
    sent: BTreeSet<i64>,
    /// The first missing `seq` above the watermark, and when it was first missed.
    waiting: Option<(i64, Instant)>,
}

impl Cursor {
    fn new(watermark: i64) -> Self {
        Self {
            watermark,
            sent: BTreeSet::new(),
            waiting: None,
        }
    }

    fn sent_list(&self) -> Vec<i64> {
        self.sent.iter().copied().collect()
    }

    fn deliver(&mut self, seq: i64) {
        self.sent.insert(seq);
    }

    /// Forgets the rows at or below `floor`, which the lookback no longer reaches. Rows above
    /// the watermark are kept: they are what stops the viewer sending them twice.
    fn prune(&mut self, floor: i64) {
        let bound = floor.min(self.watermark);
        self.sent = self.sent.split_off(&(bound + 1));
    }

    /// Moves the watermark over every `seq` that is settled. `present` lists, in order, the
    /// numbers the feed holds above the watermark and whether the viewer's filters match
    /// them. A number is settled when the viewer sent it, or the feed holds it and the
    /// filters hide it. A number the feed does not hold is a hole while a later one exists:
    /// the viewer waits `patience` for its writer to commit, then gives up on it.
    fn advance(&mut self, present: &[(i64, bool)], now: Instant, patience: Duration) {
        loop {
            let next = self.watermark + 1;
            let held = present.binary_search_by_key(&next, |(seq, _)| *seq).ok();
            let settled = self.sent.contains(&next) || held.is_some_and(|at| !present[at].1);
            if settled {
                self.watermark = next;
                self.waiting = None;
                continue;
            }
            if held.is_some() {
                // A row to send that the viewer has not read yet.
                return;
            }
            let later = present.iter().any(|(seq, _)| *seq > next)
                || self.sent.iter().any(|seq| *seq > next);
            if !later {
                return;
            }
            let give_up = self.sent.range(next..).count() > MAX_AHEAD
                || self.waiting.is_some_and(|(seq, since)| {
                    seq == next && now.duration_since(since) >= patience
                });
            if give_up {
                // The numbers up to the next one anybody holds are the same age: all go.
                let after = present
                    .iter()
                    .map(|(seq, _)| *seq)
                    .chain(self.sent.iter().copied())
                    .filter(|seq| *seq > next)
                    .min()
                    .unwrap_or(next + 1);
                self.watermark = after - 1;
                self.waiting = None;
                continue;
            }
            if self.waiting.is_none_or(|(seq, _)| seq != next) {
                self.waiting = Some((next, now));
            }
            return;
        }
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

/// The frame of one row. Its `id` is the feed's epoch and the viewer's watermark after it, not
/// the row's `seq`: the position from which resuming skips nothing.
fn item_frame(item: FeedItem, reveal: bool, epoch: &str, watermark: i64) -> Event {
    let body = item_body(item, reveal);
    let id = format!("{epoch}:{watermark}");
    Event::default()
        .event("item")
        .id(id.clone())
        .json_data(&body)
        .unwrap_or_else(|_| Event::default().event("item").id(id).data("{}"))
}

/// `skipped` rows were not sent because the viewer could not keep up; `reset` says the
/// position the client resumed from was beyond the feed, which was cut short (a crash).
fn gap_frame(skipped: u64, reset: bool) -> Event {
    Event::default()
        .event("gap")
        .json_data(json!({ "skipped": skipped, "reset": reset }))
        .unwrap_or_else(|_| Event::default().event("gap").data("{}"))
}

/// Tells the client whether the server's notifier is down, so the stream only polls.
fn status_frame(degraded: bool) -> Event {
    Event::default()
        .event("status")
        .json_data(json!({ "degraded": degraded }))
        .unwrap_or_else(|_| Event::default().event("status").data("{}"))
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

/// What the `Last-Event-ID` a browser sends when it reconnects says about where to start.
#[derive(Debug, PartialEq, Eq)]
enum Resume {
    /// None, or not a feed position at all.
    Absent,
    /// A watermark of the current epoch.
    At(i64),
    /// A position of another epoch, or in the older bare-number format: its feed is gone.
    Stale,
}

fn resume_from(headers: &HeaderMap, epoch: &str) -> Resume {
    let Some(value) = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
    else {
        return Resume::Absent;
    };
    let value = value.trim();
    let position = |text: &str| text.parse::<i64>().ok().filter(|seq| *seq >= 0);
    match value.split_once(':') {
        Some((named, seq)) => match position(seq) {
            Some(seq) if named == epoch => Resume::At(seq),
            Some(_) => Resume::Stale,
            None => Resume::Absent,
        },
        None if position(value).is_some() => Resume::Stale,
        None => Resume::Absent,
    }
}

/// What one viewer needs to read the feed.
struct Viewer {
    pool: PgPool,
    workspace: WorkspaceId,
    filter: Filter,
    reveal: bool,
    /// The feed's epoch, in every frame id.
    epoch: String,
    /// How long to wait for a missing `seq`; the lookback is a multiple of it.
    patience: Duration,
    subscription: Subscription,
    frames: mpsc::Sender<Event>,
    /// Fires when the server is shutting down.
    stop: Option<watch::Receiver<bool>>,
}

/// Resolves when the server is shutting down; never when it is not told about shutdown.
async fn stopped(stop: &mut Option<watch::Receiver<bool>>) {
    match stop {
        Some(stop) => {
            while !*stop.borrow() {
                if stop.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        }
        None => std::future::pending::<()>().await,
    }
}

/// One viewer's reader: sends the rows it has not sent whenever woken, and ends when the
/// client is gone or the server shuts down. `cursor` is where to begin; `reset` says
/// the client's position belonged to another life of the feed, so it is told with a gap frame.
async fn pump(mut viewer: Viewer, mut cursor: Cursor, reset: bool) {
    let mut skipped = 0_u64;
    let mut told_degraded = false;
    if reset && viewer.frames.try_send(gap_frame(0, true)).is_err() {
        return;
    }
    loop {
        let degraded = viewer.subscription.degraded();
        if degraded != told_degraded {
            match viewer.frames.try_send(status_frame(degraded)) {
                Ok(()) => told_degraded = degraded,
                Err(mpsc::error::TrySendError::Closed(_)) => return,
                Err(mpsc::error::TrySendError::Full(_)) => {}
            }
        }
        if skipped > 0 {
            match viewer.frames.try_send(gap_frame(skipped, false)) {
                Ok(()) => skipped = 0,
                Err(mpsc::error::TrySendError::Closed(_)) => return,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if let Ok(head) = debugger_feed_position(&viewer.pool).await {
                        skipped += u64::try_from(head - cursor.watermark).unwrap_or(0);
                        cursor = seeded_cursor(&viewer, cursor.watermark.max(head)).await;
                    }
                }
            }
        }
        if !read_new(&mut viewer, &mut cursor, &mut skipped).await {
            return;
        }
        tokio::select! {
            changed = viewer.subscription.ticks.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = tokio::time::sleep(POLL) => {}
            () = viewer.frames.closed() => return,
            () = stopped(&mut viewer.stop) => return,
        }
    }
}

/// The lowest `seq` the lookback reaches below `watermark`: the newest row older than the
/// lookback age, but never more than [`MAX_BEHIND`] numbers down. On a read error there is no
/// lookback.
async fn lookback_floor(viewer: &Viewer, watermark: i64) -> i64 {
    let lookback = viewer.patience * LOOKBACK_FACTOR;
    let aged = debugger_feed_start(&viewer.pool, lookback)
        .await
        .unwrap_or(watermark);
    aged.max(watermark - MAX_BEHIND).min(watermark)
}

/// A cursor at `watermark` that already counts as sent every row committed in the lookback
/// window up to it, so a connection or a skip ahead does not replay them; only a row that
/// commits later is delivered below the watermark.
async fn seeded_cursor(viewer: &Viewer, watermark: i64) -> Cursor {
    let mut cursor = Cursor::new(watermark);
    let floor = lookback_floor(viewer, watermark).await;
    let committed = debugger_feed_seqs(
        &viewer.pool,
        viewer.workspace,
        floor,
        FeedFilter::default(),
        MAX_BEHIND,
    )
    .await
    .unwrap_or_default();
    for (seq, _) in committed.into_iter().filter(|(seq, _)| *seq <= watermark) {
        cursor.deliver(seq);
    }
    cursor
}

/// Sends every row the viewer has not sent, including late ones in the lookback window below
/// the watermark, then settles the watermark. False when the client is gone.
async fn read_new(viewer: &mut Viewer, cursor: &mut Cursor, skipped: &mut u64) -> bool {
    let workspace = viewer.workspace;
    let floor = lookback_floor(viewer, cursor.watermark).await;
    cursor.prune(floor);
    let pool = &viewer.pool;
    loop {
        let rows = match read_debugger_feed(
            pool,
            workspace,
            floor,
            &cursor.sent_list(),
            viewer.filter.as_store(),
            BATCH,
        )
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                warn!(%error, "the debugger feed could not be read");
                return true;
            }
        };
        // Read after the rows, so it holds every number they do.
        let present = debugger_feed_seqs(
            pool,
            workspace,
            cursor.watermark,
            viewer.filter.as_store(),
            WINDOW,
        )
        .await
        .unwrap_or_default();
        let count = rows.len();
        for item in rows {
            cursor.deliver(item.seq);
            cursor.advance(&present, Instant::now(), viewer.patience);
            match viewer.frames.try_send(item_frame(
                item,
                viewer.reveal,
                &viewer.epoch,
                cursor.watermark,
            )) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => return false,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // Skip ahead rather than wait: a slow viewer must not hold anything.
                    *skipped += 1;
                    if let Ok(head) = debugger_feed_position(pool).await {
                        *skipped += u64::try_from(head - cursor.watermark).unwrap_or(0);
                        *cursor = seeded_cursor(viewer, cursor.watermark.max(head)).await;
                    }
                    return true;
                }
            }
        }
        cursor.advance(&present, Instant::now(), viewer.patience);
        if i64::try_from(count).is_ok_and(|count| count < BATCH) {
            return true;
        }
    }
}

/// Watches the workspace's events as they are decided: one `item` frame per outcome
/// (`accepted`, `flagged`, `unplanned`, `rejected` or `duplicate`), live, with no
/// history beyond the last few seconds before it connected (so a batch that was still
/// committing is not missed). Each frame's `id` is `<epoch>:<position>`, a position that is
/// safe to resume from: send it back as `Last-Event-ID` and the stream continues after it,
/// possibly repeating a few rows the client already has (their `seq` identifies them). A row
/// whose writer commits late is sent when it does, so frames may arrive out of order by `seq`;
/// a row that commits more than twice the 10 s patience after it was written can still be
/// missed. The epoch names the life of the feed, which is unlogged and starts over after a
/// crash: a position from another epoch, or an older bare number, is answered with a `gap`
/// frame whose `reset` is true, and the stream starts at the current position. A viewer that cannot keep up gets an `event: gap` frame with
/// the number of rows skipped (an upper bound when filters apply). While the server's
/// notifier is down, an `event: status` frame says `degraded` and the stream polls every few
/// seconds instead of reacting at once; another says it recovered.
///
/// While at least one viewer watches a workspace, ingestion writes a feed row per event;
/// when none does, it writes nothing extra. Values are masked per viewer as in the person
/// timeline; `reveal=true` shows the stored (redacted) values to a viewer who may see values
/// (owner, workspace admin or `admin` key) and is ignored for anyone else. A workspace has at
/// most 20 streams per server process; more get 429 with `Retry-After`. Streams end when the
/// server shuts down.
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
        (status = 200, description = "A text/event-stream of `item`, `gap` and `status` frames", body = DebuggerItemBody, content_type = "text/event-stream"),
        (status = 400, description = "outcome or reveal is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "The workspace already has 20 streams open on this server; see `Retry-After`", body = Problem, content_type = "application/problem+json")
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

    let subscription = state.debugger.subscribe(workspace.id).ok_or_else(|| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "this workspace already has the most debugger streams open on this server",
        )
        .with_retry_after(RETRY_AFTER)
    })?;
    // Watch first, so every event decided from now on is fed; then start at the head.
    touch_debugger_watch(&state.pool, workspace.id).await?;
    let position = debugger_feed_position(&state.pool).await?;
    // Without a position of its own, start where rows of writers still open are not yet lost.
    let patience = state.debugger.patience();
    let fresh = debugger_feed_start(&state.pool, patience).await?;
    let epoch = debugger_epoch(&state.pool).await?;
    let (start, reset) = match resume_from(&headers, &epoch) {
        Resume::At(resumed) if resumed <= position => (resumed, false),
        Resume::At(_) | Resume::Stale => (fresh, true),
        Resume::Absent => (fresh, false),
    };

    let (frames, received) = mpsc::channel::<Event>(CHANNEL_FRAMES);
    let viewer = Viewer {
        pool: state.pool.clone(),
        workspace: workspace.id,
        filter,
        reveal,
        epoch,
        patience,
        subscription,
        frames,
        stop: state.stop.clone(),
    };
    // Seeded here, before the stream is returned, so a row that commits after this call is a
    // late row to deliver and not history to skip.
    let cursor = seeded_cursor(&viewer, start).await;
    tokio::spawn(pump(viewer, cursor, reset));
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
        assert_eq!(resume_from(&headers, "e1"), Resume::Absent);
        headers.insert("last-event-id", HeaderValue::from_static("e1:42"));
        assert_eq!(resume_from(&headers, "e1"), Resume::At(42));
        headers.insert("last-event-id", HeaderValue::from_static("e0:42"));
        assert_eq!(resume_from(&headers, "e1"), Resume::Stale);
        headers.insert("last-event-id", HeaderValue::from_static("42"));
        assert_eq!(resume_from(&headers, "e1"), Resume::Stale);
        headers.insert("last-event-id", HeaderValue::from_static("abc"));
        assert_eq!(resume_from(&headers, "e1"), Resume::Absent);
        headers.insert("last-event-id", HeaderValue::from_static("-3"));
        assert_eq!(resume_from(&headers, "e1"), Resume::Absent);
    }

    #[tokio::test]
    async fn the_hub_counts_viewers_and_forgets_a_workspace_when_the_last_leaves() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap();
        let hub = DebuggerHub::new(pool);
        let ws = WorkspaceId::new();

        let first = hub.subscribe(ws).unwrap();
        let second = hub.subscribe(ws).unwrap();
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
        let first = hub.subscribe(one).unwrap();
        let second = hub.subscribe(two).unwrap();

        hub.inner.wake(Some(one));

        assert!(first.ticks.has_changed().unwrap());
        assert!(!second.ticks.has_changed().unwrap());
        hub.inner.wake(None);
        assert!(second.ticks.has_changed().unwrap());
    }

    fn present(seqs: &[i64]) -> Vec<(i64, bool)> {
        seqs.iter().map(|seq| (*seq, true)).collect()
    }

    #[test]
    fn a_row_that_commits_late_below_the_cursor_is_still_delivered() {
        let now = Instant::now();
        let mut cursor = Cursor::new(4);
        // B took seq 6 and committed; A (seq 5) is still open.
        cursor.deliver(6);
        cursor.advance(&present(&[6]), now, PATIENCE);
        let held_back = cursor.watermark;
        let reads_while_open = cursor.sent_list();
        // A commits.
        cursor.deliver(5);
        cursor.advance(&present(&[5, 6]), now, PATIENCE);

        assert_eq!(held_back, 4, "the watermark waits for the hole");
        assert_eq!(
            reads_while_open,
            [6],
            "the next read excludes what was sent"
        );
        assert_eq!(cursor.watermark, 6);
        assert_eq!(
            cursor.sent_list(),
            [5, 6],
            "kept for the lookback, pruned later"
        );
    }

    #[test]
    fn a_row_below_the_watermark_stays_sent_until_the_lookback_leaves_it() {
        let now = Instant::now();
        let mut cursor = Cursor::new(0);
        cursor.deliver(1);
        cursor.deliver(2);
        cursor.advance(&present(&[1, 2]), now, PATIENCE);

        let kept = cursor.sent_list();
        cursor.prune(1);
        let after_one = cursor.sent_list();
        cursor.prune(50);

        assert_eq!(cursor.watermark, 2);
        assert_eq!(kept, [1, 2], "a late re-read excludes what was sent");
        assert_eq!(after_one, [2]);
        assert!(
            cursor.sent.is_empty(),
            "pruning never goes above the watermark"
        );
    }

    #[test]
    fn pruning_keeps_rows_above_the_watermark() {
        let now = Instant::now();
        let mut cursor = Cursor::new(4);
        cursor.deliver(6);
        cursor.advance(&present(&[6]), now, PATIENCE);

        cursor.prune(100);

        assert_eq!(cursor.watermark, 4);
        assert_eq!(cursor.sent_list(), [6]);
    }

    #[test]
    fn a_hole_is_given_up_after_the_patience_and_not_before() {
        let now = Instant::now();
        let mut cursor = Cursor::new(4);
        cursor.deliver(6);
        cursor.advance(&present(&[6]), now, PATIENCE);

        cursor.advance(&present(&[6]), now + PATIENCE / 2, PATIENCE);
        let early = cursor.watermark;
        cursor.advance(&present(&[6]), now + PATIENCE, PATIENCE);

        assert_eq!(early, 4);
        assert_eq!(cursor.watermark, 6, "the writer of seq 5 rolled back");
    }

    #[test]
    fn rows_a_filter_hides_settle_without_waiting_and_a_tail_is_not_a_hole() {
        let now = Instant::now();
        let mut cursor = Cursor::new(0);
        cursor.deliver(3);

        cursor.advance(&[(1, false), (2, false), (3, true)], now, PATIENCE);
        let settled = cursor.watermark;
        cursor.advance(&[], now + PATIENCE * 3, PATIENCE);

        assert_eq!(settled, 3);
        assert_eq!(cursor.watermark, 3);
        assert!(cursor.waiting.is_none());
    }

    #[test]
    fn a_row_to_send_blocks_the_watermark_until_it_is_sent() {
        let now = Instant::now();
        let mut cursor = Cursor::new(0);

        cursor.advance(&present(&[1, 2]), now, PATIENCE);

        assert_eq!(cursor.watermark, 0);
    }

    #[test]
    fn a_frame_id_is_the_epoch_and_watermark_so_a_resume_never_skips_a_row() {
        let frame = item_frame(item("accepted", None), false, "e1", 4);

        let text = format!("{frame:?}");

        assert!(text.contains("e1:4"), "{text}");
    }

    #[tokio::test]
    async fn the_most_streams_per_workspace_is_enforced_and_a_leaver_frees_a_place() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap();
        let hub = DebuggerHub::new(pool);
        let (ws, other) = (WorkspaceId::new(), WorkspaceId::new());
        let mut held: Vec<Subscription> = (0..MAX_VIEWERS)
            .map(|_| hub.subscribe(ws).unwrap())
            .collect();

        let refused = hub.subscribe(ws);
        let elsewhere = hub.subscribe(other);
        held.pop();
        let after_leaving = hub.subscribe(ws);

        assert!(refused.is_none());
        assert!(elsewhere.is_some());
        assert!(after_leaving.is_some());
    }

    #[tokio::test]
    async fn a_hub_reports_degraded_until_told_otherwise_and_wakes_viewers_on_a_change() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap();
        let hub = DebuggerHub::new(pool);
        let mut viewer = hub.subscribe(WorkspaceId::new()).unwrap();
        assert!(!viewer.degraded());

        hub.inner.set_degraded(true);

        assert!(viewer.degraded());
        assert!(viewer.ticks.has_changed().unwrap());
        viewer.ticks.mark_unchanged();
        hub.inner.set_degraded(true);
        assert!(!viewer.ticks.has_changed().unwrap(), "no change, no wake");
    }

    #[tokio::test]
    async fn a_shutdown_signal_resolves_stopped_and_no_signal_never_does() {
        let (stop, rx) = watch::channel(false);
        let mut with_signal = Some(rx);
        let mut without = None;

        let before =
            tokio::time::timeout(Duration::from_millis(50), stopped(&mut with_signal)).await;
        stop.send(true).unwrap();
        let after = tokio::time::timeout(Duration::from_secs(1), stopped(&mut with_signal)).await;
        let never = tokio::time::timeout(Duration::from_millis(50), stopped(&mut without)).await;

        assert!(before.is_err());
        assert!(after.is_ok());
        assert!(never.is_err());
    }

    #[test]
    fn numbers_another_workspace_holds_are_settled_not_holes() {
        let now = Instant::now();
        let mut cursor = Cursor::new(0);
        cursor.deliver(4);
        // Seqs 1 to 3 and 5 belong to other workspaces; 4 is this viewer's, already sent.
        let global = [(1, false), (2, false), (3, false), (4, true), (5, false)];

        cursor.advance(&global, now, PATIENCE);

        assert_eq!(cursor.watermark, 5);
        assert!(
            cursor.waiting.is_none(),
            "no wait for other workspaces' numbers"
        );
    }

    #[test]
    fn giving_up_on_a_hole_skips_the_whole_run_of_missing_numbers() {
        let now = Instant::now();
        let mut cursor = Cursor::new(0);
        cursor.deliver(9);
        let global = [(9, true)];
        cursor.advance(&global, now, PATIENCE);
        let waiting = cursor.watermark;

        cursor.advance(&global, now + PATIENCE, PATIENCE);

        assert_eq!(waiting, 0);
        assert_eq!(
            cursor.watermark, 9,
            "one wait covers numbers 1 to 8, not one wait each"
        );
    }

    #[tokio::test]
    async fn the_process_wide_cap_counts_every_workspace_and_a_leaver_frees_a_place() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap();
        let hub = DebuggerHub::new(pool);
        let mut held: Vec<Subscription> = Vec::new();
        for _ in 0..MAX_VIEWERS_TOTAL / MAX_VIEWERS {
            let workspace = WorkspaceId::new();
            for _ in 0..MAX_VIEWERS {
                held.push(hub.subscribe(workspace).unwrap());
            }
        }

        let refused = hub.subscribe(WorkspaceId::new());
        held.pop();
        let after_leaving = hub.subscribe(WorkspaceId::new());

        assert_eq!(held.len(), MAX_VIEWERS_TOTAL - 1);
        assert!(refused.is_none());
        assert!(after_leaving.is_some());
    }
}
