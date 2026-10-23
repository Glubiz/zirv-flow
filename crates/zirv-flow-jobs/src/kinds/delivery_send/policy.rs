//! The consent policy at gate 3 (architecture 10.1, OQ-32): the published rules, compiled for the
//! send and evaluated for the person in order. The first rule that matches denies the send and is
//! the one the message names.

use time::OffsetDateTime;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::policy::{PolicyRule, RECORDED_CONSENT_RULE};
use zirv_flow_core::consent::{Channel, Dimensions, RecordedConsent};
use zirv_flow_segments::{Catalog, RuleKind, SendContext, TraitStorage, compile_rules};
use zirv_flow_store::{
    Clock, DecidingRule, PgPool, RuleCheck, RuleToCheck, begin, evaluate_consent_policy,
    published_consent_policy, segment_catalog,
};

use crate::JobError;
use crate::kinds::segment_refresh::{catalog_of, sql_query};

/// What decided a send that the policy denies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Denial {
    pub(super) id: String,
    pub(super) name: String,
}

impl From<DecidingRule> for Denial {
    fn from(rule: DecidingRule) -> Self {
        Self {
            id: rule.id,
            name: rule.name,
        }
    }
}

impl Denial {
    pub(super) fn is_builtin(&self) -> bool {
        self.id == RECORDED_CONSENT_RULE
    }
}

/// How each rule is checked. The built-in rule matches when the recorded consent denies the send.
/// A rule that no longer compiles, because something it reads was archived since it was published,
/// matches: a consent system stops the send rather than let it through.
pub(super) fn checks(
    rules: &[PolicyRule],
    catalog: &Catalog,
    storage: &TraitStorage,
    now: OffsetDateTime,
    send: &SendContext,
    recorded_denies: bool,
) -> Vec<RuleToCheck> {
    compile_rules(rules, catalog, storage, now, send)
        .into_iter()
        .map(|rule| RuleToCheck {
            id: rule.id,
            name: rule.name,
            check: match rule.kind {
                RuleKind::RecordedConsent => RuleCheck::RecordedConsent {
                    denies: recorded_denies,
                },
                RuleKind::Query(query) => RuleCheck::Query(sql_query(query)),
                RuleKind::Unevaluable => RuleCheck::Unevaluable,
            },
        })
        .collect()
}

