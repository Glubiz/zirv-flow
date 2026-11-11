//! The drain's batched event path (architecture 7.1 and 12.1): a run of events applied with one
//! statement per kind of write, not one per event.
//!
//! A pass over the run resolves its events' people together ([`Planned`]): one lookup of every
//! identifier, one lock of the owners not held yet and one look again under the locks. Then each
//! event is judged in memory in order, as [`apply_event_record`] judges it, and its writes are
//! queued: the person and identities it claims, its `event_dedupe` claim, its event row and its
//! marks. They are written when the pass ends, one statement for each: the new people, the
//! identities, the dedupe claims together with the repeats counted since, the events and the
//! marks. A repeat of a message id the batch claimed is counted in that statement too.
//!
//! An event the pass cannot judge in memory goes to [`apply_event_record`] once the pass has
//! written what came before it: one the input checks or the policy refuse (its reject may name a
//! person the pass just made), one whose identifiers need a merge, a conflict or a split, or one
//! whose message id the batch did not look up. A new pass starts after it, so every event sees
//! what the events before it wrote, as with one statement each.
//!
//! A message id another transaction claimed after the batch looked it up fails the batch as a
//! retryable race, as [`apply_event_record`] does: the retry counts it as a repeat.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use sqlx::PgConnection;
use sqlx::types::Json;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::decision_actions;
use zirv_flow_core::events::{Reason, ValidationState, redact};
use zirv_flow_core::people::Identifier;
use zirv_flow_core::segment::ChangeClasses;
use zirv_flow_core::{Cause, HmacKey, TraceContext, WorkspaceId};

use super::{
    EventBatch, EventOutcome, EventRecord, Verdict, apply_event_record, checked_input, decide,
    observe,
};
use crate::changes::MarkBuffer;
use crate::people::{Planned, record_resolution};
use crate::{ForwardRef, StoreError};

/// Applies `records` in order, as [`apply_event_record`] would one after another, with a
/// statement per kind of write for each pass (see the module documentation). The caller looked
/// the records' message ids up first ([`EventBatch::prefetch_duplicates`]) and ends the batch
/// with [`EventBatch::finish`].
pub(crate) async fn apply_event_group(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &HmacKey,
    batch: &mut EventBatch,
    records: &[&EventRecord],
) -> Result<Vec<EventOutcome>, StoreError> {
    if !batch.cached_readers.contains(&workspace.as_uuid()) {
        crate::changes::cache_change_readers(connection, workspace).await?;
    }
    let mut outcomes = Vec::with_capacity(records.len());
    let mut start = 0;
    while start < records.len() {
        let rest = &records[start..];
        let sets = resolvable(connection, workspace, batch, rest).await?;
        let sets: Vec<&[Identifier]> = sets.iter().map(Vec::as_slice).collect();
        let mut plan = Planned::load(connection, workspace, &sets, &mut batch.profiles).await?;
        let mut writes = Writes::default();
        let mut single = None;
        for (offset, record) in rest.iter().enumerate() {
            let judged = judge_planned(
                connection,
                workspace,
                key,
                batch,
                &mut plan,
                &mut writes,
                record,
            )
            .await?;
            match judged {
                Some(outcome) => outcomes.push(outcome),
                None => {
                    single = Some(offset);
                    break;
                }
            }
        }
        plan.write(connection, workspace, &mut batch.profiles)
            .await?;
        writes.write(connection, workspace).await?;
        let Some(offset) = single else {
            break;
        };
        outcomes.push(apply_event_record(connection, workspace, key, batch, rest[offset]).await?);
        start += offset + 1;
    }
    Ok(outcomes)
}

