//! Flow triggers (architecture 9.1 and 9.4): the payload of the `flow.enroll` job, the queries
//! that find the flows a membership change or an event triggers, and the `schedules` row of a
//! schedule trigger.
//!
//! Every trigger ends in a `flow.enroll` job of at most [`MAX_ENROLL_PEOPLE`] people. A job
//! either carries its people as `(person, occurrence)` pairs, or names an audience that the job
//! pages through, a page of [`MAX_ENROLL_PEOPLE`] at a time, with the occurrence the audience
//! enrolls for. The job calls [`super::enroll_entries`], which is idempotent on
//! `(flow, person, occurrence)`.

use hashbrown::HashMap;
use serde_json::{Value, json};
use sqlx::PgConnection;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::flow::{Audience, FlowSettings, SegmentChange, Trigger};
use zirv_flow_core::schedule::next_after;
use zirv_flow_core::token::sha256;
use zirv_flow_core::{Cause, CauseKind, TraceContext, TraceId, WorkspaceId};

use super::arrivals::Arrival;
use super::enroll::MAX_ENROLL_PEOPLE;
use crate::segments::{SegmentMembershipChange, SegmentMembershipChanges, lock_segment_refresh};
use crate::{Clock, NewJob, StoreError, enqueue};

/// The kind of the job that enrolls people in a flow.
pub const ENROLL_JOB_KIND: &str = "flow.enroll";

/// What made the enrollments of a `flow.enroll` job.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TriggerKind {
    Segment,
    Event,
    Schedule,
    Manual,
    Api,
}

impl TriggerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Segment => "segment",
            Self::Event => "event",
            Self::Schedule => "schedule",
            Self::Manual => "manual",
            Self::Api => "api",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::Segment,
            Self::Event,
            Self::Schedule,
            Self::Manual,
            Self::Api,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == text)
    }
}

/// The people an audience trigger enrolls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudienceSource {
    /// Every person who has not been merged into another.
    Everyone,
    /// The current members of a segment.
    Segment(Uuid),
    /// The members of a list.
    List(Uuid),
}

impl AudienceSource {
    pub fn to_value(self) -> Value {
        match self {
            Self::Everyone => json!({"type": "everyone"}),
            Self::Segment(id) => json!({"type": "segment", "segment_id": id}),
            Self::List(id) => json!({"type": "list", "list_id": id}),
        }
    }

    pub fn parse(value: &Value) -> Option<Self> {
        let id = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .and_then(|text| Uuid::parse_str(text).ok())
        };
        match value.get("type")?.as_str()? {
            "everyone" => Some(Self::Everyone),
            "segment" => id("segment_id").map(Self::Segment),
            "list" => id("list_id").map(Self::List),
            _ => None,
        }
    }
}

impl From<Audience> for AudienceSource {
    fn from(audience: Audience) -> Self {
        match audience {
            Audience::Everyone {} => Self::Everyone,
            Audience::Segment { segment_id } => Self::Segment(segment_id),
        }
    }
}

/// The people of a `flow.enroll` job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnrollWork {
    /// These people, each with the occurrence they enter for.
    People(Vec<(Uuid, Uuid)>),
    /// A page of an audience: the people after `after` in id order, all for one occurrence. A
    /// schedule's job has no occurrence yet; the job makes it from its slot. `entered_by` is set by
    /// a publish fan-out to the publish time: a segment page then holds only the members whose
    /// current membership began at or before it, and a later joiner enters through their own
    /// `segment.entered` occurrence. Schedule and manual fan-outs leave it unset.
    Audience {
        source: AudienceSource,
        occurrence: Option<Uuid>,
        after: Option<Uuid>,
        entered_by: Option<OffsetDateTime>,
    },
}

/// The payload of a `flow.enroll` job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnrollJobPayload {
    pub workspace: WorkspaceId,
    pub flow: Uuid,
    /// The version to enroll into; the flow's current one when absent.
    pub version: Option<i32>,
    pub trigger: TriggerKind,
    pub work: EnrollWork,
}

impl EnrollJobPayload {
    pub fn to_value(&self) -> Value {
        let mut payload = json!({
            "workspace_id": self.workspace,
            "flow_id": self.flow,
            "trigger": self.trigger.as_str(),
        });
        if let Some(version) = self.version {
            payload["version"] = json!(version);
        }
        match &self.work {
            EnrollWork::People(pairs) => {
                payload["people"] = Value::Array(
                    pairs
                        .iter()
                        .map(|(person, occurrence)| json!([person, occurrence]))
                        .collect(),
                );
            }
            EnrollWork::Audience {
                source,
                occurrence,
                after,
                entered_by,
            } => {
                payload["audience"] = source.to_value();
                if let Some(occurrence) = occurrence {
                    payload["occurrence"] = json!(occurrence);
                }
                if let Some(after) = after {
                    payload["after"] = json!(after);
                }
                if let Some(entered_by) = entered_by
                    && let Ok(text) = entered_by.format(&Rfc3339)
                {
                    payload["entered_by"] = json!(text);
                }
            }
        }
        payload
    }

