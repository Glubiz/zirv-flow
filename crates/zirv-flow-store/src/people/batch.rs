//! What the records one transaction applies share (architecture 7.1).
//!
//! A [`ProfileBatch`] keeps, for the records and events of one batch:
//!
//! - whether the segment guard and the shared slot lock are held, so a record does not take them
//!   again;
//! - the trait definitions read so far ([`Definitions`]), each read once;
//! - the people the transaction holds `FOR UPDATE`, so a record whose people are all held already
//!   neither locks them again nor looks their identities up a second time: nobody else can move
//!   or delete an identity of a person this transaction holds, and this transaction's own merges
//!   update what the batch remembers (it deletes no person: a new one's row is inserted only
//!   once its identities are claimed);
//! - the trait recomputes its records owe. [`ProfileBatch::finish`] runs them for the whole batch
//!   at once: one statement locks and reads the people, one reads their ledger contributions for
//!   the keys that changed, one reads the definitions the batch has not read, and one writes every
//!   person whose value or source changed ([`recompute_people`]).
//!
//! The people stay locked in the order the records lock them; the recompute only reads and
//! writes rows the batch already holds (a person it does not hold yet, as in `traits.recompute`,
//! is locked by the same statement in id order).
//!
//! A batch serves one workspace and one transaction, and never outlives a savepoint that is rolled
//! back: the rollback releases the locks and undoes the writes the batch remembers. A path that
//! applies records in savepoints gives each record a batch of its own.

use std::collections::{BTreeMap, BTreeSet};

use hashbrown::HashMap;
use serde_json::{Map, Value};
use sqlx::PgConnection;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::decision_actions;
use zirv_flow_core::people::{Contribution, Precedence, resolve};
use zirv_flow_core::segment::ChangeClasses;
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::apply::same_value;
use super::lock::{LockedRow, UPDATE_WITH_TRAITS};
use super::registry::Definitions;
use super::slots::{lock_and_read_writer_slots, lock_slots_shared, slot_assignments_sql};
use crate::changes::mark_people;
use crate::decisions::DecisionBuffer;
use crate::{NewDecision, StoreError};

/// A person this transaction holds `FOR UPDATE`: the person it was merged into and when it was
/// created, as locked or as this transaction has changed them since.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Held {
    pub(super) merged_into: Option<Uuid>,
    pub(super) created_at: OffsetDateTime,
}

/// The people a batch holds `FOR UPDATE`, handed from one [`ProfileBatch`] to the next batch of
/// the same transaction ([`ProfileBatch::adopt`]).
pub(crate) struct HeldPeople(HashMap<Uuid, Held>);

/// A recompute a record owes: `keys` of `person`.
pub(super) struct Owed {
    pub(super) person: Uuid,
    pub(super) keys: Vec<String>,
    pub(super) record_id: Uuid,
    pub(super) trace: TraceContext,
    /// The record moved away from this person; otherwise this is the record's own person.
    pub(super) moved_from: bool,
    /// The record made the person, which is marked dirty whether or not a value changed.
    pub(super) created: bool,
    /// The id of the record's `trait.change` decision, taken when the record was applied, so the
    /// decision keeps its place among the record's others.
    pub(super) decision: Uuid,
}

/// See the module documentation.
#[derive(Default)]
pub(crate) struct ProfileBatch {
    guarded: bool,
    pub(super) definitions: Definitions,
    held: HashMap<Uuid, Held>,
    owed: Vec<Owed>,
}

impl ProfileBatch {
    /// A batch whose caller took the segment guard (`cache_change_readers`) and the shared slot
    /// lock itself, outside any savepoint.
    pub(crate) fn guarded() -> Self {
        Self {
            guarded: true,
            ..Self::default()
        }
    }

    /// Takes the segment guard and then the shared slot lock, unless the batch holds them: after
    /// the guard and before any row lock, as `slots` orders them.
    pub(crate) async fn guard(
        &mut self,
        connection: &mut PgConnection,
        workspace: WorkspaceId,
    ) -> Result<(), StoreError> {
        if self.guarded {
            return Ok(());
        }
        crate::changes::cache_change_readers(connection, workspace).await?;
        lock_slots_shared(connection, workspace).await?;
        self.guarded = true;
        Ok(())
    }