/// The normalized identifiers of the records at the head of `records` whose people a pass may
/// resolve: up to the first one the input checks or the policy refuse, leaving out a message id
/// the lookup found stored.
async fn resolvable(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    batch: &mut EventBatch,
    records: &[&EventRecord],
) -> Result<Vec<Vec<Identifier>>, StoreError> {
    let mut sets = Vec::new();
    for record in records {
        let Ok(identifiers) = checked_input(record) else {
            break;
        };
        if record
            .message_id
            .as_ref()
            .is_some_and(|message_id| batch.known_duplicates.contains_key(message_id))
        {
            continue;
        }
        if let (_, Verdict::Refused { .. }) = batch.verdict(connection, workspace, record).await? {
            break;
        }
        sets.push(identifiers);
    }
    Ok(sets)
}

/// Judges `record` as [`apply_event_record`] would after the records before it, queueing its
/// writes in `writes` and its decisions, arrival and forward in `batch`. `None` when it needs
/// [`apply_event_record`]; nothing is queued for it then.
async fn judge_planned(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &HmacKey,
    batch: &mut EventBatch,
    plan: &mut Planned,
    writes: &mut Writes,
    record: &EventRecord,
) -> Result<Option<EventOutcome>, StoreError> {
    let Ok(identifiers) = checked_input(record) else {
        return Ok(None);
    };
    if let Some(message_id) = &record.message_id {
        let repeated = if let Some(original) = batch.known_duplicates.get(message_id) {
            // Counted by the lookup.
            Some(*original)
        } else if let Some(original) = batch.claimed.get(message_id) {
            writes.repeat(message_id);
            Some(*original)
        } else if batch.prefetched.contains(message_id) {
            None
        } else {
            return Ok(None);
        };
        if let Some(original_event_id) = repeated {
            let outcome = EventOutcome::Duplicate { original_event_id };
            observe(connection, workspace, key, batch, record, &outcome).await?;
            return Ok(Some(outcome));
        }
    }

    let (policy, verdict) = batch.verdict(connection, workspace, record).await?;
    let Verdict::Accepted {
        state,
        schema_version,
        reasons,
        timing,
    } = verdict
    else {
        return Ok(None);
    };
    if state == ValidationState::Unplanned {
        batch.draft(connection, workspace, record).await?;
    }
    let event_id = Uuid::now_v7();
    let Some(resolution) = plan.resolve(
        &identifiers,
        &batch.profiles,
        &record.trace,
        &mut writes.marks,
    ) else {
        return Ok(None);
    };

    let person_id = resolution.person_id;
    if resolution.created {
        decide(
            batch,
            workspace,
            record,
            &decision_actions::PERSON_CREATED,
            None,
            vec![("person", person_id)],
        );
    }
    // Without a conflict it only queues decisions.
    record_resolution(
        &mut *connection,
        &mut batch.decisions,
        workspace,
        &record.trace,
        person_id,
        &resolution,
        None,
    )
    .await?;
    if let Some(message_id) = &record.message_id {
        writes.claim(message_id, event_id, timing.occurred_at, person_id);
        batch.claimed.insert(message_id.clone(), event_id);
    }
    let mut properties = record.properties.clone();
    redact(&mut properties, &policy.redaction, workspace, key);
    let flags: Vec<&str> = timing.flags.iter().map(|flag| flag.as_str()).collect();
    writes.events.push(EventRow {
        id: event_id,
        person_id,
        name: record.name.clone(),
        schema_version,
        occurred_at: timing.occurred_at,
        received_at: record.received_at,
        properties,
        source: record.source.clone(),
        message_id: record.message_id.clone(),
        trace_id: record.trace.trace_id.as_uuid(),
        state,
        reasons,
        flags: flags.join(","),
        source_record_id: record.source_record_id,
    });
    writes.marks.push(
        person_id,
        ChangeClasses::EVENTS,
        &TraceContext {
            trace_id: record.trace.trace_id,
            cause: Cause::event(event_id),
        },
        timing.is_late().then_some(timing.occurred_at),
    );
    batch.forwards.push(ForwardRef {
        id: event_id,
        occurred_at: timing.occurred_at,
    });
    let outcome = EventOutcome::Accepted {
        event_id,
        person_id,
        state,
        flags: timing.flags,
    };
    observe(connection, workspace, key, batch, record, &outcome).await?;
    Ok(Some(outcome))
}