    pub fn parse(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str);
        let uuid = |key: &str| text(key).and_then(|text| Uuid::parse_str(text).ok());
        let work = if let Some(people) = payload.get("people") {
            let pairs = people
                .as_array()?
                .iter()
                .map(|pair| {
                    let pair = pair.as_array()?;
                    let person = Uuid::parse_str(pair.first()?.as_str()?).ok()?;
                    let occurrence = Uuid::parse_str(pair.get(1)?.as_str()?).ok()?;
                    Some((person, occurrence))
                })
                .collect::<Option<Vec<_>>>()?;
            EnrollWork::People(pairs)
        } else {
            EnrollWork::Audience {
                source: AudienceSource::parse(payload.get("audience")?)?,
                occurrence: uuid("occurrence"),
                after: uuid("after"),
                entered_by: match text("entered_by") {
                    Some(text) => Some(OffsetDateTime::parse(text, &Rfc3339).ok()?),
                    None => None,
                },
            }
        };
        let version = match payload.get("version") {
            Some(version) => Some(i32::try_from(version.as_i64()?).ok()?),
            None => None,
        };
        Some(Self {
            workspace: text("workspace_id")?.parse().ok()?,
            flow: uuid("flow_id")?,
            version,
            trigger: TriggerKind::parse(text("trigger")?)?,
            work,
        })
    }

    /// The job that carries this payload. `unique_key` makes a second enqueue of the same work,
    /// while the first is still queued, a no-op.
    pub fn job(&self, trace: &TraceContext, unique_key: String) -> NewJob {
        let mut job = NewJob::new(ENROLL_JOB_KIND, trace.clone());
        job.workspace_id = Some(self.workspace);
        job.payload = self.to_value();
        job.unique_key = Some(unique_key);
        job
    }
}

/// An occurrence id that depends only on `scope`: the same text always gives the same id, so a
/// slot or a request key that arrives twice is one occurrence.
pub fn occurrence_of(scope: &str) -> Uuid {
    let digest = sha256(scope);
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // Mark it as a name-based (version 8) id so it cannot be mistaken for a time-ordered one.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// A published or paused flow that a segment's entry or exit triggers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TriggeredFlow {
    pub flow: Uuid,
    /// The flow's current version, which the trigger enrolls into.
    pub version: i32,
    pub change: SegmentChange,
}

/// The flows whose segment trigger names `segment`. A paused flow is included: its job records
/// the skip of each person, so a pause is counted and not silent.
pub async fn segment_trigger_flows(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment: Uuid,
) -> Result<Vec<TriggeredFlow>, StoreError> {
    let rows: Vec<(Uuid, i32, String)> = sqlx::query_as(
        "SELECT f.id, f.current_version, v.settings -> 'trigger' ->> 'on' \
         FROM flows f \
         JOIN flow_versions v ON v.flow_id = f.id AND v.version = f.current_version \
         WHERE f.workspace_id = $1 AND f.state IN ('published', 'paused') \
           AND v.settings -> 'trigger' ->> 'type' = 'segment' \
           AND v.settings -> 'trigger' ->> 'segment_id' = $2 \
         ORDER BY f.id",
    )
    .bind(workspace.as_uuid())
    .bind(segment.to_string())
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .filter_map(|(flow, version, on)| {
            let change = match on.as_str() {
                "entered" => SegmentChange::Entered,
                "exited" => SegmentChange::Exited,
                _ => return None,
            };
            Some(TriggeredFlow {
                flow,
                version,
                change,
            })
        })
        .collect())
}

/// Enqueues `flow.enroll` jobs of up to [`MAX_ENROLL_PEOPLE`] pairs for the changes `flows`
/// trigger on. The occurrence of a person is the id of the `segment.entered` or
/// `segment.exited` event that recorded the change, and a job runs under the trace and cause of
/// the change. A change caused by the segment's own definition triggers nothing (ruling
/// 336b80fa). Returns the number of jobs enqueued.
pub(crate) async fn enqueue_segment_enrolls(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flows: &[TriggeredFlow],
    changes: &SegmentMembershipChanges,
) -> Result<usize, StoreError> {
    let mut enqueued = 0;
    for triggered in flows {
        let list = match triggered.change {
            SegmentChange::Entered => &changes.entered,
            SegmentChange::Exited => &changes.exited,
        };
        // People of one trace and cause share jobs, so the job's trace is the change's.
        let mut groups: HashMap<(Uuid, CauseKind, &str), Vec<&SegmentMembershipChange>> =
            HashMap::new();
        let mut order = Vec::new();
        for change in list
            .iter()
            .filter(|change| change.trace.cause.kind != CauseKind::DefinitionChange)
        {
            let key = (
                change.trace.trace_id.as_uuid(),
                change.trace.cause.kind,
                change.trace.cause.id.as_str(),
            );
            groups
                .entry(key)
                .or_insert_with(|| {
                    order.push(key);
                    Vec::new()
                })
                .push(change);
        }
        for key in order {
            let members = &groups[&key];
            for chunk in members.chunks(MAX_ENROLL_PEOPLE) {
                let Some(first) = chunk.first() else {
                    continue;
                };
                let payload = EnrollJobPayload {
                    workspace,
                    flow: triggered.flow,
                    version: Some(triggered.version),
                    trigger: TriggerKind::Segment,
                    work: EnrollWork::People(
                        chunk
                            .iter()
                            .map(|change| (change.person, change.event))
                            .collect(),
                    ),
                };
                let unique = format!("flow.enroll:segment:{}:{}", triggered.flow, first.event);
                if enqueue(&mut *connection, &payload.job(&first.trace, unique)).await? {
                    enqueued += 1;
                }
            }
        }
    }
    Ok(enqueued)
}