    /// What the batch holds, to hand on when it is done.
    pub(crate) fn into_held(self) -> HeldPeople {
        HeldPeople(self.held)
    }

    /// Takes over people an earlier batch of this transaction held. They stay locked until the
    /// transaction ends, and nothing else can move or delete an identity of theirs, so the
    /// batch resolves them on the held path.
    pub(crate) fn adopt(&mut self, held: HeldPeople) {
        self.held.extend(held.0);
    }

    pub(super) fn held(&self, person: Uuid) -> Option<Held> {
        self.held.get(&person).copied()
    }

    pub(super) fn hold(&mut self, person: Uuid, held: Held) {
        self.held.insert(person, held);
    }

    /// Remembers rows this transaction locked `FOR UPDATE`.
    pub(super) fn hold_rows(&mut self, rows: impl IntoIterator<Item = LockedRow>) {
        for (person, merged_into, created_at) in rows {
            self.hold(
                person,
                Held {
                    merged_into,
                    created_at,
                },
            );
        }
    }

    /// This transaction merged `losers` into `survivor`: they, and the people merged into them,
    /// now point at the survivor.
    pub(super) fn merged(&mut self, survivor: Uuid, losers: &[Uuid]) {
        for (person, held) in &mut self.held {
            if losers.contains(person) || held.merged_into.is_some_and(|to| losers.contains(&to)) {
                held.merged_into = Some(survivor);
            }
        }
    }

    pub(super) fn owe(&mut self, owed: Owed) {
        self.owed.push(owed);
    }

    /// Runs the recomputes the batch's records owe, all together ([`recompute_people`]), marks
    /// dirty each person a value of whose record's keys changed (and each person a record
    /// created) and queues a `trait.change` decision for each such record. Returns, for each
    /// applied record in the order it was applied, whether a value of its keys changed: what its
    /// outcome reports as `changed`. Several records of one person in a batch are judged by what
    /// the batch did, from the person as the batch found it to the person as it leaves it.
    pub(crate) async fn finish(
        &mut self,
        connection: &mut PgConnection,
        workspace: WorkspaceId,
        decisions: &mut DecisionBuffer,
    ) -> Result<Vec<bool>, StoreError> {
        let owed = std::mem::take(&mut self.owed);
        if owed.is_empty() {
            return Ok(Vec::new());
        }
        let mut wanted: BTreeMap<Uuid, BTreeSet<String>> = BTreeMap::new();
        for entry in &owed {
            wanted
                .entry(entry.person)
                .or_default()
                .extend(entry.keys.iter().cloned());
        }
        let changed =
            recompute_people(connection, workspace, &wanted, &mut self.definitions).await?;
        let mut applied = Vec::with_capacity(owed.len());
        let mut marks: Vec<(&TraceContext, Uuid)> = Vec::new();
        for entry in &owed {
            let value_changed = changed
                .get(&entry.person)
                .is_some_and(|keys| entry.keys.iter().any(|key| keys.contains(key)));
            if value_changed || (entry.created && !entry.moved_from) {
                marks.push((&entry.trace, entry.person));
            }
            if value_changed {
                decisions.push_as(
                    entry.decision,
                    NewDecision {
                        graph: None,
                        workspace_id: workspace,
                        trace: entry.trace.clone(),
                        spec: &decision_actions::TRAIT_CHANGED,
                        detail: None,
                        subject_ids: vec![
                            ("person", entry.person),
                            ("source_record", entry.record_id),
                        ],
                        job: None,
                    },
                );
            }
            if !entry.moved_from {
                applied.push(value_changed);
            }
        }
        // One statement per run of records on one trace, in the records' order, so the latest
        // change's trace is the one a mark keeps.
        let mut start = 0;
        while start < marks.len() {
            let trace = marks[start].0;
            let end = marks[start..]
                .iter()
                .position(|(other, _)| *other != trace)
                .map_or(marks.len(), |offset| start + offset);
            let people: Vec<Uuid> = marks[start..end]
                .iter()
                .map(|(_, person)| *person)
                .collect();
            mark_people(
                connection,
                workspace,
                &people,
                ChangeClasses::TRAITS,
                trace,
                None,
            )
            .await?;
            start = end;
        }
        Ok(applied)
    }
}

