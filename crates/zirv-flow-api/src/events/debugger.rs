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
//! viewer therefore keeps a watermark (the highest `seq` it has read) and the holes below it:
//! numbers that no row of any workspace held when the watermark passed them, each with the
//! time it was first noticed. A number another workspace holds, or a filter hides, is not a
//! hole. Each read takes the rows above the watermark in batches, plus point lookups of the
//! holes, so a writer that commits after its number was passed is delivered, out of order,
//! once. A hole is dropped when its row is delivered, when it is older than the lookback
//! window ([`LOOKBACK_FACTOR`] times the patience), or when it falls more than [`MAX_BEHIND`]
//! below the watermark, and the set is pruned after every batch, so it never holds more than
//! [`MAX_BEHIND`] numbers. A row that commits after its hole was dropped is not delivered.
//! A viewer without a position starts at the newest row older than the patience and takes the
//! missing numbers in the window below it as holes, so it also gets the rows of writers that
//! were still open when it connected.
//!
//! A frame's `id` is `<epoch>:<position>`, where the position is just below the oldest hole,
//! or the watermark when there is none, so resuming from it never skips a row that was still
//! being waited for (it may repeat rows above it). A resume does not recover a hole: a late
//! row below the resume point whose writer committed while the client was away is not
//! delivered. The epoch names the life of the unlogged feed. A crash empties the feed and starts
//! `seq` over; a position from another epoch, or in the older bare-number format, is answered
//! with a reset `gap` frame, and so is an open stream that notices on its periodic poll that
//! the epoch changed or the feed's head fell below its watermark.
//!
//! The feed holds redacted values only; `reveal` shows them as stored, never as received.
//! Nothing here logs a name, an identifier or a value.

use hashbrown::HashMap;
use std::collections::BTreeMap;
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
    FeedBatch, FeedFilter, FeedItem, PgPool, StoreError, debugger_epoch, debugger_epoch_peek,
    debugger_feed_missing, debugger_feed_position, debugger_feed_start, listen_for_debugger,
    read_debugger_batch, touch_debugger_watch,
};

use crate::AppState;
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::consent::bool_param;
use crate::problem::{ApiError, Problem};

/// Frames a viewer may have queued before it is skipped ahead.
const CHANNEL_FRAMES: usize = 64;

/// Feed rows read per query.
const BATCH: i64 = 200;

/// How long a connection without a position treats a writer as possibly still open, and the
/// base of the lookback window.
const PATIENCE: Duration = Duration::from_secs(10);

/// How long a hole lives, as a multiple of the patience, counted from when the viewer noticed
/// it. A row is written when its writer starts, so a writer that commits up to one patience
/// after its number was passed has usually been open for about as long again; twice the
/// patience catches it, and a longer window would only keep point lookups alive for numbers
/// that were rolled back.
const LOOKBACK_FACTOR: u32 = 2;

/// The most holes a viewer keeps, as the most `seq` numbers a hole may lie below the
/// watermark. It bounds the viewer's memory and the array of the point lookup, whatever the
/// feed's rate, and the numbers one missing-number probe examines.
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

/// One viewer's place in the feed: the highest `seq` it has read, and the holes below it.
/// See the module documentation.
struct Cursor {
    watermark: i64,
    /// Numbers below the watermark that no row held when it passed them, with when each was
    /// first noticed.
    holes: BTreeMap<i64, Instant>,
}

impl Cursor {
    fn new(watermark: i64) -> Self {
        Self {
            watermark,
            holes: BTreeMap::new(),
        }
    }

    /// A cursor at `watermark` whose holes are the `missing` numbers below it, noticed `now`.
    fn seeded(watermark: i64, missing: &[i64], now: Instant) -> Self {
        let mut cursor = Self::new(watermark);
        cursor.holes = missing
            .iter()
            .copied()
            .filter(|seq| *seq <= watermark)
            .map(|seq| (seq, now))
            .collect();
        cursor.prune(now, Duration::MAX);
        cursor
    }

    fn hole_list(&self) -> Vec<i64> {
        self.holes.keys().copied().collect()
    }

    /// The `seq` a frame names: just below the oldest hole, else the watermark.
    fn position(&self) -> i64 {
        self.holes
            .keys()
            .next()
            .map_or(self.watermark, |oldest| oldest - 1)
    }

    /// A row above the watermark was read: the `missing` numbers between the watermark and it
    /// become holes, and the watermark moves to it.
    fn pass(&mut self, seq: i64, missing: &[i64], now: Instant) {
        if seq <= self.watermark {
            return;
        }
        let from = missing.partition_point(|hole| *hole <= self.watermark);
        for hole in missing[from..].iter().take_while(|hole| **hole < seq) {
            self.holes.entry(*hole).or_insert(now);
        }
        self.watermark = seq;
    }

    /// The row of a hole was delivered.
    fn fill(&mut self, seq: i64) {
        self.holes.remove(&seq);
    }