/// The rule that denies the send, if one does. With no published policy only the built-in rule
/// exists, so the answer is the recorded consent's alone and nothing else is read.
pub(super) async fn denial(
    pool: &PgPool,
    clock: &Clock,
    workspace: WorkspaceId,
    person: uuid::Uuid,
    dimensions: &Dimensions,
    recorded: RecordedConsent,
    recorded_denies: bool,
) -> Result<Option<Denial>, JobError> {
    let builtin = || {
        Denial::from(DecidingRule {
            id: RECORDED_CONSENT_RULE.to_owned(),
            name: zirv_flow_core::consent::policy::RECORDED_CONSENT_NAME.to_owned(),
        })
    };
    let Some(published) = published_consent_policy(pool, workspace).await? else {
        return Ok(recorded_denies.then(builtin));
    };
    // A transaction of its own that ends here, as the other gates that read the workspace.
    let mut connection = begin(pool).await?;
    let now = clock.now(&mut *connection).await?;
    let send = SendContext {
        channel: Channel::Email,
        dimensions: dimensions.clone(),
        consent: recorded,
    };
    // The catalog is only read when a rule has a condition to compile.
    let (catalog, storage) = if published.rules.iter().any(|rule| rule.condition.is_some()) {
        catalog_of(segment_catalog(&mut *connection, workspace).await?)
    } else {
        (Catalog::default(), TraitStorage::default())
    };
    let rules = checks(
        &published.rules,
        &catalog,
        &storage,
        now,
        &send,
        recorded_denies,
    );
    Ok(
        evaluate_consent_policy(&mut *connection, workspace, person, &rules)
            .await?
            .map(Denial::from),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use uuid::Uuid;
    use zirv_flow_segments::{Definition, compile_policy};

    use super::*;

    const NOW: OffsetDateTime = datetime!(2026-01-01 12:00 UTC);

    fn send() -> SendContext {
        SendContext {
            channel: Channel::Email,
            dimensions: Dimensions::new(),
            consent: RecordedConsent::None,
        }
    }

    fn checked(rules: &[PolicyRule], denies: bool) -> Vec<RuleToCheck> {
        checks(
            rules,
            &Catalog::default(),
            &TraitStorage::default(),
            NOW,
            &send(),
            denies,
        )
    }

    #[test]
    fn the_built_in_rule_carries_what_the_recorded_consent_said() {
        let rules = [PolicyRule::recorded_consent()];

        assert_eq!(
            checked(&rules, true)[0].check,
            RuleCheck::RecordedConsent { denies: true }
        );
        assert_eq!(
            checked(&rules, false)[0].check,
            RuleCheck::RecordedConsent { denies: false }
        );
    }

    #[test]
    fn a_compiled_rule_is_a_query_and_one_that_does_not_compile_is_unevaluable() {
        let good = PolicyRule {
            id: Uuid::from_u128(1).to_string(),
            name: "Good".into(),
            condition: Some(json!({"v": 1, "consent": {"status": "none"}})),
        };
        let archived = PolicyRule {
            condition: Some(json!({"v": 1, "trait": "gone", "op": "exists"})),
            ..good.clone()
        };

        let checks = checked(&[good, archived], false);

        assert!(matches!(&checks[0].check, RuleCheck::Query(_)));
        assert_eq!(checks[1].check, RuleCheck::Unevaluable);
    }

    #[test]
    fn a_denial_is_the_built_in_one_by_its_id() {
        assert!(
            Denial {
                id: RECORDED_CONSENT_RULE.into(),
                name: "x".into()
            }
            .is_builtin()
        );
        assert!(
            !Denial {
                id: Uuid::nil().to_string(),
                name: "x".into()
            }
            .is_builtin()
        );
    }

    /// A generated corpus of policy conditions over the two policy leaves, as JSON.
    fn corpus() -> Vec<serde_json::Value> {
        let windows = [
            serde_json::Value::Null,
            json!({"last": "P1D"}),
            json!({"last": "PT3H"}),
            json!({"between": ["2030-12-31T00:00:00Z", "2031-01-01T10:00:00Z"]}),
        ];
        let dimensions = [
            json!({}),
            json!({"vertical": "retail"}),
            json!({"vertical": "b2b"}),
            json!({"vertical": "retail", "brand": "x"}),
            json!({"brand": "nope"}),
        ];
        let mut leaves = Vec::new();
        for status in ["granted", "denied", "none"] {
            leaves.push(json!({"consent": {"status": status}}));
        }
        for window in &windows {
            for dims in &dimensions {
                for op in ["eq", "neq", "gt", "gte", "lt", "lte"] {
                    for value in [0, 1, 2, 3] {
                        let mut leaf = json!({"channel": "email", "dimensions": dims,
                            "agg": {"fn": "count", "op": op, "value": value}});
                        if !window.is_null() {
                            leaf["window"] = window.clone();
                        }
                        leaves.push(json!({"messages": leaf}));
                    }
                }
                for (op, value) in [
                    ("within_last", json!("PT1H")),
                    ("within_last", json!("P1D")),
                    ("more_than_ago", json!("PT3H")),
                    ("more_than_ago", json!("P2D")),
                    ("before", json!("2031-01-01T08:00:00Z")),
                    ("after", json!("2031-01-01T08:00:00Z")),
                ] {
                    let mut leaf = json!({"channel": "email", "dimensions": dims,
                        "agg": {"fn": "last_time", "op": op, "value": value}});
                    if !window.is_null() {
                        leaf["window"] = window.clone();
                    }
                    leaves.push(json!({"messages": leaf}));
                }
            }
        }
        let mut all = leaves.clone();
        all.extend(leaves.iter().map(|leaf| json!({"not": leaf})));
        // Pairs, so that the connectives are compared too.
        for (index, leaf) in leaves.iter().enumerate().step_by(7) {
            let other = &leaves[(index * 13 + 5) % leaves.len()];
            all.push(json!({"all": [leaf, other]}));
            all.push(json!({"any": [leaf, {"not": other}]}));
        }
        all
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_reference_evaluator_agrees_with_the_compiled_sql_on_a_generated_corpus(
        pool: PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = zirv_flow_store::create_workspace(
            &pool,
            &zirv_flow_core::WorkspaceName::parse("corpus").unwrap(),
            &zirv_flow_core::Slug::parse("corpus").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let person = match &zirv_flow_store::apply_profile_batch(
            &pool,
            ws,
            &[zirv_flow_store::ProfileRecord {
                source: "crm".into(),
                stream: "contacts".into(),
                record_key: "k".into(),
                identifiers: vec![("email".into(), "ada@example.com".into())],
                traits: serde_json::Map::new(),
                observed_at: NOW - time::Duration::days(3),
                trace: zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t")),
            }],
        )
        .await
        .unwrap()[0]
        {
            zirv_flow_store::ProfileOutcome::Applied { person_id, .. }
            | zirv_flow_store::ProfileOutcome::Unchanged { person_id, .. } => *person_id,
            zirv_flow_store::ProfileOutcome::Rejected { reason } => panic!("rejected: {reason}"),
        };
        // (status, test, hours ago it ended, dimensions): only sent, non-test rows count.
        let history: [(&str, bool, i64, serde_json::Value); 8] = [
            ("sent", false, 1, json!({"vertical": "retail"})),
            (
                "sent",
                false,
                2,
                json!({"vertical": "retail", "brand": "x"}),
            ),
            ("sent", false, 5, json!({"vertical": "b2b"})),
            ("sent", false, 30, json!({})),
            ("sent", false, 60, json!({"vertical": "retail"})),
            ("skipped", false, 1, json!({"vertical": "retail"})),
            ("failed", false, 2, json!({"vertical": "b2b"})),
            ("sent", true, 1, json!({"vertical": "b2b"})),
        ];
        let mut sent = Vec::new();
        for (status, test, hours, dims) in &history {
            let at = NOW - time::Duration::hours(*hours);
            sqlx::query(
                "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, \
                    template_id, destination_id, category, dimensions, status, reason, test, \
                    created_at, completed_at) \
                 VALUES ($1, $2, $3, $4, 'mail', $5, $6, 'marketing', $7, $8, \
                    CASE WHEN $8 = 'sent' THEN NULL ELSE 'no_consent' END, $9, $10, $10)",
            )
            .bind(Uuid::now_v7())
            .bind(ws.as_uuid())
            .bind(person)
            .bind(Uuid::now_v7())
            .bind(Uuid::now_v7())
            .bind(Uuid::now_v7())
            .bind(dims)
            .bind(*status)
            .bind(*test)
            .bind(at)
            .execute(&pool)
            .await
            .unwrap();
            if *status == "sent" && !*test {
                sent.push(zirv_flow_segments::SnapshotMessage {
                    channel: Channel::Email,
                    dimensions: dims
                        .as_object()
                        .unwrap()
                        .iter()
                        .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_owned()))
                        .collect(),
                    sent_at: at,
                });
            }
        }
        let catalog = Catalog::default();
        let storage = TraitStorage::default();
        let mut connection = pool.acquire().await.unwrap();
        let mut compared = 0;
        for consent in RecordedConsent::ALL {
            let send = SendContext {
                channel: Channel::Email,
                dimensions: Dimensions::new(),
                consent,
            };
            let facts = zirv_flow_segments::PolicyFacts {
                send: send.clone(),
                messages: sent.clone(),
            };
            for condition in corpus() {
                let mut value = condition.clone();
                value["v"] = json!(1);
                let definition = Definition::from_value(&value).unwrap();
                let compiled = compile_policy(&definition, &catalog, &storage, NOW, &send)
                    .unwrap_or_else(|issues| panic!("{condition}: {issues:?}"));
                let selected = zirv_flow_store::run_person_batch(
                    &mut connection,
                    ws,
                    &sql_query(compiled.batch_sql()),
                    &[person],
                )
                .await
                .unwrap();
                let expected = zirv_flow_segments::evaluate_policy(
                    &definition,
                    &catalog,
                    &zirv_flow_segments::PersonSnapshot::default(),
                    &facts,
                    NOW,
                )
                .outcome;
                assert_eq!(
                    !selected.is_empty(),
                    expected == zirv_flow_segments::Outcome::Pass,
                    "{consent:?} {condition}"
                );
                compared += 1;
            }
        }
        assert!(compared > 3000, "{compared}");
    }
}