/// Enqueues `flow.enroll` jobs for the flows whose event trigger names an event of `arrivals`,
/// with one query for the whole batch. The occurrence of a person is the event's id. The store
/// cannot read the segment AST, so the job applies the trigger's property filters to the stored
/// events. Returns the number of jobs enqueued.
pub(crate) async fn enqueue_event_enrolls(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    arrivals: &[Arrival],
) -> Result<usize, StoreError> {
    if arrivals.is_empty() {
        return Ok(0);
    }
    let events: Vec<Uuid> = arrivals.iter().map(|a| a.event_id).collect();
    let people: Vec<Uuid> = arrivals.iter().map(|a| a.person_id).collect();
    let names: Vec<&str> = arrivals.iter().map(|a| a.name.as_str()).collect();
    let matches: Vec<(Uuid, i32, Uuid, Uuid)> = sqlx::query_as(
        "SELECT f.id, f.current_version, t.person_id, t.event_id \
         FROM unnest($2::uuid[], $3::uuid[], $4::text[]) AS t(event_id, person_id, name) \
         JOIN flow_versions v ON v.workspace_id = $1 \
                             AND v.settings -> 'trigger' ->> 'type' = 'event' \
                             AND v.settings -> 'trigger' ->> 'event' = t.name \
         JOIN flows f ON f.id = v.flow_id AND f.current_version = v.version \
                     AND f.workspace_id = $1 AND f.state IN ('published', 'paused') \
         ORDER BY f.id, t.person_id, t.event_id",
    )
    .bind(workspace.as_uuid())
    .bind(&events)
    .bind(&people)
    .bind(&names)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    // The rows come ordered by flow: one entry per flow with its version and its pairs.
    type Pairs = Vec<(Uuid, Uuid)>;
    let mut by_flow: Vec<(Uuid, i32, Pairs)> = Vec::new();
    for (flow, version, person, event) in matches {
        match by_flow.last_mut() {
            Some((last, _, pairs)) if *last == flow => pairs.push((person, event)),
            _ => by_flow.push((flow, version, vec![(person, event)])),
        }
    }
    let mut enqueued = 0;
    for (flow, version, pairs) in by_flow {
        for chunk in pairs.chunks(MAX_ENROLL_PEOPLE) {
            let Some(&(_, first)) = chunk.first() else {
                continue;
            };
            let payload = EnrollJobPayload {
                workspace,
                flow,
                version: Some(version),
                trigger: TriggerKind::Event,
                work: EnrollWork::People(chunk.to_vec()),
            };
            let trace = TraceContext {
                trace_id: TraceId::new(),
                cause: Cause::event(first),
            };
            let unique = format!("flow.enroll:event:{flow}:{first}");
            if enqueue(&mut *connection, &payload.job(&trace, unique)).await? {
                enqueued += 1;
            }
        }
    }
    Ok(enqueued)
}

/// An event as the trigger filters see it, with the person it belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct TriggerEvent {
    pub id: Uuid,
    pub person_id: Uuid,
    pub name: String,
    pub occurred_at: OffsetDateTime,
    pub properties: Value,
}

/// Reads events by id. An erased event is simply missing.
pub async fn find_events(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    ids: &[Uuid],
) -> Result<Vec<TriggerEvent>, StoreError> {
    let rows: Vec<(Uuid, Uuid, String, OffsetDateTime, Value)> = sqlx::query_as(
        "SELECT id, person_id, name, occurred_at, properties FROM events \
         WHERE workspace_id = $1 AND id = ANY($2) AND person_id IS NOT NULL",
    )
    .bind(workspace.as_uuid())
    .bind(ids)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(
            |(id, person_id, name, occurred_at, properties)| TriggerEvent {
                id,
                person_id,
                name,
                occurred_at,
                properties,
            },
        )
        .collect())
}

/// The next page of an audience: up to [`MAX_ENROLL_PEOPLE`] people with an id after `after`, in
/// id order. A person merged into another is never in a page. With `entered_by` set, a segment
/// page holds only the members whose current membership began at or before it (a publish
/// fan-out's snapshot); the other sources ignore it.
pub async fn audience_page(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    source: AudienceSource,
    after: Option<Uuid>,
    entered_by: Option<OffsetDateTime>,
) -> Result<Vec<Uuid>, StoreError> {
    let limit = i64::try_from(MAX_ENROLL_PEOPLE).unwrap_or(1000);
    let page = match source {
        AudienceSource::Everyone => {
            sqlx::query_scalar(
                "SELECT id FROM people \
                 WHERE workspace_id = $1 AND merged_into IS NULL \
                   AND ($2::uuid IS NULL OR id > $2) \
                 ORDER BY id LIMIT $3",
            )
            .bind(workspace.as_uuid())
            .bind(after)
            .bind(limit)
            .fetch_all(connection)
            .await
        }
        AudienceSource::Segment(segment) => {
            sqlx::query_scalar(
                "SELECT person_id FROM segment_members \
                 WHERE workspace_id = $1 AND segment_id = $2 AND exited_at IS NULL \
                   AND ($3::uuid IS NULL OR person_id > $3) \
                   AND ($5::timestamptz IS NULL OR entered_at <= $5) \
                 ORDER BY person_id LIMIT $4",
            )
            .bind(workspace.as_uuid())
            .bind(segment)
            .bind(after)
            .bind(limit)
            .bind(entered_by)
            .fetch_all(connection)
            .await
        }
        AudienceSource::List(list) => {
            sqlx::query_scalar(
                "SELECT person_id FROM list_members \
                 WHERE workspace_id = $1 AND list_id = $2 \
                   AND ($3::uuid IS NULL OR person_id > $3) \
                 ORDER BY person_id LIMIT $4",
            )
            .bind(workspace.as_uuid())
            .bind(list)
            .bind(after)
            .bind(limit)
            .fetch_all(connection)
            .await
        }
    };
    page.map_err(StoreError::query)
}

