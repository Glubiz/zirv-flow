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
//!   the keys that changed, one reads the definitions the batch has not read, one writes every
//!   person whose value or source changed ([`recompute_people`]), and one marks them
//!   ([`MarkBuffer`]).
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
use crate::changes::MarkBuffer;
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
        let mut marks = MarkBuffer::default();
        for entry in &owed {
            let value_changed = changed
                .get(&entry.person)
                .is_some_and(|keys| entry.keys.iter().any(|key| keys.contains(key)));
            if value_changed || (entry.created && !entry.moved_from) {
                marks.push(entry.person, ChangeClasses::TRAITS, &entry.trace, None);
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
        // One statement for all of them, in the records' order, so the latest change's trace is
        // the one a mark keeps.
        marks.flush(connection, workspace).await?;
        Ok(applied)
    }
}

/// A person's contributions to the keys being recomputed, by key.
type Contributions = HashMap<String, Vec<Contribution<Value>>>;

/// The live ledger contributions of the people `$2` to the `(person, key)` pairs `$3`, `$4`. A
/// record with no traits holds no key, so `traits <> '{}'` changes no row of the result; it lets
/// the read walk `source_records_traits_idx` (0095), the live records that hold traits (22% of
/// the ledger on the 5x sim), instead of fetching every record of the people.
pub(super) const LEDGER_CONTRIBUTIONS: &str = "SELECT s.person_id, w.key, s.id, s.source, \
         s.traits -> w.key \
     FROM source_records s \
     JOIN unnest($3::uuid[], $4::text[]) AS w(person_id, key) ON w.person_id = s.person_id \
     WHERE s.workspace_id = $1 AND s.person_id = ANY($2) AND s.deleted_at IS NULL \
       AND s.traits <> '{}' AND s.traits ? w.key";

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
    let found: Vec<(Uuid, String, Uuid, String, Value)> = sqlx::query_as(LEDGER_CONTRIBUTIONS)
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

    // The ledger reads that skip the records holding no traits (round 7e2, migration 0095).

    /// Main's three reads before they gained `traits <> '{}'`: the reference the new ones must
    /// agree with.
    const MAIN_LEDGER_CONTRIBUTIONS: &str = "SELECT s.person_id, w.key, s.id, s.source, \
             s.traits -> w.key \
         FROM source_records s \
         JOIN unnest($3::uuid[], $4::text[]) AS w(person_id, key) ON w.person_id = s.person_id \
         WHERE s.workspace_id = $1 AND s.person_id = ANY($2) AND s.deleted_at IS NULL \
           AND s.traits ? w.key";
    const MAIN_PERSON_LEDGER_KEYS: &str = "SELECT DISTINCT jsonb_object_keys(traits) \
         FROM source_records \
         WHERE workspace_id = $1 AND person_id = $2 AND deleted_at IS NULL";
    const MAIN_PEOPLE_HOLDING_KEY: &str = "SELECT DISTINCT person_id FROM source_records \
         WHERE workspace_id = $1 AND deleted_at IS NULL AND traits ? $2 \
           AND ($3::uuid IS NULL OR person_id > $3) \
         ORDER BY person_id LIMIT $4";

    /// Every person of `ws`, recomputed one at a time from all their live ledger rows, with or
    /// without traits, on every key they or their records hold, has nothing left to change.
    async fn nothing_left_to_recompute(pool: &PgPool, ws: WorkspaceId) {
        let people: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM people WHERE workspace_id = $1 ORDER BY id")
                .bind(ws.as_uuid())
                .fetch_all(pool)
                .await
                .unwrap();
        let mut tx = begin(pool).await.unwrap();
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
                !per_person_recompute(&mut tx, ws, person, &keys).await,
                "{person}"
            );
        }
        commit(tx).await.unwrap();
    }

    /// Two people with records that hold traits and records that hold none (as a Kafka
    /// message's record does), then a record whose traits become empty, one whose traits stop
    /// being empty, an older observation of a key (#584), a record that goes and comes back, and
    /// a merge that moves both people's records to one. Returns the survivor.
    async fn mixed_ledger(pool: &PgPool, ws: WorkspaceId) -> Uuid {
        define(
            pool,
            ws,
            "tier",
            Precedence::SourcePriority,
            &["crm", "shop"],
        )
        .await;
        define(pool, ws, "plan", Precedence::First, &[]).await;
        let (a, b) = (["a@x.example"], ["b@x.example"]);
        let apply = |records: Vec<ProfileRecord>| async move {
            apply_profile_batch(pool, ws, &records).await.unwrap();
        };
        apply(vec![
            record("crm", "a", &a, json!({"tier": "gold", "plan": "basic"}), 1),
            record("shop", "a", &a, json!({}), 1),
            record("web", "a", &a, json!({"city": "Oslo"}), 2),
            record("kafka", "a-1", &a, json!({}), 2),
            record("shop", "b", &b, json!({"tier": "silver", "plan": "pro"}), 1),
            record("kafka", "b-1", &b, json!({}), 2),
        ])
        .await;
        let (_, traits, _) = person_of(pool, ws, "a@x.example").await;
        assert_eq!(
            traits,
            json!({"tier": "gold", "plan": "basic", "city": "Oslo"})
        );

        // web/a's traits become empty; shop/a's stop being empty.
        apply(vec![
            record("web", "a", &a, json!({"city": null}), 3),
            record(
                "shop",
                "a",
                &a,
                json!({"tier": "silver", "city": "Bergen"}),
                3,
            ),
        ])
        .await;
        let web: Value = sqlx::query_scalar(
            "SELECT traits FROM source_records WHERE source = 'web' AND record_key = 'a'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(web, json!({}));
        let (_, traits, sources) = person_of(pool, ws, "a@x.example").await;
        assert_eq!(
            traits,
            json!({"tier": "gold", "plan": "basic", "city": "Bergen"})
        );
        assert_eq!(sources["city"], json!("shop"));

        // Within one record an older observation does not replace a newer one (#584).
        apply(vec![record("shop", "a", &a, json!({"city": "Paris"}), 2)]).await;
        let (_, traits, _) = person_of(pool, ws, "a@x.example").await;
        assert_eq!(traits["city"], json!("Bergen"));

        // crm/a goes: the tier falls to shop and no plan is left. It comes back holding only
        // what it says now.
        let trace = TraceContext::root(Cause::api("test"));
        assert!(
            crate::people::remove_contribution(pool, ws, "crm", "contacts", "a", &trace)
                .await
                .unwrap()
        );
        let (_, traits, _) = person_of(pool, ws, "a@x.example").await;
        assert_eq!(traits, json!({"tier": "silver", "city": "Bergen"}));
        apply(vec![record("crm", "a", &a, json!({"tier": "platinum"}), 5)]).await;
        let (_, traits, _) = person_of(pool, ws, "a@x.example").await;
        assert_eq!(traits, json!({"tier": "platinum", "city": "Bergen"}));

        // A record naming both merges them, and every record of both moves to the survivor.
        apply(vec![record(
            "crm",
            "ab",
            &["a@x.example", "b@x.example"],
            json!({"vip": "yes"}),
            6,
        )])
        .await;
        let (survivor, _, _) = person_of(pool, ws, "a@x.example").await;
        assert_eq!(person_of(pool, ws, "b@x.example").await.0, survivor);
        survivor
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn records_that_empty_fill_go_come_back_and_move_keep_precedence_and_observed_time(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "mixed").await;

        let survivor = mixed_ledger(&pool, ws).await;

        let (_, traits, sources) = person_of(&pool, ws, "a@x.example").await;
        assert_eq!(
            traits,
            json!({"tier": "platinum", "plan": "pro", "city": "Bergen", "vip": "yes"})
        );
        assert_eq!(
            sources,
            json!({"tier": "crm", "plan": "shop", "city": "shop", "vip": "crm"})
        );
        let (live, empty): (i64, i64) = sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE traits = '{}') FROM source_records \
             WHERE person_id = $1 AND deleted_at IS NULL",
        )
        .bind(survivor)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (live, empty),
            (7, 3),
            "the survivor holds both people's records"
        );
        nothing_left_to_recompute(&pool, ws).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_ledger_reads_return_the_rows_they_returned_without_the_traits_predicate(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "mixed").await;
        mixed_ledger(&pool, ws).await;
        // Two more people, one holding only records without traits.
        apply_profile_batch(
            &pool,
            ws,
            &[
                record("crm", "c", &["c@x.example"], json!({"plan": "basic"}), 1),
                record("kafka", "c-1", &["c@x.example"], json!({}), 1),
                record("kafka", "d-1", &["d@x.example"], json!({}), 1),
                record("kafka", "d-2", &["d@x.example"], json!({}), 2),
            ],
        )
        .await
        .unwrap();
        let people: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM people WHERE workspace_id = $1 ORDER BY id")
                .bind(ws.as_uuid())
                .fetch_all(&pool)
                .await
                .unwrap();
        let mut keys: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT jsonb_object_keys(traits) FROM source_records ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        keys.push("nobody".to_owned());
        let (mut pair_people, mut pair_keys) = (Vec::new(), Vec::new());
        for person in &people {
            for key in &keys {
                pair_people.push(*person);
                pair_keys.push(key.as_str());
            }
        }
        let tombstones: i64 =
            sqlx::query_scalar("SELECT count(*) FROM source_records WHERE deleted_at IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(tombstones, 0, "crm/a came back");

        type Contribution = (Uuid, String, Uuid, String, Value);
        let (db, everyone) = (&pool, &people);
        let (pair_people, pair_keys) = (&pair_people, &pair_keys);
        let contributions = |sql: &'static str| async move {
            let mut rows: Vec<Contribution> = sqlx::query_as(sql)
                .bind(ws.as_uuid())
                .bind(everyone)
                .bind(pair_people)
                .bind(pair_keys)
                .fetch_all(db)
                .await
                .unwrap();
            rows.sort_by(|x, y| (x.0, &x.1, x.2).cmp(&(y.0, &y.1, y.2)));
            rows
        };
        let found = contributions(LEDGER_CONTRIBUTIONS).await;
        assert_eq!(found, contributions(MAIN_LEDGER_CONTRIBUTIONS).await);
        // crm/a tier, shop/a tier and city, shop/b tier and plan, crm/ab vip, crm/c plan.
        assert_eq!(found.len(), 7, "{found:#?}");

        for person in &people {
            let keys_of = |sql: &'static str| async move {
                let mut keys: Vec<String> = sqlx::query_scalar(sql)
                    .bind(ws.as_uuid())
                    .bind(person)
                    .fetch_all(db)
                    .await
                    .unwrap();
                keys.sort();
                keys
            };
            assert_eq!(
                keys_of(super::super::merge::PERSON_LEDGER_KEYS).await,
                keys_of(MAIN_PERSON_LEDGER_KEYS).await,
                "{person}"
            );
        }

        for key in &keys {
            let holding = |sql: &'static str| async move {
                let people: Vec<Uuid> = sqlx::query_scalar(sql)
                    .bind(ws.as_uuid())
                    .bind(key)
                    .bind(None::<Uuid>)
                    .bind(500_i64)
                    .fetch_all(db)
                    .await
                    .unwrap();
                people
            };
            assert_eq!(
                holding(super::super::recompute::PEOPLE_HOLDING_KEY).await,
                holding(MAIN_PEOPLE_HOLDING_KEY).await,
                "{key}"
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_ledger_reads_walk_the_traits_index_on_a_ledger_of_mostly_empty_records(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "five-x").await;
        // 10,000 people with 12 records each, 22% of them holding traits: the shape of the 5x
        // sim's ledger.
        sqlx::query(
            "INSERT INTO people (id, workspace_id) \
             SELECT md5('p' || p)::uuid, $1 FROM generate_series(0, 9999) p",
        )
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO source_records \
                (id, workspace_id, source, stream, record_key, person_id, traits, content_hash) \
             SELECT gen_random_uuid(), $1, 'source-' || k, 'stream', 'r' || i, \
                    md5('p' || p)::uuid, \
                    CASE WHEN k < 2 OR (k = 2 AND p % 25 < 16) THEN jsonb_build_object( \
                        'plan', jsonb_build_object('v', 'basic', 'at', '2026-11-01T00:00:00Z'), \
                        'tier', jsonb_build_object('v', 'gold', 'at', '2026-11-01T00:00:00Z')) \
                    ELSE '{}' END, '\\x00' \
             FROM (SELECT i, i % 10000 AS p, i / 10000 AS k FROM generate_series(0, 119999) i) g",
        )
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE source_records")
            .execute(&pool)
            .await
            .unwrap();
        let people: Vec<Uuid> =
            sqlx::query_scalar("SELECT md5('p' || (p * 17))::uuid FROM generate_series(0, 99) p")
                .fetch_all(&pool)
                .await
                .unwrap();
        let (mut pair_people, mut pair_keys) = (Vec::new(), Vec::new());
        for person in &people {
            for key in ["plan", "tier", "city"] {
                pair_people.push(*person);
                pair_keys.push(key);
            }
        }
        let explain = |sql: &str| format!("EXPLAIN (FORMAT JSON) {sql}");

        let lookup: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(explain(LEDGER_CONTRIBUTIONS)))
            .bind(ws.as_uuid())
            .bind(&people)
            .bind(&pair_people)
            .bind(&pair_keys)
            .fetch_one(&pool)
            .await
            .unwrap();
        let keys: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(explain(
            super::super::merge::PERSON_LEDGER_KEYS,
        )))
        .bind(ws.as_uuid())
        .bind(people[0])
        .fetch_one(&pool)
        .await
        .unwrap();
        let holding: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(explain(
            super::super::recompute::PEOPLE_HOLDING_KEY,
        )))
        .bind(ws.as_uuid())
        .bind("plan")
        .bind(None::<Uuid>)
        .bind(500_i64)
        .fetch_one(&pool)
        .await
        .unwrap();

        for (read, plan) in [("lookup", lookup), ("keys", keys), ("holding", holding)] {
            let plan = plan.to_string();
            assert!(
                plan.contains("source_records_traits_idx")
                    && !plan.contains("source_records_person_idx"),
                "{read}: {plan}"
            );
        }
    }
}