/// The claims of a pass, `$2` to `$6`, in workspace `$1`; the result counts those it made.
macro_rules! claimed {
    () => {
        "claimed AS ( \
             INSERT INTO event_dedupe \
                (workspace_id, message_id, event_id, event_occurred_at, person_id, duplicates) \
             SELECT $1, c.message_id, c.event_id, c.occurred_at, c.person_id, c.repeats \
             FROM unnest($2::text[], $3::uuid[], $4::timestamptz[], $5::uuid[], $6::int4[]) \
                 AS c(message_id, event_id, occurred_at, person_id, repeats) \
             ORDER BY c.message_id \
             ON CONFLICT (workspace_id, message_id) DO NOTHING RETURNING 1) \
         SELECT count(*) FROM claimed"
    };
}

/// A pass's claims, with no repeat of an earlier pass's claim to count. No join with the counted
/// ids then: with statistics from before the workspace had dedupe rows, PostgreSQL plans that
/// join, even of no ids, as a walk over every dedupe row of the workspace (round 7e1's bench
/// measured 6,759 buffer hits for one id).
const CLAIM: &str = concat!("WITH ", claimed!());

/// [`CLAIM`], and the repeats `$8` of the ids `$7` that earlier passes of the batch claimed.
const CLAIM_AND_COUNT: &str = concat!(
    "WITH counted AS ( \
         UPDATE event_dedupe d SET duplicates = d.duplicates + v.repeats \
         FROM unnest($7::text[], $8::int4[]) AS v(message_id, repeats) \
         WHERE d.workspace_id = $1 AND d.message_id = v.message_id RETURNING 1), ",
    claimed!()
);

/// An event row to insert, as [`super::insert_event`] writes it.
struct EventRow {
    id: Uuid,
    person_id: Uuid,
    name: String,
    schema_version: Option<i32>,
    occurred_at: OffsetDateTime,
    received_at: OffsetDateTime,
    /// Already redacted.
    properties: Map<String, Value>,
    source: String,
    message_id: Option<String>,
    trace_id: Uuid,
    state: ValidationState,
    reasons: Vec<Reason>,
    /// The flags' codes, comma-separated.
    flags: String,
    source_record_id: Option<Uuid>,
}

/// The claim of a message id in a pass, for the event it stores.
struct Claim {
    event_id: Uuid,
    occurred_at: OffsetDateTime,
    person_id: Uuid,
    /// Repeats of it later in the same pass.
    repeats: i32,
}

/// What a pass writes when it ends.
#[derive(Default)]
struct Writes {
    /// The claims by message id, in the order they are locked.
    claims: BTreeMap<String, Claim>,
    /// Repeats of message ids the batch claimed before this pass, by id.
    repeats: BTreeMap<String, i32>,
    events: Vec<EventRow>,
    marks: MarkBuffer,
}

impl Writes {
    fn claim(
        &mut self,
        message_id: &str,
        event_id: Uuid,
        occurred_at: OffsetDateTime,
        person_id: Uuid,
    ) {
        let claim = Claim {
            event_id,
            occurred_at,
            person_id,
            repeats: 0,
        };
        self.claims.insert(message_id.to_owned(), claim);
    }

    /// Counts a repeat of a message id the batch claimed.
    fn repeat(&mut self, message_id: &str) {
        match self.claims.get_mut(message_id) {
            Some(claim) => claim.repeats += 1,
            None => *self.repeats.entry(message_id.to_owned()).or_default() += 1,
        }
    }

    /// Writes the claims and the repeats in one statement, then the events and the marks in one
    /// each.
    async fn write(
        mut self,
        connection: &mut PgConnection,
        workspace: WorkspaceId,
    ) -> Result<(), StoreError> {
        self.write_dedupe(connection, workspace).await?;
        self.write_events(connection, workspace).await?;
        self.marks.flush(connection, workspace).await
    }

