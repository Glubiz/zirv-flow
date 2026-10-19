//! The internal effects of a flow step (architecture 9.1 and 9.2): the flow's own contribution to
//! a trait, and list membership. They run in the step's transaction, so a branch later in the same
//! step sees them. The step execution row decides whether a step's effects are applied at all; the
//! effects themselves are also idempotent.

use std::fmt;

use serde_json::{Map, Number, Value};
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::flow::{ListChange, TraitChange};
use zirv_flow_core::{TraceContext, WorkspaceId};

use crate::changes::request_reevaluation;
use crate::lists::{ListSource, MembershipError, add_members, remove_members};
use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_record};
use crate::{StoreError, find_person, list_identities};

/// The stream of a flow's ledger rows: one row per person, holding that flow's contributions.
const STREAM: &str = "actions";

/// The source name of a flow's contributions to a trait.
pub fn flow_source(flow: Uuid) -> String {
    format!("flow:{flow}")
}

/// Why an internal effect could not be applied. The description is fixed text, safe to store.
#[derive(Debug)]
pub enum EffectError {
    /// An increment of a trait whose value is not a number.
    NotANumber,
    /// The person has no identifier to write the contribution under, or does not exist.
    NoPerson,
    /// The trait write was refused or dropped (an archived or mistyped trait).
    Refused(&'static str),
    /// The list does not exist in the workspace.
    NoSuchList,
    Store(StoreError),
}

impl EffectError {
    /// The fixed description of this error.
    pub const fn describe(&self) -> &'static str {
        match self {
            Self::NotANumber => "an increment needs a trait whose value is a number",
            Self::NoPerson => "the person no longer exists",
            Self::Refused(_) => "the trait write was refused",
            Self::NoSuchList => "the list no longer exists",
            Self::Store(_) => "database query failed",
        }
    }
}

impl fmt::Display for EffectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.describe())
    }
}

impl std::error::Error for EffectError {}

impl From<StoreError> for EffectError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// One trait write of a flow.
#[derive(Clone, Copy, Debug)]
pub struct FlowTraitWrite<'a> {
    pub workspace: WorkspaceId,
    pub flow: Uuid,
    pub person: Uuid,
    pub key: &'a str,
    pub change: &'a TraitChange,
    pub now: OffsetDateTime,
    pub trace: &'a TraceContext,
}

/// Writes the flow's contribution to a trait, as source `flow:<flow_id>` (architecture 9.1): set
/// writes the value, unset retracts the flow's contribution, increment writes the person's
/// effective value (missing counts as 0) plus `by`. The precedence policy of the trait decides
/// what the person's value is afterwards. Returns whether the person's traits changed.
pub async fn apply_flow_trait(
    connection: &mut PgConnection,
    write: &FlowTraitWrite<'_>,
) -> Result<bool, EffectError> {
    let FlowTraitWrite {
        workspace,
        flow,
        person,
        key,
        change,
        now,
        trace,
    } = *write;
    let value = match change {
        TraitChange::Set { value } => value.clone(),
        TraitChange::Unset {} => Value::Null,
        TraitChange::Increment { by } => {
            let current = find_person(&mut *connection, workspace, person)
                .await?
                .ok_or(EffectError::NoPerson)?;
            let held = match current.traits.get(key) {
                None | Some(Value::Null) => 0.0,
                Some(Value::Number(number)) => number.as_f64().ok_or(EffectError::NotANumber)?,
                Some(_) => return Err(EffectError::NotANumber),
            };
            number(held + by).ok_or(EffectError::NotANumber)?
        }
    };
    let identifiers: Vec<(String, String)> = list_identities(&mut *connection, workspace, person)
        .await?
        .into_iter()
        .map(|identity| (identity.kind, identity.value))
        .collect();
    if identifiers.is_empty() {
        return Err(EffectError::NoPerson);
    }
    let mut traits = Map::new();
    traits.insert(key.to_owned(), value);
    let record = ProfileRecord {
        source: flow_source(flow),
        stream: STREAM.to_owned(),
        record_key: person.to_string(),
        identifiers,
        traits,
        observed_at: now,
        trace: trace.clone(),
    };
    match apply_profile_record(connection, workspace, &record).await? {
        ProfileOutcome::Applied {
            changed, dropped, ..
        } => {
            if let Some(dropped) = dropped.first() {
                return Err(EffectError::Refused(dropped.reason));
            }
            request_reevaluation(connection, workspace).await?;
            Ok(changed)
        }
        ProfileOutcome::Unchanged { dropped, .. } => match dropped.first() {
            Some(dropped) => Err(EffectError::Refused(dropped.reason)),
            None => Ok(false),
        },
        ProfileOutcome::Rejected { reason } => Err(EffectError::Refused(reason)),
    }
}