/// A person's contributions to the keys being recomputed, by key.
type Contributions = HashMap<String, Vec<Contribution<Value>>>;

/// Recomputes, from the live ledger rows, the keys `wanted` names of each person, and writes every
/// person a value or a source of whose keys changed in one statement. One statement locks and
/// reads the people, in id order; one reads the contributions to those keys only; the definitions
/// come from `definitions`, which reads the keys it lacks in one more. Returns, by person, the
/// keys whose value changed; a person who does not exist is left out.
pub(super) async fn recompute_people(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    wanted: &BTreeMap<Uuid, BTreeSet<String>>,
    definitions: &mut Definitions,
) -> Result<HashMap<Uuid, BTreeSet<String>>, StoreError> {
    let mut changed = HashMap::new();
    if wanted.is_empty() {
        return Ok(changed);
    }
    let ids: Vec<Uuid> = wanted.keys().copied().collect();
    let people: Vec<(Uuid, Value, Value)> = sqlx::query_as(UPDATE_WITH_TRAITS)
        .bind(workspace.as_uuid())
        .bind(&ids)
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    if people.is_empty() {
        return Ok(changed);
    }
    let present: Vec<Uuid> = people.iter().map(|(id, ..)| *id).collect();
    let (mut pair_people, mut pair_keys): (Vec<Uuid>, Vec<&str>) = (Vec::new(), Vec::new());
    for person in &present {
        for key in &wanted[person] {
            pair_people.push(*person);
            pair_keys.push(key);
        }
    }
    definitions
        .load(&mut *connection, workspace, pair_keys.iter().copied())
        .await?;
    let found: Vec<(Uuid, String, Uuid, String, Value)> = sqlx::query_as(
        "SELECT s.person_id, w.key, s.id, s.source, s.traits -> w.key \
         FROM source_records s \
         JOIN unnest($3::uuid[], $4::text[]) AS w(person_id, key) ON w.person_id = s.person_id \
         WHERE s.workspace_id = $1 AND s.person_id = ANY($2) AND s.deleted_at IS NULL \
           AND s.traits ? w.key",
    )
    .bind(workspace.as_uuid())
    .bind(&present)
    .bind(&pair_people)
    .bind(&pair_keys)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut contributions: HashMap<Uuid, Contributions> = HashMap::new();
    for (person, key, record_id, source, entry) in found {
        let (Some(value), Some(at)) = (entry.get("v"), entry.get("at").and_then(Value::as_str))
        else {
            return Err(decode_error("malformed ledger trait"));
        };
        let at = OffsetDateTime::parse(at, &Rfc3339)
            .map_err(|_| decode_error("malformed ledger time"))?;
        contributions
            .entry(person)
            .or_default()
            .entry(key)
            .or_default()
            .push(Contribution {
                source,
                at,
                record_id,
                value: value.clone(),
            });
    }

    let none = Contributions::new();
    let (mut update_ids, mut update_traits, mut update_sources) =
        (Vec::new(), Vec::new(), Vec::new());
    for (person, traits, sources) in people {
        let mut traits = traits.as_object().cloned().unwrap_or_default();
        let mut sources = sources.as_object().cloned().unwrap_or_default();
        let resolved = resolve_keys(
            &mut traits,
            &mut sources,
            &wanted[&person],
            definitions,
            contributions.get(&person).unwrap_or(&none),
        );
        if resolved.values.is_empty() && !resolved.sources {
            continue;
        }
        if !resolved.values.is_empty() {
            changed.insert(person, resolved.values);
        }
        update_ids.push(person);
        update_traits.push(Value::Object(traits));
        update_sources.push(Value::Object(sources));
    }
    if update_ids.is_empty() {
        return Ok(changed);
    }
    // The writer lock comes in a statement of its own, before the read of the assignments, and
    // the slots are written by the statement that writes `traits`.
    let slots = lock_and_read_writer_slots(connection, workspace).await?;
    let sql = format!(
        "UPDATE people SET traits = u.traits, trait_sources = u.sources, updated_at = now(){} \
         FROM unnest($2::uuid[], $3::jsonb[], $4::jsonb[]) AS u(id, traits, sources) \
         WHERE people.workspace_id = $1 AND people.id = u.id",
        slot_assignments_sql(&slots, "u.traits", 5)
    );
    let mut update = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(workspace.as_uuid())
        .bind(&update_ids)
        .bind(&update_traits)
        .bind(&update_sources);
    for (key, _) in &slots {
        update = update.bind(key.as_str());
    }
    update
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(changed)
}

