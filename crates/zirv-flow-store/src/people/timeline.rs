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

/// The action of the decision that duplicates an event entry.
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
    /// Include `event.accepted`, which duplicates the event entry.
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
    let people = sqlx::query_scalar(
        "WITH RECURSIVE group_ AS (              SELECT id FROM people WHERE workspace_id = $1 AND id = $2              UNION              SELECT p.id FROM people p JOIN group_ g ON p.merged_into = g.id              WHERE p.workspace_id = $1)          SELECT id FROM group_",
    )
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
                    e.validation_state, e.flags, e.trace_id \
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
            })
        }));
    }
    if filter.decisions {
        let rows: Vec<DecisionRow> = sqlx::query_as(
            "SELECT id, recorded_at, action, outcome, reason, cause_kind, subject_ids, trace_id \
             FROM decision_records \
             WHERE workspace_id = $1 AND subject_ids ? 'person' \
               AND (subject_ids ->> 'person') = ANY($2) \
               AND ($3 OR action <> $4) \
               AND ($5::timestamptz IS NULL OR recorded_at < $5 \
                    OR (recorded_at = $5 AND $6 = 1 AND id < $7)) \
             ORDER BY recorded_at DESC, id DESC LIMIT $8",
        )
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
        let mut tx = begin(pool).await.unwrap();
        set_consent(
            &mut tx,
            ws,
            &ConsentChange {
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
        assert!(all.contains(&"event.accepted".to_owned()));
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

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_persons_enrollment_and_step_decisions_are_on_their_timeline_with_the_enrollments_trace(
        pool: PgPool,
    ) {
        use crate::{GraphPoint, NewDecision, append_decision};

        let ws = workspace(&pool, "flows").await;
        let person = apply_profile(
            &pool,
            ws,
            profile("crm", "1", &[("email", "a@example.com")], "x"),
        )
        .await;
        let (flow, enrollment) = (Uuid::now_v7(), Uuid::now_v7());
        let trace = TraceContext::root(Cause::trait_change(Uuid::now_v7()));
        for (action, outcome, node) in [
            ("flow.enroll", "enrolled", None),
            ("flow.step", "done", Some("a")),
            ("flow.wait", "waiting", Some("w")),
            ("flow.enrollment", "failed", Some("b")),
        ] {
            append_decision(
                &pool,
                &NewDecision {
                    graph: Some(GraphPoint {
                        version: 1,
                        node: node.map(str::to_owned),
                    }),
                    workspace_id: ws,
                    trace: trace.clone(),
                    action,
                    outcome,
                    reason: None,
                    subject_ids: vec![
                        ("flow", flow),
                        ("enrollment", enrollment),
                        ("person", person),
                    ],
                    job: None,
                },
            )
            .await
            .unwrap();
        }

        let page = load_timeline(&pool, ws, person, TimelineFilter::default(), 50, None)
            .await
            .unwrap()
            .unwrap();

        let flow_entries: Vec<&TimelineDecision> = page
            .entries
            .iter()
            .filter_map(|entry| match entry {
                TimelineEntry::Decision(decision) if decision.action.starts_with("flow.") => {
                    Some(decision)
                }
                _ => None,
            })
            .collect();
        assert_eq!(flow_entries.len(), 4);
        for entry in &flow_entries {
            assert_eq!(entry.trace_id, trace.trace_id.as_uuid(), "{}", entry.action);
            assert_eq!(
                entry.subject_ids["enrollment"],
                json!(enrollment.to_string())
            );
        }
        let (records, _) = list_trace(&pool, ws, trace.trace_id, 100).await.unwrap();
        assert_eq!(records.len(), 4, "the trace resolves to the same decisions");
    }
}