    /// Claims the new message ids, each with the repeats that followed it in the pass, and adds
    /// the pass's repeats of the ids claimed earlier in the batch. In message id order, so two
    /// batches claiming the same ids take them in one order. A claim another transaction made
    /// first fails as a retryable race.
    async fn write_dedupe(
        &self,
        connection: &mut PgConnection,
        workspace: WorkspaceId,
    ) -> Result<(), StoreError> {
        if self.claims.is_empty() && self.repeats.is_empty() {
            return Ok(());
        }
        let ids: Vec<&str> = self.claims.keys().map(String::as_str).collect();
        let claims = || self.claims.values();
        let events: Vec<Uuid> = claims().map(|c| c.event_id).collect();
        let occurred: Vec<OffsetDateTime> = claims().map(|c| c.occurred_at).collect();
        let people: Vec<Uuid> = claims().map(|c| c.person_id).collect();
        let repeats: Vec<i32> = claims().map(|c| c.repeats).collect();
        let (again, counts): (Vec<&str>, Vec<i32>) = self
            .repeats
            .iter()
            .map(|(id, count)| (id.as_str(), *count))
            .unzip();
        let counted = !again.is_empty();
        let mut query = sqlx::query_scalar(if counted { CLAIM_AND_COUNT } else { CLAIM })
            .bind(workspace.as_uuid())
            .bind(&ids)
            .bind(&events)
            .bind(&occurred)
            .bind(&people)
            .bind(&repeats);
        if counted {
            query = query.bind(&again).bind(&counts);
        }
        let claimed: i64 = query
            .fetch_one(&mut *connection)
            .await
            .map_err(StoreError::query)?;
        // Another transaction committed one of the ids after the batch looked it up: the retry
        // counts the repeat, and the rollback takes this pass's people with it.
        if usize::try_from(claimed).unwrap_or_default() < self.claims.len() {
            return Err(StoreError::retryable_race());
        }
        Ok(())
    }

