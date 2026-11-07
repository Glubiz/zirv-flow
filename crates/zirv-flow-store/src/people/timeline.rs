//! The person timeline (architecture section 12.3): the person's events and the decisions the
//! system made about the person, newest first, in one keyset-paged stream.
//!
//! Two streams are merged: events by `occurred_at`, and decision records whose
//! `subject_ids.person` is the person or a person merged into it, by `recorded_at`. The
//! cursor is `(at, kind, id)`; at equal times a decision sorts before an event, then ids
//! descend. Each entry carries the trace it belongs to.

use std::fmt;

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::events::SchemaProperty;

use crate::StoreError;
use crate::bounded::begin_bounded;

/// The action of the decision that duplicates an event entry. Versions before 0.231.0 wrote one
/// per accepted event; later ones write none, so only older rows match.
const EVENT_ACCEPTED: &str = "event.accepted";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineKind {
    Event,
    Decision,
}

impl TimelineKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Event => "event",
            Self::Decision => "decision",
        }
    }

    /// Sorts at equal times: a decision comes before an event in a newest-first stream.
    const fn rank(&self) -> i16 {
        match self {
            Self::Event => 0,
            Self::Decision => 1,
        }
    }
}

impl std::str::FromStr for TimelineKind {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, ()> {
        match text {
            "event" => Ok(Self::Event),
            "decision" => Ok(Self::Decision),
            _ => Err(()),
        }
    }
}

/// Where a page ended: the next page starts after this entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimelineCursor {
    pub at: OffsetDateTime,
    pub kind: TimelineKind,
    pub id: Uuid,
}

/// Which entries to read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimelineFilter {
    pub events: bool,
    pub decisions: bool,
    /// Include `event.accepted`, which duplicates the event entry. Only events accepted by
    /// versions before 0.231.0 have one.
    pub all_decisions: bool,
}

impl Default for TimelineFilter {
    fn default() -> Self {
        Self {
            events: true,
            decisions: true,
            all_decisions: false,
        }
    }
}

/// An event on the timeline. Its properties are as stored, redacted at ingest; the viewer's
/// masking is applied by the caller from `schema`.
#[derive(Clone, PartialEq)]
pub struct TimelineEvent {
    pub id: Uuid,
    pub at: OffsetDateTime,
    pub name: String,
    pub properties: Value,
    /// The published schema version the event was validated against; absent for an event
    /// without one.
    pub schema: Option<Vec<SchemaProperty>>,
    pub validation_state: String,
    pub flags: Vec<String>,
    pub trace_id: Uuid,
    /// Whether the trace holds any decision. An accepted event writes none of its own, so the
    /// trace of an event that changed nothing is empty and the trace API answers 404 for it.
    pub has_decisions: bool,
}

// Event properties are personal data.
impl fmt::Debug for TimelineEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TimelineEvent")
            .field("id", &self.id)
            .field("at", &self.at)
            .finish_non_exhaustive()
    }
}

/// A decision about the person: ids and a reason code only.
#[derive(Clone, Debug, PartialEq)]
pub struct TimelineDecision {
    pub id: Uuid,
    pub at: OffsetDateTime,
    pub action: String,
    pub outcome: String,
    pub reason: Option<String>,
    pub cause_kind: String,
    pub subject_ids: Value,
    pub trace_id: Uuid,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TimelineEntry {
    Event(TimelineEvent),
    Decision(TimelineDecision),
}

impl TimelineEntry {
    pub fn cursor(&self) -> TimelineCursor {
        match self {
            Self::Event(event) => TimelineCursor {
                at: event.at,
                kind: TimelineKind::Event,
                id: event.id,
            },
            Self::Decision(decision) => TimelineCursor {
                at: decision.at,
                kind: TimelineKind::Decision,
                id: decision.id,
            },
        }
    }
}

/// A page, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct TimelinePage {
    pub entries: Vec<TimelineEntry>,
    /// Pass as `before` for the next page; absent on the last.
    pub next: Option<TimelineCursor>,
}

#[derive(FromRow)]
struct EventRow {
    id: Uuid,
    occurred_at: OffsetDateTime,
    name: String,
    properties: Value,
    schema: Option<Value>,
    validation_state: String,
    flags: Vec<String>,
    trace_id: Uuid,
    has_decisions: bool,
}