/// Makes the `schedules` row of a flow match its trigger, in the caller's transaction: a schedule
/// trigger has one row with the trigger's cron and zone, any other trigger none. A changed cron or
/// zone moves the first slot to the next one after now; an unchanged one keeps its place.
pub(crate) async fn sync_flow_schedule(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
    settings: &FlowSettings,
) -> Result<(), StoreError> {
    let Trigger::Schedule {
        cron,
        timezone,
        audience,
    } = &settings.trigger
    else {
        return remove_flow_schedule(connection, workspace, flow).await;
    };
    let Ok(next_run_at) = next_after(cron, timezone, OffsetDateTime::now_utc()) else {
        // The publish validated the cron and the zone; a cron with no later slot has nothing to
        // fire.
        return remove_flow_schedule(connection, workspace, flow).await;
    };
    let payload = EnrollJobPayload {
        workspace,
        flow,
        version: None,
        trigger: TriggerKind::Schedule,
        work: EnrollWork::Audience {
            source: (*audience).into(),
            occurrence: None,
            after: None,
            entered_by: None,
        },
    }
    .to_value();
    sqlx::query(
        "INSERT INTO schedules (id, workspace_id, kind, cron, timezone, next_run_at, payload, \
            enabled, flow_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, true, $8) \
         ON CONFLICT (flow_id) WHERE flow_id IS NOT NULL DO UPDATE SET \
            next_run_at = CASE WHEN schedules.cron <> EXCLUDED.cron \
                                 OR schedules.timezone <> EXCLUDED.timezone \
                               THEN EXCLUDED.next_run_at ELSE schedules.next_run_at END, \
            cron = EXCLUDED.cron, timezone = EXCLUDED.timezone, payload = EXCLUDED.payload, \
            enabled = true \
         WHERE schedules.workspace_id = EXCLUDED.workspace_id",
    )
    .bind(Uuid::new_v4())
    .bind(workspace.as_uuid())
    .bind(ENROLL_JOB_KIND)
    .bind(cron)
    .bind(timezone)
    .bind(next_run_at)
    .bind(payload)
    .bind(flow)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Removes the flow's `schedules` row, if it has one.
pub(crate) async fn remove_flow_schedule(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM schedules WHERE flow_id = $1 AND workspace_id = $2")
        .bind(flow)
        .bind(workspace.as_uuid())
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

/// Gives every published or paused flow with a schedule trigger and no `schedules` row one, in
/// the caller's transaction: a flow published before this existed, or whose row was lost. Only
/// unscheduled flows are touched. Returns the rows created; a second call finds nothing to do.
pub async fn reconcile_flow_schedules(connection: &mut PgConnection) -> Result<u64, StoreError> {
    let unscheduled: Vec<(Uuid, Uuid, Value)> = sqlx::query_as(
        "SELECT f.workspace_id, f.id, v.settings FROM flows f \
         JOIN flow_versions v ON v.flow_id = f.id AND v.version = f.current_version \
         WHERE f.state IN ('published', 'paused') \
           AND v.settings -> 'trigger' ->> 'type' = 'schedule' \
           AND NOT EXISTS (SELECT 1 FROM schedules s WHERE s.flow_id = f.id) \
         ORDER BY f.id",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut created = 0;
    for (workspace, flow, settings) in unscheduled {
        let Ok(settings) = serde_json::from_value::<FlowSettings>(settings) else {
            continue;
        };
        let workspace = WorkspaceId::from_uuid(workspace);
        sync_flow_schedule(connection, workspace, flow, &settings).await?;
        created += 1;
    }
    Ok(created)
}

/// Enqueues the audience fan-out of a segment trigger with `enroll_current`, for the version
/// being published. The publish is the occurrence: its trace id. The fan-out carries the publish
/// time, read from the store's clock (the one that stamps `segment_members.entered_at`), so it
/// enrolls only the members present at the publish.
///
/// It takes the segment's refresh lock first, for the rest of the publish transaction. A refresh
/// or re-evaluation of the segment in flight read the flows that trigger on it before this
/// publish committed, so it queues no enroll for this flow; without the lock it could stamp a
/// member before the publish time and commit after the last page ran, and nothing would enroll
/// that member. Waiting for it makes its members visible to the pages; one that starts later
/// waits for this transaction and then sees the published flow. The lock comes after the flow
/// row lock, and no refresh path waits for a flow row while holding it.
pub(crate) async fn enqueue_publish_fan_out(
    connection: &mut PgConnection,
    clock: &Clock,
    workspace: WorkspaceId,
    flow: Uuid,
    version: i32,
    settings: &FlowSettings,
    trace: &TraceContext,
) -> Result<(), StoreError> {
    let Trigger::Segment {
        segment_id,
        enroll_current: true,
        ..
    } = &settings.trigger
    else {
        return Ok(());
    };
    lock_segment_refresh(connection, *segment_id).await?;
    let published_at = clock.now(connection).await?;
    let payload = EnrollJobPayload {
        workspace,
        flow,
        version: Some(version),
        trigger: TriggerKind::Segment,
        work: EnrollWork::Audience {
            source: AudienceSource::Segment(*segment_id),
            occurrence: Some(trace.trace_id.as_uuid()),
            after: None,
            entered_by: Some(published_at),
        },
    };
    let unique = format!("flow.enroll:publish:{flow}:{version}");
    enqueue(connection, &payload.job(trace, unique)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_workspace() -> WorkspaceId {
        WorkspaceId::from_uuid(Uuid::from_u128(1))
    }

    #[test]
    fn a_payload_of_people_round_trips() {
        let payload = EnrollJobPayload {
            workspace: unit_workspace(),
            flow: Uuid::from_u128(2),
            version: Some(3),
            trigger: TriggerKind::Segment,
            work: EnrollWork::People(vec![
                (Uuid::from_u128(4), Uuid::from_u128(5)),
                (Uuid::from_u128(6), Uuid::from_u128(7)),
            ]),
        };

        assert_eq!(EnrollJobPayload::parse(&payload.to_value()), Some(payload));
    }

    #[test]
    fn a_payload_of_an_audience_round_trips_with_and_without_an_occurrence() {
        for (source, occurrence, after, version, entered_by) in [
            (AudienceSource::Everyone, None, None, None, None),
            (
                AudienceSource::Segment(Uuid::from_u128(8)),
                Some(Uuid::from_u128(9)),
                Some(Uuid::from_u128(10)),
                Some(2),
                Some(time::macros::datetime!(2030-01-02 03:04:05.678901 UTC)),
            ),
            (
                AudienceSource::List(Uuid::from_u128(11)),
                None,
                None,
                None,
                None,
            ),
        ] {
            let payload = EnrollJobPayload {
                workspace: unit_workspace(),
                flow: Uuid::from_u128(2),
                version,
                trigger: TriggerKind::Manual,
                work: EnrollWork::Audience {
                    source,
                    occurrence,
                    after,
                    entered_by,
                },
            };

            assert_eq!(EnrollJobPayload::parse(&payload.to_value()), Some(payload));
        }
    }

    #[test]
    fn a_payload_that_is_not_a_job_is_refused() {
        let good = EnrollJobPayload {
            workspace: unit_workspace(),
            flow: Uuid::from_u128(2),
            version: None,
            trigger: TriggerKind::Api,
            work: EnrollWork::People(vec![(Uuid::from_u128(4), Uuid::from_u128(5))]),
        }
        .to_value();
        assert!(EnrollJobPayload::parse(&good).is_some());

        for broken in [
            json!({}),
            json!({"workspace_id": unit_workspace(), "flow_id": Uuid::from_u128(2), "trigger": "api"}),
            {
                let mut value = good.clone();
                value["trigger"] = json!("telepathy");
                value
            },
            {
                let mut value = good.clone();
                value["people"] = json!([["not a uuid", "nor this"]]);
                value
            },
            {
                let mut value = good.clone();
                value["version"] = json!("one");
                value
            },
        ] {
            assert_eq!(EnrollJobPayload::parse(&broken), None, "{broken}");
        }
    }

    #[test]
    fn the_text_of_a_trigger_round_trips() {
        for kind in [
            TriggerKind::Segment,
            TriggerKind::Event,
            TriggerKind::Schedule,
            TriggerKind::Manual,
            TriggerKind::Api,
        ] {
            assert_eq!(TriggerKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(TriggerKind::parse("webhook"), None);
    }

    #[test]
    fn an_occurrence_of_the_same_scope_is_the_same_id_and_another_scope_another() {
        let first = occurrence_of("flow.api:a:key");

        assert_eq!(first, occurrence_of("flow.api:a:key"));
        assert_ne!(first, occurrence_of("flow.api:a:other"));
        assert_eq!(first.get_version_num(), 8);
    }

    use sqlx::PgPool;
    use zirv_flow_core::flow::SegmentChange;

    use crate::flows::tests_support::{create, lifecycle, publish, workspace};
    use crate::flows::{FlowAction, FlowPatch, update_flow_draft};

    /// A published flow with the given settings.
    async fn published(pool: &PgPool, ws: WorkspaceId, name: &str, settings: Value) -> Uuid {
        let flow = create(pool, ws, name).await;
        let mut connection = pool.acquire().await.unwrap();
        update_flow_draft(
            &mut connection,
            ws,
            flow.id,
            &FlowPatch {
                settings: Some(settings),
                ..FlowPatch::default()
            },
        )
        .await
        .unwrap();
        drop(connection);
        publish(pool, ws, flow.id).await.unwrap();
        flow.id
    }

    fn segment_trigger(segment: Uuid, on: &str) -> Value {
        json!({
            "trigger": {"type": "segment", "segment_id": segment, "on": on},
            "reentry": {"mode": "once"}
        })
    }

    async fn segment_row(pool: &PgPool, ws: WorkspaceId, name: &str) -> Uuid {
        crate::segments::tests_support::segment(pool, ws, name, 60)
            .await
            .id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_segment_trigger_finds_published_and_paused_flows_of_that_segment_only(pool: PgPool) {
        let ws = workspace(&pool, "triggers").await;
        let (one, two) = (
            segment_row(&pool, ws, "One").await,
            segment_row(&pool, ws, "Two").await,
        );
        let entering = published(&pool, ws, "In", segment_trigger(one, "entered")).await;
        let leaving = published(&pool, ws, "Out", segment_trigger(one, "exited")).await;
        let paused = published(&pool, ws, "Paused", segment_trigger(one, "entered")).await;
        lifecycle(&pool, ws, paused, FlowAction::Pause)
            .await
            .unwrap();
        let archived = published(&pool, ws, "Archived", segment_trigger(one, "entered")).await;
        lifecycle(&pool, ws, archived, FlowAction::Archive)
            .await
            .unwrap();
        published(&pool, ws, "Other", segment_trigger(two, "entered")).await;
        create(&pool, ws, "Draft").await;
        let mut connection = pool.acquire().await.unwrap();

        let found = segment_trigger_flows(&mut connection, ws, one)
            .await
            .unwrap();

        let mut flows: Vec<(Uuid, SegmentChange)> =
            found.iter().map(|f| (f.flow, f.change)).collect();
        flows.sort_by_key(|(flow, _)| *flow);
        let mut expected = vec![
            (entering, SegmentChange::Entered),
            (leaving, SegmentChange::Exited),
            (paused, SegmentChange::Entered),
        ];
        expected.sort_by_key(|(flow, _)| *flow);
        assert_eq!(flows, expected);
        assert!(found.iter().all(|f| f.version == 1));
    }

    fn change(person: u128, event: u128, trace: &TraceContext) -> SegmentMembershipChange {
        SegmentMembershipChange {
            event: Uuid::from_u128(event),
            person: Uuid::from_u128(person),
            trace: trace.clone(),
        }
    }

    async fn enroll_jobs(pool: &PgPool) -> Vec<(Value, String, String)> {
        sqlx::query_as(
            "SELECT payload, cause_kind, cause_id FROM jobs WHERE kind = 'flow.enroll' \
             ORDER BY unique_key",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn segment_changes_become_jobs_of_a_thousand_pairs_under_the_trace_of_the_change(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "chunks").await;
        let segment = segment_row(&pool, ws, "Pros").await;
        let flow = published(&pool, ws, "In", segment_trigger(segment, "entered")).await;
        let mut connection = pool.acquire().await.unwrap();
        let triggered = segment_trigger_flows(&mut connection, ws, segment)
            .await
            .unwrap();
        let schedule = TraceContext::root(Cause::schedule(Uuid::from_u128(7)));
        let manual = TraceContext::root(Cause::api("req"));
        let definition = TraceContext::root(Cause::definition_change(segment));
        let mut entered: Vec<SegmentMembershipChange> = (0..2_001)
            .map(|n| change(n, 10_000 + n, &schedule))
            .collect();
        entered.push(change(5_000, 20_000, &manual));
        entered.push(change(5_001, 20_001, &definition));
        let changes = SegmentMembershipChanges {
            entered,
            exited: vec![change(1, 30_000, &schedule)],
        };

        let first = enqueue_segment_enrolls(&mut connection, ws, &triggered, &changes)
            .await
            .unwrap();
        let again = enqueue_segment_enrolls(&mut connection, ws, &triggered, &changes)
            .await
            .unwrap();

        // 2,001 under the schedule's trace make three jobs; the api change makes one; the one
        // caused by the definition makes none; and the exit triggers nothing for an entry flow.
        assert_eq!((first, again), (4, 0));
        let jobs = enroll_jobs(&pool).await;
        let sizes: Vec<usize> = jobs
            .iter()
            .map(
                |(payload, _, _)| match EnrollJobPayload::parse(payload).unwrap().work {
                    EnrollWork::People(pairs) => pairs.len(),
                    EnrollWork::Audience { .. } => 0,
                },
            )
            .collect();
        let mut sorted = sizes.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, [1, 1, 1000, 1000]);
        assert!(jobs.iter().all(|(payload, _, _)| {
            let parsed = EnrollJobPayload::parse(payload).unwrap();
            parsed.flow == flow
                && parsed.trigger == TriggerKind::Segment
                && parsed.version == Some(1)
        }));
        let causes: Vec<&str> = jobs.iter().map(|(_, kind, _)| kind.as_str()).collect();
        assert_eq!(causes.iter().filter(|kind| **kind == "schedule").count(), 3);
        assert_eq!(causes.iter().filter(|kind| **kind == "api").count(), 1);
        assert!(!causes.contains(&"definition_change"));
    }

    fn schedule_trigger(cron: &str) -> Value {
        json!({
            "trigger": {
                "type": "schedule", "cron": cron, "timezone": "Europe/Copenhagen",
                "audience": {"type": "everyone"}
            },
            "reentry": {"mode": "once"}
        })
    }

    fn manual_trigger() -> Value {
        json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}})
    }

    async fn republish(pool: &PgPool, ws: WorkspaceId, flow: Uuid, settings: Value) {
        let mut connection = pool.acquire().await.unwrap();
        update_flow_draft(
            &mut connection,
            ws,
            flow,
            &FlowPatch {
                settings: Some(settings),
                ..FlowPatch::default()
            },
        )
        .await
        .unwrap();
        drop(connection);
        publish(pool, ws, flow).await.unwrap();
    }

    type ScheduleRow = (String, String, String, bool, OffsetDateTime, Value);

    async fn schedule_rows(pool: &PgPool) -> Vec<ScheduleRow> {
        sqlx::query_as(
            "SELECT kind, cron, timezone, enabled, next_run_at, payload FROM schedules \
             WHERE flow_id IS NOT NULL",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_schedule_trigger_is_mirrored_to_a_row_that_moves_only_with_its_cron(pool: PgPool) {
        let ws = workspace(&pool, "schedules").await;
        let flow = published(&pool, ws, "Daily", schedule_trigger("0 9 * * *")).await;

        let first = schedule_rows(&pool).await;
        assert_eq!(first.len(), 1);
        let (kind, cron, zone, enabled, next, payload) = first[0].clone();
        assert_eq!(
            (kind.as_str(), cron.as_str(), zone.as_str(), enabled),
            ("flow.enroll", "0 9 * * *", "Europe/Copenhagen", true)
        );
        assert!(next > OffsetDateTime::now_utc());
        let parsed = EnrollJobPayload::parse(&payload).unwrap();
        assert_eq!(parsed.flow, flow);
        assert_eq!(parsed.trigger, TriggerKind::Schedule);
        assert_eq!(
            parsed.work,
            EnrollWork::Audience {
                source: AudienceSource::Everyone,
                occurrence: None,
                after: None,
                entered_by: None
            }
        );

        // The same cron keeps its slot; another one moves it.
        sqlx::query("UPDATE schedules SET next_run_at = next_run_at + interval '30 days'")
            .execute(&pool)
            .await
            .unwrap();
        let kept = schedule_rows(&pool).await[0].4;
        republish(&pool, ws, flow, schedule_trigger("0 9 * * *")).await;
        assert_eq!(schedule_rows(&pool).await[0].4, kept);
        republish(&pool, ws, flow, schedule_trigger("30 18 * * *")).await;
        let moved = schedule_rows(&pool).await;
        assert_eq!(moved[0].1, "30 18 * * *");
        assert!(moved[0].4 < kept);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn another_trigger_or_an_archive_takes_the_schedule_away(pool: PgPool) {
        let ws = workspace(&pool, "unscheduled").await;
        let flow = published(&pool, ws, "Daily", schedule_trigger("0 9 * * *")).await;
        assert_eq!(schedule_rows(&pool).await.len(), 1);

        republish(&pool, ws, flow, manual_trigger()).await;
        assert!(schedule_rows(&pool).await.is_empty());
        republish(&pool, ws, flow, schedule_trigger("0 9 * * *")).await;
        assert_eq!(schedule_rows(&pool).await.len(), 1);
        lifecycle(&pool, ws, flow, FlowAction::Pause).await.unwrap();
        assert_eq!(schedule_rows(&pool).await.len(), 1, "a pause keeps the row");
        lifecycle(&pool, ws, flow, FlowAction::Archive)
            .await
            .unwrap();
        assert!(schedule_rows(&pool).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn reconciling_gives_a_live_flow_its_missing_schedule_once(pool: PgPool) {
        let ws = workspace(&pool, "reconcile").await;
        let live = published(&pool, ws, "Live", schedule_trigger("0 9 * * *")).await;
        let paused = published(&pool, ws, "Paused", schedule_trigger("0 10 * * *")).await;
        lifecycle(&pool, ws, paused, FlowAction::Pause)
            .await
            .unwrap();
        create(&pool, ws, "Draft").await;
        published(&pool, ws, "Manual", manual_trigger()).await;
        sqlx::query("DELETE FROM schedules WHERE flow_id IS NOT NULL")
            .execute(&pool)
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        let created = reconcile_flow_schedules(&mut connection).await.unwrap();
        let again = reconcile_flow_schedules(&mut connection).await.unwrap();

        assert_eq!((created, again), (2, 0));
        let mut flows: Vec<Uuid> =
            sqlx::query_scalar("SELECT flow_id FROM schedules WHERE flow_id IS NOT NULL")
                .fetch_all(&pool)
                .await
                .unwrap();
        flows.sort_unstable();
        let mut expected = vec![live, paused];
        expected.sort_unstable();
        assert_eq!(flows, expected);
    }

    fn event_trigger(name: &str) -> Value {
        json!({"trigger": {"type": "event", "event": name}, "reentry": {"mode": "once"}})
    }

    fn arrival(person: u128, event: u128, name: &str) -> Arrival {
        Arrival {
            event_id: Uuid::from_u128(event),
            person_id: Uuid::from_u128(person),
            name: name.into(),
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn one_query_matches_a_batch_of_events_to_the_flows_that_name_them(pool: PgPool) {
        let ws = workspace(&pool, "events").await;
        let orders = published(&pool, ws, "Orders", event_trigger("order")).await;
        let refunds = published(&pool, ws, "Refunds", event_trigger("refund")).await;
        let paused = published(&pool, ws, "Paused", event_trigger("order")).await;
        lifecycle(&pool, ws, paused, FlowAction::Pause)
            .await
            .unwrap();
        published(&pool, ws, "Manual", manual_trigger()).await;
        let batch = [
            arrival(2, 12, "order"),
            arrival(1, 11, "order"),
            arrival(1, 13, "refund"),
            arrival(3, 14, "signup"),
        ];
        let mut connection = pool.acquire().await.unwrap();

        let first = enqueue_event_enrolls(&mut connection, ws, &batch)
            .await
            .unwrap();
        let again = enqueue_event_enrolls(&mut connection, ws, &batch)
            .await
            .unwrap();

        assert_eq!((first, again), (3, 0));
        let jobs = enroll_jobs(&pool).await;
        let mut seen: Vec<(Uuid, Vec<(u128, u128)>)> = jobs
            .iter()
            .map(|(payload, kind, _)| {
                assert_eq!(kind, "event");
                let parsed = EnrollJobPayload::parse(payload).unwrap();
                assert_eq!(parsed.trigger, TriggerKind::Event);
                let EnrollWork::People(pairs) = parsed.work else {
                    panic!("an event trigger lists its people");
                };
                (
                    parsed.flow,
                    pairs
                        .iter()
                        .map(|(person, event)| (person.as_u128(), event.as_u128()))
                        .collect(),
                )
            })
            .collect();
        seen.sort();
        let mut expected = vec![
            (orders, vec![(1, 11), (2, 12)]),
            (paused, vec![(1, 11), (2, 12)]),
            (refunds, vec![(1, 13)]),
        ];
        expected.sort();
        assert_eq!(seen, expected);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn enroll_current_queues_the_fan_out_of_the_segment_once_per_publish(pool: PgPool) {
        let ws = workspace(&pool, "fanout").await;
        let segment = segment_row(&pool, ws, "Members").await;
        let settings = |current: bool| {
            json!({
                "trigger": {"type": "segment", "segment_id": segment, "on": "entered",
                            "enroll_current": current},
                "reentry": {"mode": "once"}
            })
        };
        let flow = published(&pool, ws, "Members", settings(true)).await;
        republish(&pool, ws, flow, settings(true)).await;
        published(&pool, ws, "Nobody", settings(false)).await;

        let jobs = enroll_jobs(&pool).await;

        assert_eq!(
            jobs.len(),
            2,
            "one per publish, none without enroll_current"
        );
        let traces: Vec<Uuid> = sqlx::query_scalar(
            "SELECT trace_id FROM flow_versions WHERE flow_id = $1 ORDER BY version",
        )
        .bind(flow)
        .fetch_all(&pool)
        .await
        .unwrap();
        let mut seen: Vec<(Option<i32>, Option<Uuid>)> = jobs
            .iter()
            .map(|(payload, _, _)| {
                let parsed = EnrollJobPayload::parse(payload).unwrap();
                assert_eq!(parsed.flow, flow);
                let EnrollWork::Audience {
                    source, occurrence, ..
                } = parsed.work
                else {
                    panic!("a fan-out names the segment");
                };
                assert_eq!(source, AudienceSource::Segment(segment));
                (parsed.version, occurrence)
            })
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            [(Some(1), Some(traces[0])), (Some(2), Some(traces[1]))]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_audience_is_read_in_id_order_a_thousand_at_a_time(pool: PgPool) {
        let ws = workspace(&pool, "audience").await;
        sqlx::query(
            "INSERT INTO people (id, workspace_id) \
             SELECT gen_random_uuid(), $1 FROM generate_series(1, 2500)",
        )
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        // One person merged into another leaves the audience.
        sqlx::query(
            "UPDATE people SET merged_into = (SELECT id FROM people ORDER BY id LIMIT 1) \
             WHERE id = (SELECT id FROM people ORDER BY id DESC LIMIT 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let segment = segment_row(&pool, ws, "Members").await;
        sqlx::query(
            "INSERT INTO segment_members (workspace_id, segment_id, person_id, version, \
                entered_at, entered_trace_id, entered_cause) \
             SELECT $1, $2, id, 1, now(), gen_random_uuid(), 'time' \
             FROM people ORDER BY id LIMIT 1500",
        )
        .bind(ws.as_uuid())
        .bind(segment)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE segment_members SET exited_at = now(), exited_version = 1, \
                exited_trace_id = gen_random_uuid(), exited_cause = 'time' \
             WHERE person_id = (SELECT person_id FROM segment_members ORDER BY person_id LIMIT 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        for (source, total) in [
            (AudienceSource::Everyone, 2499),
            (AudienceSource::Segment(segment), 1499),
        ] {
            let mut all = Vec::new();
            let mut after = None;
            loop {
                let page = audience_page(&mut connection, ws, source, after, None)
                    .await
                    .unwrap();
                assert!(page.len() <= MAX_ENROLL_PEOPLE);
                after = page.last().copied();
                let last = page.len() < MAX_ENROLL_PEOPLE;
                all.extend(page);
                if last {
                    break;
                }
            }
            assert_eq!(all.len(), total, "{source:?}");
            assert!(all.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }
}