/// What [`resolve_keys`] changed: the keys whose value changed, and whether a source did.
#[derive(Debug, Default)]
struct Resolved {
    values: BTreeSet<String>,
    sources: bool,
}

/// Sets each of `keys` in `traits` and `sources` to the winner of its contributions by the key's
/// precedence (`latest` for a key without a definition). An archived key is not materialized: its
/// value and source go, while the ledger keeps the contributions for an unarchive.
fn resolve_keys(
    traits: &mut Map<String, Value>,
    sources: &mut Map<String, Value>,
    keys: &BTreeSet<String>,
    definitions: &Definitions,
    contributions: &Contributions,
) -> Resolved {
    let mut resolved = Resolved::default();
    for key in keys {
        let definition = definitions.get(key);
        let found: &[Contribution<Value>] = if definition.is_some_and(|d| d.archived) {
            &[]
        } else {
            contributions.get(key).map_or(&[], Vec::as_slice)
        };
        let (precedence, order) = definition.map_or((Precedence::Latest, &[][..]), |d| {
            (d.precedence, d.source_order.as_slice())
        });
        match resolve(precedence, order, found) {
            Some(winner) => {
                if !traits
                    .get(key)
                    .is_some_and(|current| same_value(current, &winner.value))
                {
                    traits.insert(key.clone(), winner.value.clone());
                    resolved.values.insert(key.clone());
                }
                let source = Value::String(winner.source.clone());
                if sources.get(key) != Some(&source) {
                    sources.insert(key.clone(), source);
                    resolved.sources = true;
                }
            }
            None => {
                if traits.remove(key).is_some() {
                    resolved.values.insert(key.clone());
                }
                resolved.sources |= sources.remove(key).is_some();
            }
        }
    }
    resolved
}