#[derive(FromRow)]
struct DecisionRow {
    id: Uuid,
    recorded_at: OffsetDateTime,
    action: String,
    outcome: String,
    reason: Option<String>,
    cause_kind: String,
    subject_ids: Value,
    trace_id: Uuid,
}

/// The people of a merge group, from the survivor (`$2`) down. The equality on
/// `merged_into` implies not-null, so `people_merged_into_idx` (a partial index) serves the
/// recursive step.
const GROUP_MEMBERS: &str = "WITH RECURSIVE group_ AS ( \
         SELECT id FROM people WHERE workspace_id = $1 AND id = $2 \
         UNION \
         SELECT p.id FROM people p JOIN group_ g ON p.merged_into = g.id \
         WHERE p.workspace_id = $1) \
     SELECT id FROM group_";

/// The people that read as `person`: the survivor it was merged into (or itself) and everyone
/// merged into that survivor. `None` when the workspace has no such person. Errors are
/// bounded-read errors, so a timeout is reported as one.
pub(crate) async fn person_group(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Option<Vec<Uuid>>, StoreError> {
    let current: Option<Uuid> = sqlx::query_scalar(
        "SELECT COALESCE(merged_into, id) FROM people WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::bounded)?;
    let Some(current) = current else {
        return Ok(None);
    };
    let people = sqlx::query_scalar(GROUP_MEMBERS)
        .bind(workspace.as_uuid())
        .bind(current)
        .fetch_all(connection)
        .await
        .map_err(StoreError::bounded)?;
    Ok(Some(people))
}

/// The timeline of `person` and the people merged into it, or `None` when the workspace has
/// no such person. A merged person reads as its survivor.
pub async fn load_timeline(
    pool: &PgPool,
    workspace: WorkspaceId,
    person: Uuid,
    filter: TimelineFilter,
    limit: u32,
    before: Option<TimelineCursor>,
) -> Result<Option<TimelinePage>, StoreError> {
    let limit = i64::from(limit.max(1));
    let mut tx = begin_bounded(pool).await?;
    let Some(people) = person_group(&mut tx, workspace, person).await? else {
        return Ok(None);
    };
    let texts: Vec<String> = people.iter().map(Uuid::to_string).collect();
    let (at, rank, id) = match before {
        Some(cursor) => (Some(cursor.at), cursor.kind.rank(), Some(cursor.id)),
        None => (None, 0, None),
    };

    let mut entries: Vec<TimelineEntry> = Vec::new();
    if filter.events {
        let rows: Vec<EventRow> = sqlx::query_as(
            "SELECT e.id, e.occurred_at, e.name, e.properties, v.properties AS schema, \
                    e.validation_state, e.flags, e.trace_id, \
                    EXISTS (SELECT 1 FROM decision_records d \
                            WHERE d.workspace_id = e.workspace_id AND d.trace_id = e.trace_id) \
                        AS has_decisions \
             FROM events e LEFT JOIN event_schema_versions v \
                ON v.workspace_id = e.workspace_id AND v.name = e.name \
               AND v.version = e.schema_version \
             WHERE e.workspace_id = $1 AND e.person_id = ANY($2) \
               AND ($3::timestamptz IS NULL OR e.occurred_at < $3 \
                    OR (e.occurred_at = $3 AND ($4 = 1 OR ($4 = 0 AND e.id < $5)))) \
             ORDER BY e.occurred_at DESC, e.id DESC LIMIT $6",
        )
        .bind(workspace.as_uuid())
        .bind(&people)
        .bind(at)
        .bind(rank)
        .bind(id)
        .bind(limit + 1)
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::bounded)?;
        entries.extend(rows.into_iter().map(|row| {
            TimelineEntry::Event(TimelineEvent {
                id: row.id,
                at: row.occurred_at,
                name: row.name,
                properties: row.properties,
                schema: row
                    .schema
                    .and_then(|schema| serde_json::from_value(schema).ok()),
                validation_state: row.validation_state,
                flags: row.flags,
                trace_id: row.trace_id,
                has_decisions: row.has_decisions,
            })
        }));
    }
    if filter.decisions {
        let rows: Vec<DecisionRow> = sqlx::query_as(DECISIONS)
            .bind(workspace.as_uuid())
            .bind(&texts)
            .bind(filter.all_decisions)
            .bind(EVENT_ACCEPTED)
            .bind(at)
            .bind(rank)
            .bind(id)
            .bind(limit + 1)
            .fetch_all(&mut *tx)
            .await
            .map_err(StoreError::bounded)?;
        entries.extend(rows.into_iter().map(|row| {
            TimelineEntry::Decision(TimelineDecision {
                id: row.id,
                at: row.recorded_at,
                action: row.action,
                outcome: row.outcome,
                reason: row.reason,
                cause_kind: row.cause_kind,
                subject_ids: row.subject_ids,
                trace_id: row.trace_id,
            })
        }));
    }
    drop(tx);

    let key = |entry: &TimelineEntry| {
        let cursor = entry.cursor();
        (cursor.at, cursor.kind.rank(), cursor.id)
    };
    entries.sort_by_key(|entry| std::cmp::Reverse(key(entry)));
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let next = if entries.len() > limit {
        entries.truncate(limit);
        entries.last().map(TimelineEntry::cursor)
    } else {
        None
    };
    Ok(Some(TimelinePage { entries, next }))
}

/// The decisions of a merged group, newest first. One bounded walk of `decision_records_person_idx`
/// per person, then a top-N over the walks: `= ANY(array)` over the `subject_ids ->> 'person'`
/// expression cannot return rows in index order, so the planner walks the primary key backwards
/// and filters every row it passes.
const DECISIONS: &str = "\
    SELECT d.id, d.recorded_at, d.action, d.outcome, d.reason, d.cause_kind, d.subject_ids, \
           d.trace_id \
    FROM unnest($2::text[]) AS u(person) \
    CROSS JOIN LATERAL ( \
        SELECT id, recorded_at, action, outcome, reason, cause_kind, subject_ids, trace_id \
        FROM decision_records \
        WHERE workspace_id = $1 AND subject_ids ? 'person' \
          AND (subject_ids ->> 'person') = u.person \
          AND ($3 OR action <> $4) \
          AND ($5::timestamptz IS NULL OR recorded_at < $5 \
               OR (recorded_at = $5 AND $6 = 1 AND id < $7)) \
        ORDER BY recorded_at DESC, id DESC LIMIT $8) d \
    ORDER BY d.recorded_at DESC, d.id DESC LIMIT $8";

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::Duration;
    use time::macros::datetime;
    use zirv_flow_core::consent::{ChangeSource, Channel, ConsentStatus};
    use zirv_flow_core::events::{EventMode, PropertyType, SchemaProperty};
    use zirv_flow_core::{
        Cause, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceName,
    };

    use super::*;
    use crate::events::{
        EventOutcome, EventRecord, NewEventSchema, apply_event_batch, create_event_schema,
        publish_schema_draft,
    };
    use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_batch};
    use crate::{ConsentChange, begin, commit, create_workspace, list_trace, set_consent};

    const T0: OffsetDateTime = datetime!(2026-03-01 10:00 UTC);
    const SECRET_EMAIL: &str = "ada.secret@example.com";
    const SECRET_TRAIT: &str = "platinum-tier-xyz";
    const SECRET_EVENT: &str = "checkout_completed_zz";

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn profile(source: &str, key: &str, identifiers: &[(&str, &str)], tier: &str) -> ProfileRecord {
        ProfileRecord {
            source: source.into(),
            stream: "s".into(),
            record_key: key.into(),
            identifiers: identifiers
                .iter()
                .map(|(kind, value)| ((*kind).into(), (*value).into()))
                .collect(),
            traits: json!({"tier": tier}).as_object().cloned().unwrap(),
            observed_at: T0,
            trace: TraceContext::root(Cause::api("test")),
        }
    }

    async fn apply_profile(pool: &PgPool, ws: WorkspaceId, record: ProfileRecord) -> Uuid {
        match apply_profile_batch(pool, ws, &[record])
            .await
            .unwrap()
            .remove(0)
        {
            ProfileOutcome::Applied { person_id, .. }
            | ProfileOutcome::Unchanged { person_id, .. } => person_id,
            ProfileOutcome::Rejected { reason } => panic!("rejected: {reason}"),
        }
    }

    async fn track(pool: &PgPool, ws: WorkspaceId, name: &str, email: &str) -> Uuid {
        let record = EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), email.into())],
            name: name.into(),
            properties: json!({"coupon": "SECRET-COUPON"})
                .as_object()
                .cloned()
                .unwrap(),
            timestamp: None,
            received_at: OffsetDateTime::now_utc(),
            message_id: None,
            trace: TraceContext::root(Cause::api("test")),
        };
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        match apply_event_batch(pool, ws, &key, &[record])
            .await
            .unwrap()
            .remove(0)
        {
            EventOutcome::Accepted { event_id, .. } => event_id,
            other => panic!("not accepted: {other:?}"),
        }
    }

    /// A decision as versions before 0.231.0 wrote it for an accepted event, which no code writes
    /// now.
    async fn insert_legacy_accepted(pool: &PgPool, ws: WorkspaceId, event: Uuid) {
        sqlx::query(
            "INSERT INTO decision_records \
                (id, workspace_id, trace_id, action, outcome, cause_kind, cause_id, subject_ids) \
             SELECT gen_random_uuid(), workspace_id, trace_id, 'event.accepted', 'unplanned', \
                    'api', 'test', jsonb_build_object('event', id, 'person', person_id) \
             FROM events WHERE id = $1 AND workspace_id = $2",
        )
        .bind(event)
        .bind(ws.as_uuid())
        .execute(pool)
        .await
        .unwrap();
    }

    async fn set_decision_time(pool: &PgPool, action: &str, at: OffsetDateTime) {
        sqlx::query("UPDATE decision_records SET recorded_at = $2 WHERE action = $1")
            .bind(action)
            .bind(at)
            .execute(pool)
            .await
            .unwrap();
    }

    struct Seeded {
        ws: WorkspaceId,
        survivor: Uuid,
        merged: Uuid,
        event: Uuid,
    }

    /// One person (identified by email, with a tier trait) with an event, a consent change and
    /// an anonymous person merged into it. Times are set to a known order, oldest first:
    /// `person.created` and `trait.change` (the same instant), the event, `consent.change`,
    /// `person.merge`.
    async fn seed(pool: &PgPool, slug: &str) -> Seeded {
        let ws = workspace(pool, slug).await;
        let survivor = apply_profile(
            pool,
            ws,
            profile("crm", "1", &[("email", SECRET_EMAIL)], SECRET_TRAIT),
        )
        .await;
        let merged = apply_profile(
            pool,
            ws,
            profile("web", "2", &[("anonymous_id", "anon-1")], "free"),
        )
        .await;
        assert_ne!(survivor, merged);
        let event = track(pool, ws, SECRET_EVENT, SECRET_EMAIL).await;
        insert_legacy_accepted(pool, ws, event).await;
        let mut tx = begin(pool).await.unwrap();
        set_consent(
            &mut tx,
            ws,
            &ConsentChange {
                dimensions: Default::default(),
                person: survivor,
                channel: Channel::Email,
                topic: None,
                status: ConsentStatus::Unsubscribed,
                source: ChangeSource::Api,
                evidence: json!({}),
                occurred_at: T0,
                actor: None,
            },
            &TraceContext::root(Cause::api("test")),
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
        let both = apply_profile(
            pool,
            ws,
            profile(
                "app",
                "3",
                &[("email", SECRET_EMAIL), ("anonymous_id", "anon-1")],
                SECRET_TRAIT,
            ),
        )
        .await;
        assert_eq!(both, survivor);

        for action in ["person.created", "trait.change"] {
            set_decision_time(pool, action, T0 + Duration::hours(1)).await;
        }
        sqlx::query("UPDATE events SET occurred_at = $1")
            .bind(T0 + Duration::hours(2))
            .execute(pool)
            .await
            .unwrap();
        set_decision_time(pool, "event.accepted", T0 + Duration::hours(2)).await;
        set_decision_time(pool, "consent.change", T0 + Duration::hours(3)).await;
        set_decision_time(pool, "person.merge", T0 + Duration::hours(4)).await;
        Seeded {
            ws,
            survivor,
            merged,
            event,
        }
    }

    async fn all_pages(seeded: &Seeded, pool: &PgPool, limit: u32) -> Vec<TimelineEntry> {
        let mut entries = Vec::new();
        let mut before = None;
        loop {
            let page = load_timeline(
                pool,
                seeded.ws,
                seeded.survivor,
                TimelineFilter::default(),
                limit,
                before,
            )
            .await
            .unwrap()
            .unwrap();
            assert!(page.entries.len() <= limit as usize);
            entries.extend(page.entries);
            match page.next {
                Some(next) => before = Some(next),
                None => return entries,
            }
        }
    }

    fn label(entry: &TimelineEntry) -> String {
        match entry {
            TimelineEntry::Event(_) => "event".to_owned(),
            TimelineEntry::Decision(decision) => decision.action.clone(),
        }
    }

    async fn labels(pool: &PgPool, seeded: &Seeded, filter: TimelineFilter) -> Vec<String> {
        load_timeline(pool, seeded.ws, seeded.survivor, filter, 100, None)
            .await
            .unwrap()
            .unwrap()
            .entries
            .iter()
            .map(label)
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merge_decision_is_on_the_survivors_timeline_through_the_person_key(pool: PgPool) {
        let seeded = seed(&pool, "one").await;

        let subjects: Value = sqlx::query_scalar(
            "SELECT subject_ids FROM decision_records WHERE action = 'person.merge'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(subjects["person"], seeded.survivor.to_string());
        assert_eq!(subjects["survivor"], seeded.survivor.to_string());
        assert_eq!(subjects["merged"], seeded.merged.to_string());
        let entries = all_pages(&seeded, &pool, 50).await;
        assert!(entries.iter().any(|entry| label(entry) == "person.merge"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_timeline_interleaves_an_event_a_trait_change_a_consent_change_and_a_merge_in_time_order(
        pool: PgPool,
    ) {
        let seeded = seed(&pool, "one").await;

        let entries = all_pages(&seeded, &pool, 50).await;

        let labels: Vec<String> = entries.iter().map(label).collect();
        let position = |name: &str| labels.iter().position(|l| l == name).unwrap();
        assert!(position("person.merge") < position("consent.change"));
        assert!(position("consent.change") < position("event"));
        assert!(position("event") < position("trait.change"));
        let times: Vec<OffsetDateTime> = entries.iter().map(|e| e.cursor().at).collect();
        assert!(times.windows(2).all(|pair| pair[0] >= pair[1]), "{times:?}");
        let event = entries
            .iter()
            .find_map(|entry| match entry {
                TimelineEntry::Event(event) => Some(event),
                TimelineEntry::Decision(_) => None,
            })
            .unwrap();
        assert_eq!(event.id, seeded.event);
        assert_eq!(event.name, SECRET_EVENT);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn paging_across_a_page_boundary_and_equal_times_loses_and_repeats_nothing(pool: PgPool) {
        let seeded = seed(&pool, "one").await;
        let whole = all_pages(&seeded, &pool, 100).await;

        for limit in [1, 2, 3] {
            let paged = all_pages(&seeded, &pool, limit).await;
            assert_eq!(paged, whole, "limit {limit}");
        }
        let mut ids: Vec<Uuid> = whole.iter().map(|e| e.cursor().id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), whole.len());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_entry_says_whether_its_trace_holds_a_decision(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let first = track(&pool, ws, "first_seen", SECRET_EMAIL).await;
        let second = track(&pool, ws, "seen_again", SECRET_EMAIL).await;
        let person: Uuid = sqlx::query_scalar("SELECT person_id FROM events WHERE id = $1")
            .bind(first)
            .fetch_one(&pool)
            .await
            .unwrap();

        let page = load_timeline(
            &pool,
            ws,
            person,
            TimelineFilter {
                decisions: false,
                ..TimelineFilter::default()
            },
            50,
            None,
        )
        .await
        .unwrap()
        .unwrap();

        let flags: Vec<(Uuid, bool)> = page
            .entries
            .iter()
            .filter_map(|entry| match entry {
                TimelineEntry::Event(event) => Some((event.id, event.has_decisions)),
                TimelineEntry::Decision(_) => None,
            })
            .collect();
        assert!(flags.contains(&(first, true)), "it created the person");
        assert!(flags.contains(&(second, false)), "it changed nothing");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_decision_probe_of_an_event_entry_is_an_index_scan_on_the_trace_index(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        sqlx::query(
            "INSERT INTO decision_records \
                 (id, workspace_id, trace_id, action, outcome, cause_kind, cause_id) \
             SELECT gen_random_uuid(), $1, gen_random_uuid(), 'step', 'done', 'api', 'x' \
             FROM generate_series(1, 50000)",
        )
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("VACUUM (ANALYZE) decision_records")
            .execute(&pool)
            .await
            .unwrap();
        track(&pool, ws, "first_seen", SECRET_EMAIL).await;

        let plan: Vec<String> = sqlx::query_scalar(
            "EXPLAIN (COSTS OFF) SELECT e.id, EXISTS (SELECT 1 FROM decision_records d \
                 WHERE d.workspace_id = e.workspace_id AND d.trace_id = e.trace_id) \
             FROM events e WHERE e.workspace_id = $1 AND e.occurred_at < now() + interval '1 day' \
             ORDER BY e.occurred_at DESC, e.id DESC LIMIT 51",
        )
        .bind(ws.as_uuid())
        .fetch_all(&pool)
        .await
        .unwrap();

        let plan = plan.join(
            "
",
        );
        eprintln!("{plan}");
        // The rows are in this month's partition; the empty ones read as a trivial sequential scan.
        let month: String = sqlx::query_scalar(
            "SELECT 'decision_records_y' || to_char(now(), 'YYYY') || 'm' || to_char(now(), 'MM')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            plan.contains(&format!(
                "Index Only Scan using {month}_workspace_id_trace_id"
            )),
            "{plan}"
        );
        assert!(!plan.contains(&format!("Seq Scan on {month}")), "{plan}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn event_accepted_is_left_out_unless_all_decisions_are_asked_for(pool: PgPool) {
        let seeded = seed(&pool, "one").await;

        let default = labels(&pool, &seeded, TimelineFilter::default()).await;
        let all = labels(
            &pool,
            &seeded,
            TimelineFilter {
                all_decisions: true,
                ..TimelineFilter::default()
            },
        )
        .await;
        let events_only = labels(
            &pool,
            &seeded,
            TimelineFilter {
                decisions: false,
                ..TimelineFilter::default()
            },
        )
        .await;
        let decisions_only = labels(
            &pool,
            &seeded,
            TimelineFilter {
                events: false,
                ..TimelineFilter::default()
            },
        )
        .await;

        assert!(!default.contains(&"event.accepted".to_owned()));
        assert!(
            all.contains(&"event.accepted".to_owned()),
            "an old row still reads"
        );
        assert_eq!(events_only, ["event"]);
        assert!(!decisions_only.contains(&"event".to_owned()));
        assert!(decisions_only.contains(&"person.merge".to_owned()));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merged_person_reads_as_its_survivor_and_another_workspace_reads_none(pool: PgPool) {
        let seeded = seed(&pool, "one").await;
        let read = |ws, person| {
            let pool = pool.clone();
            async move {
                load_timeline(&pool, ws, person, TimelineFilter::default(), 100, None)
                    .await
                    .unwrap()
            }
        };
        let other = workspace(&pool, "two").await;

        let through_loser = read(seeded.ws, seeded.merged).await.unwrap();
        let through_survivor = read(seeded.ws, seeded.survivor).await.unwrap();

        assert_eq!(through_loser, through_survivor);
        assert!(read(other, seeded.survivor).await.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_system_entrys_trace_resolves_to_decisions_that_hold_ids_and_reasons_only(
        pool: PgPool,
    ) {
        let seeded = seed(&pool, "one").await;
        let entries = all_pages(&seeded, &pool, 100).await;
        let mut resolved = 0;

        for entry in &entries {
            let trace = match entry {
                TimelineEntry::Event(event) => event.trace_id,
                TimelineEntry::Decision(decision) => decision.trace_id,
            };
            let (decisions, _) = list_trace(
                &pool,
                seeded.ws,
                zirv_flow_core::TraceId::from_uuid(trace),
                100,
            )
            .await
            .unwrap();
            assert!(!decisions.is_empty(), "trace {trace} holds no decisions");
            for decision in decisions {
                resolved += 1;
                let text = format!("{decision:?}");
                for secret in [SECRET_EMAIL, SECRET_TRAIT, SECRET_EVENT, "SECRET-COUPON"] {
                    assert!(!text.contains(secret), "{secret} in {text}");
                }
                for value in decision.subject_ids.as_object().unwrap().values() {
                    assert!(
                        value.as_str().unwrap().parse::<Uuid>().is_ok(),
                        "a subject is not an id"
                    );
                }
            }
        }
        assert!(resolved > 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_carries_its_published_schema_so_the_caller_can_mask(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        create_event_schema(
            &pool,
            ws,
            &NewEventSchema {
                name: "signup".into(),
                mode: EventMode::Allow,
                description: String::new(),
                properties: vec![SchemaProperty {
                    name: "coupon".into(),
                    property_type: PropertyType::String,
                    required: false,
                    enum_values: None,
                    description: String::new(),
                    pii: true,
                }],
            },
        )
        .await
        .unwrap();
        let mut tx = begin(&pool).await.unwrap();
        publish_schema_draft(&mut tx, ws, "signup", None)
            .await
            .unwrap();
        commit(tx).await.unwrap();
        let person = apply_profile(
            &pool,
            ws,
            profile("crm", "1", &[("email", SECRET_EMAIL)], "x"),
        )
        .await;
        track(&pool, ws, "signup", SECRET_EMAIL).await;
        track(&pool, ws, "unplanned_thing", SECRET_EMAIL).await;

        let page = load_timeline(
            &pool,
            ws,
            person,
            TimelineFilter {
                decisions: false,
                ..TimelineFilter::default()
            },
            10,
            None,
        )
        .await
        .unwrap()
        .unwrap();

        let schemas: Vec<(String, Option<usize>)> = page
            .entries
            .iter()
            .filter_map(|entry| match entry {
                TimelineEntry::Event(event) => {
                    Some((event.name.clone(), event.schema.as_ref().map(Vec::len)))
                }
                TimelineEntry::Decision(_) => None,
            })
            .collect();
        assert!(schemas.contains(&("signup".to_owned(), Some(1))));
        assert!(schemas.contains(&("unplanned_thing".to_owned(), None)));
    }

    async fn group_plan(pool: &PgPool, sql: &str, workspace: WorkspaceId, person: Uuid) -> String {
        let mut connection = pool.acquire().await.unwrap();
        let sql = format!("EXPLAIN (COSTS OFF) {sql}");
        let lines: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(workspace.as_uuid())
            .bind(person)
            .fetch_all(&mut *connection)
            .await
            .unwrap();
        lines.join("\n")
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_group_lookup_does_not_scan_every_person(pool: PgPool) {
        let ours = workspace(&pool, "ours").await;
        let other = workspace(&pool, "other").await;
        for (workspace, count) in [(ours, 60_000), (other, 60_000)] {
            sqlx::query(
                "INSERT INTO people (id, workspace_id) \
                 SELECT gen_random_uuid(), $1 FROM generate_series(1, $2)",
            )
            .bind(workspace.as_uuid())
            .bind(count)
            .execute(&pool)
            .await
            .unwrap();
        }
        let survivor: Uuid =
            sqlx::query_scalar("SELECT id FROM people WHERE workspace_id = $1 LIMIT 1")
                .bind(ours.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query("ANALYZE people").execute(&pool).await.unwrap();

        // Without the index (the schema before #397) the lookup scans the table.
        sqlx::query("DROP INDEX people_merged_into_idx")
            .execute(&pool)
            .await
            .unwrap();
        let before = group_plan(&pool, GROUP_MEMBERS, ours, survivor).await;
        sqlx::query("CREATE INDEX people_merged_into_idx ON people (workspace_id, merged_into) WHERE merged_into IS NOT NULL").execute(&pool).await.unwrap();
        sqlx::query("ANALYZE people").execute(&pool).await.unwrap();
        let current = group_plan(&pool, GROUP_MEMBERS, ours, survivor).await;

        assert!(
            !before.contains("people_merged_into_idx"),
            "the check does not catch a missing index\n{before}"
        );
        assert!(
            !current.contains("Seq Scan on people"),
            "the group lookup scans every person\n{current}"
        );
        assert!(current.contains("people_merged_into_idx"), "{current}");
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_merged_into_index_migration_accepts_the_index_built_by_hand(pool: PgPool) {
        crate::reset_database(&pool).await;
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = before
            .migrations
            .iter()
            .filter(|migration| migration.version < 89)
            .cloned()
            .collect();
        before.run(&pool).await.unwrap();
        sqlx::raw_sql(
            "CREATE INDEX CONCURRENTLY people_merged_into_idx \
             ON people (workspace_id, merged_into) WHERE merged_into IS NOT NULL",
        )
        .execute(&pool)
        .await
        .unwrap();

        crate::migrate(&pool).await.unwrap();

        let indexes: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_indexes WHERE indexname = 'people_merged_into_idx'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(indexes, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_decisions_of_a_group_are_read_through_the_person_index_not_the_primary_key(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "ours").await;
        let group = [Uuid::now_v7().to_string(), Uuid::now_v7().to_string()];
        // Many newer decisions of other people, and one older decision per member of the group.
        sqlx::query(
            "INSERT INTO decision_records \
                 (id, workspace_id, trace_id, recorded_at, action, outcome, cause_kind, cause_id, \
                  subject_ids) \
             SELECT gen_random_uuid(), $1, gen_random_uuid(), now() + s * interval '1 millisecond', \
                    'step', 'done', 'api', 'x', \
                    jsonb_build_object('person', gen_random_uuid()::text) \
             FROM generate_series(1, 60000) AS s",
        )
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO decision_records \
                 (id, workspace_id, trace_id, recorded_at, action, outcome, cause_kind, cause_id, \
                  subject_ids) \
             SELECT gen_random_uuid(), $1, gen_random_uuid(), now(), 'step', 'done', 'api', 'x', \
                    jsonb_build_object('person', p) \
             FROM unnest($2::text[]) AS p",
        )
        .bind(ws.as_uuid())
        .bind(&group[..])
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("VACUUM (ANALYZE) decision_records")
            .execute(&pool)
            .await
            .unwrap();

        let explain = |sql: String| {
            let pool = pool.clone();
            let group = group.to_vec();
            async move {
                let lines: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                    .bind(ws.as_uuid())
                    .bind(group)
                    .bind(false)
                    .bind(EVENT_ACCEPTED)
                    .bind(None::<OffsetDateTime>)
                    .bind(0_i32)
                    .bind(None::<Uuid>)
                    .bind(26_i64)
                    .fetch_all(&pool)
                    .await
                    .unwrap();
                lines.join("\n")
            }
        };
        let old = explain(
            "EXPLAIN (COSTS OFF) SELECT id, recorded_at, action, outcome, reason, cause_kind, \
                    subject_ids, trace_id \
             FROM decision_records \
             WHERE workspace_id = $1 AND subject_ids ? 'person' \
               AND (subject_ids ->> 'person') = ANY($2) \
               AND ($3 OR action <> $4) \
               AND ($5::timestamptz IS NULL OR recorded_at < $5 \
                    OR (recorded_at = $5 AND $6 = 1 AND id < $7)) \
             ORDER BY recorded_at DESC, id DESC LIMIT $8"
                .to_owned(),
        )
        .await;
        let current = explain(format!("EXPLAIN (COSTS OFF) {DECISIONS}")).await;

        // The rows are in this month's partition; the empty ones plan as trivial primary key scans.
        let month: String = sqlx::query_scalar(
            "SELECT 'decision_records_y' || to_char(now(), 'YYYY') || 'm' || to_char(now(), 'MM')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let walk = format!("Index Scan Backward using {month}_pkey");
        assert!(
            old.contains(&walk),
            "the check does not catch the old statement\n{old}"
        );
        assert!(
            !current.contains(&walk),
            "the group walks the primary key\n{current}"
        );
        assert!(
            current.contains(&format!("Index Scan using {month}_workspace_id_expr")),
            "the person index is not used\n{current}"
        );
    }
}