    /// Drops the holes older than `window` and those more than [`MAX_BEHIND`] below the
    /// watermark.
    fn prune(&mut self, now: Instant, window: Duration) {
        let floor = self.watermark - MAX_BEHIND;
        self.holes = self.holes.split_off(&(floor + 1));
        self.holes
            .retain(|_, noticed| now.duration_since(*noticed) < window);
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
    /// The base of the lookback window.
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
/// Every [`POLL`] it also checks that a crash did not reset the feed under it.
async fn pump(mut viewer: Viewer, mut cursor: Cursor, reset: bool) {
    let mut skipped = 0_u64;
    let mut told_degraded = false;
    let mut reset_pending = reset;
    let mut next_poll = tokio::time::Instant::now() + POLL;
    loop {
        if tokio::time::Instant::now() >= next_poll {
            next_poll = tokio::time::Instant::now() + POLL;
            if feed_was_reset(&viewer, cursor.watermark).await
                && let Ok(epoch) = debugger_epoch(&viewer.pool).await
            {
                viewer.epoch = epoch;
                let fresh = debugger_feed_start(&viewer.pool, viewer.patience)
                    .await
                    .unwrap_or(0);
                cursor = seeded_cursor(&viewer, fresh).await;
                skipped = 0;
                reset_pending = true;
            }
        }
        if reset_pending {
            match viewer.frames.try_send(gap_frame(0, true)) {
                Ok(()) => reset_pending = false,
                Err(mpsc::error::TrySendError::Closed(_)) => return,
                Err(mpsc::error::TrySendError::Full(_)) => {}
            }
        }
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
        if !reset_pending && !read_new(&mut viewer, &mut cursor, &mut skipped).await {
            return;
        }
        tokio::select! {
            changed = viewer.subscription.ticks.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = tokio::time::sleep_until(next_poll) => {}
            () = viewer.frames.closed() => return,
            () = stopped(&mut viewer.stop) => return,
        }
    }
}

/// Whether the feed started over since the viewer last knew it: its epoch is gone or
/// replaced, or its head is below the viewer's watermark (a restarted sequence). A read error
/// is not a reset.
async fn feed_was_reset(viewer: &Viewer, watermark: i64) -> bool {
    match debugger_epoch_peek(&viewer.pool).await {
        Ok(Some(epoch)) if epoch == viewer.epoch => {}
        Ok(_) => return true,
        Err(_) => return false,
    }
    debugger_feed_position(&viewer.pool)
        .await
        .is_ok_and(|head| head < watermark)
}

/// How long a hole lives.
fn lookback(viewer: &Viewer) -> Duration {
    viewer.patience * LOOKBACK_FACTOR
}

/// The lowest `seq` a seeding looks at below `watermark`: the newest row older than the
/// lookback age, but never more than [`MAX_BEHIND`] numbers down. On a read error there is
/// none.
async fn lookback_floor(viewer: &Viewer, watermark: i64) -> i64 {
    let aged = debugger_feed_start(&viewer.pool, lookback(viewer))
        .await
        .unwrap_or(watermark);
    aged.max(watermark - MAX_BEHIND).min(watermark)
}

/// A cursor at `watermark` whose holes are the numbers in the lookback window below it that
/// no row holds, so a connection or a skip ahead delivers a writer that commits later and
/// replays nothing already committed.
async fn seeded_cursor(viewer: &Viewer, watermark: i64) -> Cursor {
    let floor = lookback_floor(viewer, watermark).await;
    let missing = debugger_feed_missing(&viewer.pool, floor, watermark, MAX_BEHIND)
        .await
        .unwrap_or_default();
    Cursor::seeded(watermark, &missing, Instant::now())
}

async fn read_batch(viewer: &Viewer, cursor: &Cursor) -> Result<FeedBatch, StoreError> {
    read_debugger_batch(
        &viewer.pool,
        viewer.workspace,
        cursor.watermark,
        &cursor.hole_list(),
        viewer.filter.as_store(),
        BATCH,
        MAX_BEHIND,
    )
    .await
}

/// Sends every row the viewer has not sent: the holes that filled, then the rows above the
/// watermark, in batches. False when the client is gone.
async fn read_new(viewer: &mut Viewer, cursor: &mut Cursor, skipped: &mut u64) -> bool {
    loop {
        cursor.prune(Instant::now(), lookback(viewer));
        let batch = match read_batch(viewer, cursor).await {
            Ok(batch) => batch,
            Err(error) => {
                warn!(%error, "the debugger feed could not be read");
                return true;
            }
        };
        let count = batch.rows.len();
        let now = Instant::now();
        let ordered = batch
            .filled
            .into_iter()
            .map(|item| (true, item))
            .chain(batch.rows.into_iter().map(|item| (false, item)));
        for (was_hole, item) in ordered {
            if was_hole {
                cursor.fill(item.seq);
            } else {
                cursor.pass(item.seq, &batch.missing, now);
            }
            match viewer.frames.try_send(item_frame(
                item,
                viewer.reveal,
                &viewer.epoch,
                cursor.position(),
            )) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => return false,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // Skip ahead rather than wait: a slow viewer must not hold anything.
                    *skipped += 1;
                    if let Ok(head) = debugger_feed_position(&viewer.pool).await {
                        *skipped += u64::try_from(head - cursor.watermark).unwrap_or(0);
                        *cursor = seeded_cursor(viewer, cursor.watermark.max(head)).await;
                    }
                    return true;
                }
            }
        }
        cursor.prune(Instant::now(), lookback(viewer));
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
/// a row that commits more than twice the 10 s patience after a later row was sent can still
/// be missed, and a resumed stream does not deliver a late row below its resume point whose
/// writer committed while the client was away. The epoch names the life of the feed, which is
/// unlogged and starts over after a crash: a position from another epoch, or an older bare
/// number, is answered with a `gap` frame whose `reset` is true, and the stream starts at the
/// current position; an open stream that notices the crash on its poll does the same. A viewer that
/// cannot keep up gets an `event: gap` frame with the number of rows skipped (an upper bound
/// when filters apply). While the server's notifier is down, an `event: status` frame says
/// `degraded` and the stream polls every few seconds instead of reacting at once; another says
/// it recovered.
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

    #[test]
    fn a_row_passing_missing_numbers_leaves_them_as_holes_and_a_late_row_fills_one() {
        let now = Instant::now();
        let mut cursor = Cursor::new(4);
        // B took seq 6 and committed; A (seq 5) is still open.
        cursor.pass(6, &[5], now);
        let while_open = (cursor.watermark, cursor.position(), cursor.hole_list());
        // A commits and is read through its hole.
        cursor.fill(5);

        assert_eq!(
            while_open,
            (6, 4, vec![5]),
            "a resume must not skip the hole"
        );
        assert_eq!((cursor.watermark, cursor.position()), (6, 6));
        assert!(cursor.holes.is_empty(), "delivered once, then forgotten");
    }

    #[test]
    fn only_the_missing_numbers_between_the_watermark_and_the_row_become_holes() {
        let now = Instant::now();
        let mut cursor = Cursor::new(10);

        // 3 is below the watermark and 20 is above the row: neither is this row's to open.
        cursor.pass(15, &[3, 12, 20], now);
        cursor.pass(14, &[13], now);

        assert_eq!(cursor.hole_list(), [12]);
        assert_eq!(
            cursor.watermark, 15,
            "a row below the watermark moves nothing"
        );
    }

    #[test]
    fn a_hole_expires_after_the_window_counted_from_when_it_was_noticed() {
        let now = Instant::now();
        let window = Duration::from_secs(20);
        let mut cursor = Cursor::new(4);
        cursor.pass(10, &[5], now);

        cursor.prune(now + window / 2, window);
        let early = cursor.hole_list();
        cursor.prune(now + window, window);

        assert_eq!(early, [5]);
        assert!(cursor.holes.is_empty());
    }

    #[test]
    fn a_hole_too_far_below_the_watermark_is_dropped() {
        let now = Instant::now();
        let mut cursor = Cursor::new(0);
        cursor.pass(MAX_BEHIND + 100, &[10, 101, 102], now);

        cursor.prune(now, Duration::from_secs(20));

        assert_eq!(cursor.hole_list(), [101, 102]);
    }

    #[test]
    fn a_burst_of_many_batches_keeps_the_hole_set_bounded() {
        let now = Instant::now();
        let window = Duration::from_secs(20);
        let mut cursor = Cursor::new(0);
        let mut most = 0;
        // Fifty batches of BATCH rows, each row leaving two missing numbers before it, at one
        // instant: nothing expires by age, so only the reach below the watermark bounds it.
        for batch in 0..50_i64 {
            let first = batch * BATCH * 3;
            let mut missing = Vec::new();
            for row in 1..=BATCH {
                let seq = first + row * 3;
                missing.extend([seq - 2, seq - 1]);
                cursor.pass(seq, &missing, now);
            }
            cursor.prune(now, window);
            most = most.max(cursor.holes.len());
        }

        let reach = usize::try_from(MAX_BEHIND).unwrap();
        assert!(most <= reach, "{most} holes held");
        assert!(most > reach / 2, "the bound, not the burst, limited it");
        assert!(
            cursor
                .holes
                .keys()
                .all(|hole| *hole > cursor.watermark - MAX_BEHIND)
        );
    }

    #[test]
    fn a_seeded_cursor_holds_the_missing_numbers_up_to_its_start_as_holes() {
        let now = Instant::now();

        let cursor = Cursor::seeded(8, &[3, 7, 9], now);

        assert_eq!(cursor.watermark, 8);
        assert_eq!(cursor.hole_list(), [3, 7], "9 lies above the start");
        assert_eq!(cursor.position(), 2);
    }

    #[test]
    fn a_resume_cursor_has_no_holes_below_its_watermark() {
        let cursor = Cursor::new(42);

        assert!(cursor.hole_list().is_empty());
        assert_eq!(cursor.position(), 42);
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