    async fn write_events(
        &mut self,
        connection: &mut PgConnection,
        workspace: WorkspaceId,
    ) -> Result<(), StoreError> {
        if self.events.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.events);
        let mut ids = Vec::with_capacity(rows.len());
        let mut people = Vec::with_capacity(rows.len());
        let mut names = Vec::with_capacity(rows.len());
        let mut versions = Vec::with_capacity(rows.len());
        let mut occurred = Vec::with_capacity(rows.len());
        let mut received = Vec::with_capacity(rows.len());
        let mut properties = Vec::with_capacity(rows.len());
        let mut sources = Vec::with_capacity(rows.len());
        let mut message_ids = Vec::with_capacity(rows.len());
        let mut traces = Vec::with_capacity(rows.len());
        let mut states = Vec::with_capacity(rows.len());
        let mut reasons = Vec::with_capacity(rows.len());
        let mut flags = Vec::with_capacity(rows.len());
        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            ids.push(row.id);
            people.push(row.person_id);
            names.push(row.name);
            versions.push(row.schema_version);
            occurred.push(row.occurred_at);
            received.push(row.received_at);
            properties.push(Json(row.properties));
            sources.push(row.source);
            message_ids.push(row.message_id);
            traces.push(row.trace_id);
            states.push(row.state.as_str());
            reasons.push(Json(row.reasons));
            flags.push(row.flags);
            records.push(row.source_record_id);
        }
        sqlx::query(
            "INSERT INTO events \
                (id, workspace_id, person_id, name, schema_version, occurred_at, received_at, \
                 properties, source, message_id, trace_id, validation_state, validation_reasons, \
                 flags, source_record_id) \
             SELECT e.id, $1, e.person_id, e.name, e.schema_version, e.occurred_at, e.received_at, \
                 e.properties, e.source, e.message_id, e.trace_id, e.state, e.reasons, \
                 string_to_array(e.flags, ','), e.source_record_id \
             FROM unnest($2::uuid[], $3::uuid[], $4::text[], $5::int4[], $6::timestamptz[], \
                         $7::timestamptz[], $8::jsonb[], $9::text[], $10::text[], $11::uuid[], \
                         $12::text[], $13::jsonb[], $14::text[], $15::uuid[]) \
                 AS e(id, person_id, name, schema_version, occurred_at, received_at, properties, \
                      source, message_id, trace_id, state, reasons, flags, source_record_id)",
        )
        .bind(workspace.as_uuid())
        .bind(&ids)
        .bind(&people)
        .bind(&names)
        .bind(&versions)
        .bind(&occurred)
        .bind(&received)
        .bind(&properties)
        .bind(&sources)
        .bind(&message_ids)
        .bind(&traces)
        .bind(&states)
        .bind(&reasons)
        .bind(&flags)
        .bind(&records)
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{HmacPurpose, HmacRoot, MasterKey, Slug, WorkspaceName};

    use super::super::{apply_event_batch, apply_event_records};
    use super::*;
    use crate::{begin, commit, create_workspace};

    #[test]
    fn a_pass_claims_in_message_id_order_and_counts_each_repeat_with_its_claim() {
        let mut writes = Writes::default();
        let (at, event, person) = (
            datetime!(2026-06-01 12:00 UTC),
            Uuid::from_u128(1),
            Uuid::from_u128(2),
        );
        writes.claim("m-2", event, at, person);
        writes.claim("m-1", event, at, person);
        writes.repeat("m-2");
        writes.repeat("m-2");
        // An id an earlier pass of the batch claimed.
        writes.repeat("m-0");

        let claims: Vec<(&str, i32)> = writes
            .claims
            .iter()
            .map(|(id, claim)| (id.as_str(), claim.repeats))
            .collect();
        assert_eq!(claims, [("m-1", 0), ("m-2", 2)]);
        let earlier: Vec<(&str, i32)> = writes
            .repeats
            .iter()
            .map(|(id, count)| (id.as_str(), *count))
            .collect();
        assert_eq!(earlier, [("m-0", 1)]);
    }

    fn key() -> HmacKey {
        HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction)
    }

    fn page(identifiers: &[(&str, &str)], message_id: &str) -> EventRecord {
        EventRecord {
            source_record_id: None,
            source: "web".to_owned(),
            identifiers: identifiers
                .iter()
                .map(|(kind, value)| ((*kind).to_owned(), (*value).to_owned()))
                .collect(),
            name: "page".to_owned(),
            properties: Map::new(),
            timestamp: None,
            received_at: OffsetDateTime::now_utc(),
            message_id: Some(message_id.to_owned()),
            trace: TraceContext::root(Cause::api("test")),
        }
    }

    /// `records` as one run through [`apply_event_group`], in a transaction as the drain's.
    async fn apply_run(
        pool: &PgPool,
        ws: WorkspaceId,
        key: &HmacKey,
        records: &[EventRecord],
    ) -> Vec<EventOutcome> {
        let mut tx = begin(pool).await.unwrap();
        crate::changes::cache_change_readers(&mut tx, ws)
            .await
            .unwrap();
        let mut batch = EventBatch::default();
        batch.readers_cached(ws);
        batch
            .prefetch_duplicates(&mut tx, ws, records)
            .await
            .unwrap();
        let run: Vec<&EventRecord> = records.iter().collect();
        let outcomes = apply_event_group(&mut tx, ws, key, &mut batch, &run)
            .await
            .unwrap();
        batch.finish(&mut tx, ws).await.unwrap();
        commit(tx).await.unwrap();
        outcomes
    }

    fn label(outcome: &EventOutcome) -> &'static str {
        match outcome {
            EventOutcome::Accepted { state, .. } => state.as_str(),
            EventOutcome::Duplicate { .. } => "duplicate",
            EventOutcome::Rejected { reason, .. } => reason,
        }
    }

    /// What `ws` holds, ids aside: each event by message id with its state and its person's
    /// identifiers, each dedupe row by message id, and the identifiers of each person.
    async fn stored(pool: &PgPool, ws: WorkspaceId) -> Vec<Vec<String>> {
        let mut state = Vec::new();
        for sql in [
            "SELECT e.message_id || ' ' || e.validation_state || ' ' || \
                 (SELECT string_agg(i.kind || ':' || i.value, ',' ORDER BY i.kind, i.value) \
                  FROM people p JOIN identities i ON i.person_id = COALESCE(p.merged_into, p.id) \
                  WHERE p.id = e.person_id) \
             FROM events e WHERE e.workspace_id = $1 ORDER BY 1",
            "SELECT message_id || ' ' || duplicates FROM event_dedupe \
             WHERE workspace_id = $1 ORDER BY 1",
            "SELECT string_agg(kind || ':' || value, ',' ORDER BY kind, value) FROM identities \
             WHERE workspace_id = $1 GROUP BY person_id ORDER BY 1",
        ] {
            let rows: Vec<String> = sqlx::query_scalar(sql)
                .bind(ws.as_uuid())
                .fetch_all(pool)
                .await
                .unwrap();
            state.push(rows);
        }
        state
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_run_ends_as_its_events_applied_one_by_one(pool: PgPool) {
        let key = key();
        let seed = [
            page(&[("external:user_id", "u1")], "s-1"),
            page(&[("anonymous_id", "a2")], "s-2"),
        ];
        let run = [
            // A known person, a new one and the new one again.
            page(&[("external:user_id", "u1")], "m-1"),
            page(&[("anonymous_id", "a1")], "m-2"),
            page(&[("anonymous_id", "a1")], "m-3"),
            // A repeat inside the pass and one of a stored event.
            page(&[("external:user_id", "u1")], "m-1"),
            page(&[("external:user_id", "u1")], "s-1"),
            // The new person gaining an identifier.
            page(
                &[("anonymous_id", "a1"), ("email", "a1@example.com")],
                "m-4",
            ),
            // Refused by the input checks: one by one, and a new pass after it.
            page(&[("email", "not-an-email")], "m-5"),
            // A repeat of an id the first pass claimed.
            page(&[("anonymous_id", "a1")], "m-2"),
            // Two people's identifiers (a merge, one by one), then a new person.
            page(&[("external:user_id", "u1"), ("anonymous_id", "a2")], "m-6"),
            page(&[("anonymous_id", "a3")], "m-7"),
        ];
        let mut ended = Vec::new();
        for slug in ["grouped", "singly"] {
            let name = WorkspaceName::parse(slug).unwrap();
            let ws = create_workspace(&pool, &name, &Slug::parse(slug).unwrap())
                .await
                .unwrap()
                .id;
            apply_event_batch(&pool, ws, &key, &seed).await.unwrap();
            let outcomes = if slug == "grouped" {
                apply_run(&pool, ws, &key, &run).await
            } else {
                let mut tx = begin(&pool).await.unwrap();
                let outcomes = apply_event_records(&mut tx, ws, &key, &run).await.unwrap();
                commit(tx).await.unwrap();
                outcomes
            };
            let labels: Vec<&str> = outcomes.iter().map(label).collect();
            ended.push((labels, stored(&pool, ws).await));
        }

        let (grouped, singly) = (&ended[0], &ended[1]);
        assert_eq!(grouped, singly);
        let duplicates = grouped.0.iter().filter(|label| **label == "duplicate");
        assert_eq!(duplicates.count(), 3, "{:?}", grouped.0);
        assert_eq!(grouped.1[1].len(), 8, "a dedupe row per stored event");
    }
}