/// A sum as a JSON number: an integer when it is one, so an integer trait stays an integer.
fn number(value: f64) -> Option<Value> {
    // 2^53: every integer below it is exact in an f64.
    const EXACT: f64 = 9_007_199_254_740_992.0;
    if value.fract() == 0.0 && value.abs() < EXACT {
        return Some(Value::from(value as i64));
    }
    Number::from_f64(value).map(Value::Number)
}

/// Adds the person to a list or removes them, with the list source `flow`. Returns whether
/// membership changed.
pub async fn apply_flow_list(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    list: Uuid,
    person: Uuid,
    change: ListChange,
    trace: &TraceContext,
) -> Result<bool, EffectError> {
    let people = [person];
    let outcome = match change {
        ListChange::Add => {
            add_members(
                connection,
                workspace,
                list,
                &people,
                ListSource::Flow,
                None,
                trace,
            )
            .await
        }
        ListChange::Remove => {
            remove_members(
                connection,
                workspace,
                list,
                &people,
                ListSource::Flow,
                None,
                trace,
            )
            .await
        }
    };
    match outcome {
        Ok(outcome) => Ok(outcome.changed > 0),
        Err(MembershipError::NoSuchList) => Err(EffectError::NoSuchList),
        Err(MembershipError::TooMany) => Err(EffectError::NoPerson),
        Err(MembershipError::Store(error)) => Err(EffectError::Store(error)),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::Cause;
    use zirv_flow_core::people::{Precedence, TraitType};

    use super::*;
    use crate::flows::tests_support::workspace;
    use crate::{NewTraitDefinition, ProfileRecord as Record, create_trait_definition};

    #[test]
    fn sums_stay_integers_when_they_are_integers() {
        assert_eq!(number(3.0), Some(json!(3)));
        assert_eq!(number(-2.0), Some(json!(-2)));
        assert_eq!(number(0.5), Some(json!(0.5)));
        assert_eq!(number(f64::NAN), None);
        assert_eq!(number(1e300), Some(json!(1e300)));
    }

    #[test]
    fn the_source_of_a_flow_is_named_by_its_id() {
        assert_eq!(
            flow_source(Uuid::from_u128(5)),
            "flow:00000000-0000-0000-0000-000000000005"
        );
    }

    #[test]
    fn every_error_has_a_fixed_description() {
        for error in [
            EffectError::NotANumber,
            EffectError::NoPerson,
            EffectError::Refused("archived"),
            EffectError::NoSuchList,
        ] {
            assert!(!error.to_string().is_empty());
        }
    }

    fn trace() -> TraceContext {
        TraceContext::root(Cause::api("t"))
    }

    async fn define(pool: &PgPool, ws: WorkspaceId, key: &str, kind: TraitType) {
        create_trait_definition(
            pool,
            ws,
            &NewTraitDefinition {
                key: key.into(),
                trait_type: kind,
                precedence: Precedence::Latest,
                source_order: Vec::new(),
                pii: false,
                channel_address: false,
                inferred: false,
                description: String::new(),
            },
        )
        .await
        .unwrap();
    }

    async fn person(pool: &PgPool, ws: WorkspaceId, email: &str, traits: Value) -> Uuid {
        let record = Record {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: email.into(),
            identifiers: vec![("email".into(), email.into())],
            traits: traits.as_object().cloned().unwrap(),
            observed_at: OffsetDateTime::now_utc() - time::Duration::days(1),
            trace: trace(),
        };
        match &crate::apply_profile_batch(pool, ws, &[record])
            .await
            .unwrap()[0]
        {
            ProfileOutcome::Applied { person_id, .. }
            | ProfileOutcome::Unchanged { person_id, .. } => *person_id,
            ProfileOutcome::Rejected { reason } => panic!("rejected: {reason}"),
        }
    }

    async fn traits_of(pool: &PgPool, person: Uuid) -> Value {
        sqlx::query_scalar("SELECT traits FROM people WHERE id = $1")
            .bind(person)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn write(pool: &PgPool, ws: WorkspaceId, flow: Uuid, person: Uuid, change: TraitChange) {
        let mut tx = crate::begin(pool).await.unwrap();
        apply_flow_trait(
            &mut tx,
            &FlowTraitWrite {
                workspace: ws,
                flow,
                person,
                key: "score",
                change: &change,
                now: OffsetDateTime::now_utc(),
                trace: &trace(),
            },
        )
        .await
        .unwrap();
        crate::commit(tx).await.unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn set_increment_and_unset_write_the_flows_own_contribution(pool: PgPool) {
        let ws = workspace(&pool, "traits").await;
        define(&pool, ws, "score", TraitType::Number).await;
        let ada = person(&pool, ws, "ada@example.com", json!({"score": 1})).await;
        let flow = Uuid::now_v7();

        write(&pool, ws, flow, ada, TraitChange::Set { value: json!(10) }).await;
        assert_eq!(traits_of(&pool, ada).await["score"], json!(10));
        write(&pool, ws, flow, ada, TraitChange::Increment { by: 2.0 }).await;
        assert_eq!(traits_of(&pool, ada).await["score"], json!(12));
        write(&pool, ws, flow, ada, TraitChange::Unset {}).await;
        assert_eq!(traits_of(&pool, ada).await["score"], json!(1));
        let source: Option<String> = sqlx::query_scalar(
            "SELECT source FROM source_records WHERE person_id = $1 AND source LIKE 'flow:%'",
        )
        .bind(ada)
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert_eq!(source.as_deref(), Some(flow_source(flow).as_str()));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_increment_of_a_missing_trait_starts_at_zero_and_of_a_text_trait_fails(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "increment").await;
        define(&pool, ws, "score", TraitType::Number).await;
        define(&pool, ws, "plan", TraitType::String).await;
        let ada = person(&pool, ws, "ada@example.com", json!({"plan": "pro"})).await;
        let flow = Uuid::now_v7();

        write(&pool, ws, flow, ada, TraitChange::Increment { by: 5.0 }).await;
        let mut tx = crate::begin(&pool).await.unwrap();
        let failed = apply_flow_trait(
            &mut tx,
            &FlowTraitWrite {
                workspace: ws,
                flow,
                person: ada,
                key: "plan",
                change: &TraitChange::Increment { by: 1.0 },
                now: OffsetDateTime::now_utc(),
                trace: &trace(),
            },
        )
        .await;

        assert_eq!(traits_of(&pool, ada).await["score"], json!(5));
        assert!(matches!(failed, Err(EffectError::NotANumber)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_list_change_is_made_once_with_the_flow_source(pool: PgPool) {
        let ws = workspace(&pool, "lists").await;
        let ada = person(&pool, ws, "ada@example.com", json!({})).await;
        let list = crate::create_list(&pool, ws, "Welcomed", "", None)
            .await
            .unwrap();
        let mut tx = crate::begin(&pool).await.unwrap();

        let added = apply_flow_list(&mut tx, ws, list.id, ada, ListChange::Add, &trace())
            .await
            .unwrap();
        let again = apply_flow_list(&mut tx, ws, list.id, ada, ListChange::Add, &trace())
            .await
            .unwrap();
        let ghost =
            apply_flow_list(&mut tx, ws, Uuid::now_v7(), ada, ListChange::Add, &trace()).await;
        crate::commit(tx).await.unwrap();

        assert!(added && !again);
        assert!(matches!(ghost, Err(EffectError::NoSuchList)));
        let source: String = sqlx::query_scalar("SELECT source FROM list_members")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(source, "flow");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_same_value_written_again_after_another_source_won_wins_again(pool: PgPool) {
        let ws = workspace(&pool, "rewrite").await;
        define(&pool, ws, "plan", TraitType::String).await;
        let ada = person(&pool, ws, "ada@example.com", json!({})).await;
        let flow = Uuid::now_v7();
        let base = OffsetDateTime::now_utc();
        let set_plan = |value: &'static str, at: OffsetDateTime| {
            let pool = pool.clone();
            async move {
                let mut tx = crate::begin(&pool).await.unwrap();
                apply_flow_trait(
                    &mut tx,
                    &FlowTraitWrite {
                        workspace: ws,
                        flow,
                        person: ada,
                        key: "plan",
                        change: &TraitChange::Set {
                            value: json!(value),
                        },
                        now: at,
                        trace: &trace(),
                    },
                )
                .await
                .unwrap();
                crate::commit(tx).await.unwrap();
            }
        };

        set_plan("pro", base).await;
        let crm = Record {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: "ada@example.com".into(),
            identifiers: vec![("email".into(), "ada@example.com".into())],
            traits: json!({"plan": "free"}).as_object().cloned().unwrap(),
            observed_at: base + time::Duration::seconds(10),
            trace: trace(),
        };
        crate::apply_profile_batch(&pool, ws, &[crm]).await.unwrap();
        assert_eq!(traits_of(&pool, ada).await["plan"], json!("free"));
        set_plan("pro", base + time::Duration::seconds(20)).await;

        assert_eq!(traits_of(&pool, ada).await["plan"], json!("pro"));
    }
}