fn decode_error(what: &'static str) -> StoreError {
    StoreError::query(sqlx::Error::Decode(what.into()))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::future::Future;
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use tracing::instrument::WithSubscriber;
    use zirv_flow_core::people::TraitType;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::people::registry::{
        NewTraitDefinition, TraitDefinition, TraitDefinitionUpdate, create_trait_definition,
        find_trait_definition, update_trait_definition,
    };
    use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_batch};
    use crate::{begin, commit, create_workspace};

    /// Collects the text of every statement sqlx runs.
    struct Statements(Arc<Mutex<Vec<String>>>);

    #[derive(Default)]
    struct Text {
        summary: String,
        statement: String,
    }

    impl tracing::field::Visit for Text {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            match field.name() {
                "summary" => value.clone_into(&mut self.summary),
                "db.statement" => value.trim().clone_into(&mut self.statement),
                _ => {}
            }
        }

        fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
    }

    impl tracing::Subscriber for Statements {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            metadata.target() == "sqlx::query"
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            if event.metadata().target() != "sqlx::query" {
                return;
            }
            let mut text = Text::default();
            event.record(&mut text);
            let sql = if text.statement.is_empty() {
                text.summary
            } else {
                text.statement
            };
            self.0.lock().unwrap().push(sql);
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// What `run` returns, with the statements it ran.
    pub(crate) async fn statements_of<F: Future>(run: F) -> (F::Output, Vec<String>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let output = run
            .with_subscriber(tracing::Dispatch::new(Statements(seen.clone())))
            .await;
        let statements = std::mem::take(&mut *seen.lock().unwrap());
        (output, statements)
    }

    fn count(statements: &[String], needle: &str) -> usize {
        statements.iter().filter(|sql| sql.contains(needle)).count()
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn define(
        pool: &PgPool,
        ws: WorkspaceId,
        key: &str,
        precedence: Precedence,
        order: &[&str],
    ) {
        create_trait_definition(
            pool,
            ws,
            &NewTraitDefinition {
                key: key.to_owned(),
                trait_type: TraitType::String,
                precedence,
                source_order: order.iter().map(|source| (*source).to_owned()).collect(),
                pii: false,
                channel_address: false,
                inferred: false,
                description: String::new(),
            },
        )
        .await
        .unwrap();
    }

    fn record(source: &str, key: &str, emails: &[&str], traits: Value, day: i64) -> ProfileRecord {
        ProfileRecord {
            source: source.to_owned(),
            stream: "contacts".to_owned(),
            record_key: key.to_owned(),
            identifiers: emails
                .iter()
                .map(|email| ("email".to_owned(), (*email).to_owned()))
                .collect(),
            traits: traits.as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC) + time::Duration::days(day),
            trace: TraceContext::root(Cause::api("test")),
        }
    }

    /// Six people's records from three sources that disagree: source priority, first and latest
    /// precedence, a tie on the observed time (broken by the ledger row), a value removed, an
    /// archived key, a record that moves to another person, and two people a third record merges.
    fn conflicting() -> Vec<ProfileRecord> {
        vec![
            record(
                "crm",
                "a",
                &["a@x.example"],
                json!({"tier": "gold", "plan": "basic", "city": "Oslo", "old": "1"}),
                1,
            ),
            record(
                "shop",
                "a",
                &["a@x.example"],
                json!({"tier": "silver", "plan": "pro", "city": "Bergen"}),
                3,
            ),
            record(
                "web",
                "a",
                &["a@x.example"],
                json!({"tier": "bronze", "city": "Aarhus"}),
                3,
            ),
            record(
                "shop",
                "b",
                &["b@x.example"],
                json!({"tier": "silver", "plan": "pro"}),
                2,
            ),
            record(
                "crm",
                "b",
                &["b@x.example"],
                json!({"tier": "gold", "plan": "enterprise"}),
                4,
            ),
            record("crm", "b", &["b@x.example"], json!({"plan": null}), 5),
            record(
                "web",
                "moving",
                &["c@x.example"],
                json!({"city": "Paris", "tier": "bronze"}),
                1,
            ),
            record("crm", "c", &["c@x.example"], json!({"tier": "gold"}), 2),
            record("crm", "d", &["d@x.example"], json!({"city": "Rome"}), 2),
            record(
                "web",
                "moving",
                &["d@x.example"],
                json!({"city": "Lyon", "tier": "silver"}),
                6,
            ),
            record(
                "shop",
                "e",
                &["e@x.example"],
                json!({"city": "Turin", "plan": "pro"}),
                1,
            ),
            record(
                "web",
                "f",
                &["f@x.example"],
                json!({"city": "Milan", "plan": "basic"}),
                2,
            ),
            record(
                "crm",
                "ef",
                &["e@x.example", "f@x.example"],
                json!({"tier": "gold"}),
                3,
            ),
        ]
    }

    async fn rules(pool: &PgPool, ws: WorkspaceId) {
        define(
            pool,
            ws,
            "tier",
            Precedence::SourcePriority,
            &["crm", "shop"],
        )
        .await;
        define(pool, ws, "plan", Precedence::First, &[]).await;
        define(pool, ws, "old", Precedence::Latest, &[]).await;
        update_trait_definition(
            pool,
            ws,
            "old",
            &TraitDefinitionUpdate {
                archived: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    /// The traits and sources of the person who now holds `email`.
    async fn person_of(pool: &PgPool, ws: WorkspaceId, email: &str) -> (Uuid, Value, Value) {
        sqlx::query_as(
            "SELECT p.id, p.traits, p.trait_sources FROM identities i \
             JOIN people p ON p.id = i.person_id \
             WHERE i.workspace_id = $1 AND i.kind = 'email' AND i.value = $2",
        )
        .bind(ws.as_uuid())
        .bind(email)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// The recompute as it ran before batches: one person at a time, every definition read per
    /// key. The reference the set-based recompute has to agree with.
    async fn per_person_recompute(
        connection: &mut PgConnection,
        workspace: WorkspaceId,
        person_id: Uuid,
        keys: &[String],
    ) -> bool {
        let (traits, sources): (Value, Value) = sqlx::query_as(
            "SELECT traits, trait_sources FROM people WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(workspace.as_uuid())
        .bind(person_id)
        .fetch_one(&mut *connection)
        .await
        .unwrap();
        let (mut traits, mut sources) = (
            traits.as_object().cloned().unwrap_or_default(),
            sources.as_object().cloned().unwrap_or_default(),
        );
        let rows: Vec<(Uuid, String, Value)> = sqlx::query_as(
            "SELECT id, source, traits FROM source_records \
             WHERE workspace_id = $1 AND person_id = $2 AND deleted_at IS NULL",
        )
        .bind(workspace.as_uuid())
        .bind(person_id)
        .fetch_all(&mut *connection)
        .await
        .unwrap();
        let (mut values_changed, mut sources_changed) = (false, false);
        for key in keys {
            let definition: Option<TraitDefinition> =
                find_trait_definition(&mut *connection, workspace, key)
                    .await
                    .unwrap();
            let archived = definition.as_ref().is_some_and(|d| d.archived);
            let mut contributions = Vec::new();
            for (record_id, source, record_traits) in rows.iter().filter(|_| !archived) {
                let Some(entry) = record_traits.get(key) else {
                    continue;
                };
                contributions.push(Contribution {
                    source: source.clone(),
                    at: OffsetDateTime::parse(entry["at"].as_str().unwrap(), &Rfc3339).unwrap(),
                    record_id: *record_id,
                    value: entry["v"].clone(),
                });
            }
            let (precedence, order) = definition.map_or_else(
                || (Precedence::Latest, Vec::new()),
                |d| (d.precedence, d.source_order),
            );
            match resolve(precedence, &order, &contributions) {
                Some(winner) => {
                    if !traits
                        .get(key)
                        .is_some_and(|current| same_value(current, &winner.value))
                    {
                        traits.insert(key.clone(), winner.value.clone());
                        values_changed = true;
                    }
                    let source = Value::String(winner.source.clone());
                    if sources.get(key) != Some(&source) {
                        sources.insert(key.clone(), source);
                        sources_changed = true;
                    }
                }
                None => {
                    values_changed |= traits.remove(key).is_some();
                    sources_changed |= sources.remove(key).is_some();
                }
            }
        }
        values_changed || sources_changed
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_leaves_its_people_as_one_record_at_a_time_and_the_per_person_recompute_do(
        pool: PgPool,
    ) {
        let (batched, single) = (
            workspace(&pool, "batched").await,
            workspace(&pool, "single").await,
        );
        rules(&pool, batched).await;
        rules(&pool, single).await;
        let records = conflicting();

        let outcomes = apply_profile_batch(&pool, batched, &records).await.unwrap();
        for record in &records {
            apply_profile_batch(&pool, single, std::slice::from_ref(record))
                .await
                .unwrap();
        }

        assert!(
            outcomes
                .iter()
                .all(|outcome| matches!(outcome, ProfileOutcome::Applied { .. }))
        );
        let emails = ["a", "b", "c", "d", "e", "f"].map(|name| format!("{name}@x.example"));
        for email in &emails {
            let (_, traits, sources) = person_of(&pool, batched, email).await;
            let (_, expected_traits, expected_sources) = person_of(&pool, single, email).await;
            assert_eq!(
                (&traits, &sources),
                (&expected_traits, &expected_sources),
                "{email}"
            );
        }
        let (_, a, a_sources) = person_of(&pool, batched, "a@x.example").await;
        assert_eq!(
            a,
            json!({"tier": "gold", "plan": "basic", "city": "Aarhus"})
        );
        assert_eq!(
            a_sources,
            json!({"tier": "crm", "plan": "crm", "city": "web"})
        );
        let (_, b, _) = person_of(&pool, batched, "b@x.example").await;
        assert_eq!(b, json!({"tier": "gold", "plan": "pro"}));
        let (_, c, _) = person_of(&pool, batched, "c@x.example").await;
        assert_eq!(c, json!({"tier": "gold"}));
        let (_, d, d_sources) = person_of(&pool, batched, "d@x.example").await;
        assert_eq!(d, json!({"tier": "silver", "city": "Lyon"}));
        assert_eq!(d_sources, json!({"tier": "web", "city": "web"}));
        let (e, merged, _) = person_of(&pool, batched, "e@x.example").await;
        let (f, _, _) = person_of(&pool, batched, "f@x.example").await;
        assert_eq!(e, f, "the third record merged them");
        assert_eq!(
            merged,
            json!({"tier": "gold", "plan": "pro", "city": "Milan"})
        );

        // The recompute one person at a time, every key it holds, finds nothing left to change.
        let people: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM people WHERE workspace_id = $1 ORDER BY id")
                .bind(batched.as_uuid())
                .fetch_all(&pool)
                .await
                .unwrap();
        let mut tx = begin(&pool).await.unwrap();
        for person in people {
            let keys: Vec<String> = sqlx::query_scalar(
                "SELECT DISTINCT jsonb_object_keys(traits) FROM source_records \
                 WHERE person_id = $1 AND deleted_at IS NULL \
                 UNION SELECT jsonb_object_keys(traits) FROM people WHERE id = $1",
            )
            .bind(person)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
            assert!(
                !per_person_recompute(&mut tx, batched, person, &keys).await,
                "{person}"
            );
        }
        commit(tx).await.unwrap();
    }

    /// People with two ledger rows each, and a record of each from a third source.
    async fn known_people(pool: &PgPool, ws: WorkspaceId, people: usize) -> Vec<ProfileRecord> {
        for source in ["crm", "shop"] {
            let records: Vec<ProfileRecord> = (0..people)
                .map(|n| {
                    record(
                        source,
                        &n.to_string(),
                        &[&format!("p{n}@x.example")],
                        json!({"tier": source, "plan": source, "city": source}),
                        1,
                    )
                })
                .collect();
            apply_profile_batch(pool, ws, &records).await.unwrap();
        }
        (0..people)
            .map(|n| {
                record(
                    "web",
                    &n.to_string(),
                    &[&format!("p{n}@x.example")],
                    json!({"tier": "web", "plan": "web", "city": "web"}),
                    2,
                )
            })
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_recomputes_all_its_people_in_one_read_of_the_ledger_and_one_update(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let records = known_people(&pool, ws, 6).await;

        let (outcomes, statements) = statements_of(apply_profile_batch(&pool, ws, &records)).await;

        assert!(
            outcomes
                .unwrap()
                .iter()
                .all(|outcome| matches!(outcome, ProfileOutcome::Applied { changed: true, .. }))
        );
        let ledger_reads = statements
            .iter()
            .filter(|sql| {
                sql.starts_with("SELECT")
                    && sql.contains("FROM source_records")
                    && sql.contains("deleted_at IS NULL")
            })
            .count();
        assert_eq!(ledger_reads, 1, "{statements:#?}");
        assert_eq!(
            count(&statements, "UPDATE people SET traits"),
            1,
            "{statements:#?}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_reads_definitions_takes_the_slot_lock_and_looks_up_held_people_once(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let records = known_people(&pool, ws, 6).await;

        let (outcomes, statements) = statements_of(apply_profile_batch(&pool, ws, &records)).await;

        outcomes.unwrap();
        // The records' keys once, and the recompute's none: it reads what the records read.
        assert_eq!(
            count(
                &statements,
                "FROM trait_definitions WHERE workspace_id = $1 AND key"
            ),
            1,
            "{statements:#?}"
        );
        // The slot lock: the batch's own and the trait writer's, which comes in a statement of
        // its own right before the writer reads the assignments.
        assert_eq!(
            count(
                &statements,
                "pg_advisory_xact_lock_shared($1, hashtext($2::text))"
            ),
            2,
            "{statements:#?}"
        );
        // One lookup per record: the batch holds every owner, so none is looked up twice or
        // locked again.
        assert_eq!(
            count(&statements, "WITH ORDINALITY AS t(kind, value, ord)"),
            6,
            "{statements:#?}"
        );
        assert_eq!(
            count(
                &statements,
                "FROM people WHERE workspace_id = $1 AND id = ANY($2) ORDER BY id FOR UPDATE"
            ),
            0,
            "{statements:#?}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_known_person_with_nothing_new_reads_no_identities(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let records = known_people(&pool, ws, 6).await;

        let (outcomes, statements) = statements_of(apply_profile_batch(&pool, ws, &records)).await;

        outcomes.unwrap();
        // Every record names an identifier its person holds already, so there is nothing to
        // fit and no reason to read the person's identities.
        assert_eq!(
            count(
                &statements,
                "SELECT kind, value FROM identities WHERE workspace_id = $1 AND person_id = ANY($2)"
            ),
            0,
            "{statements:#?}"
        );
    }
}
