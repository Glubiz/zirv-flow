//! The consent policy of a workspace (architecture 10.1, OQ-32): a draft that is edited, and
//! immutable published versions that the send gate evaluates.
//!
//! One policy per workspace: consent dimensions and topics are already per workspace, and a rule
//! decides on the message's own channel and dimension values, so a rule that concerns one topic or
//! vertical says so in its condition. This crate never sees the compiler: the caller validates the
//! conditions before a draft is saved or published, and passes the compiled SQL of each rule to
//! [`evaluate_consent_policy`].

use serde_json::Value;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::consent::policy::{PolicyRule, default_rules};
use zirv_flow_core::{TraceContext, UserId, WorkspaceId};

use crate::people::current_person;
use crate::segments::{SqlQuery, run_person_batch};
use crate::{NewDecision, StoreError, append_decision};

/// A workspace's policy row: the draft and which version is published.
#[derive(Clone, Debug, PartialEq)]
pub struct ConsentPolicy {
    pub id: Uuid,
    pub draft_rules: Vec<PolicyRule>,
    /// Counts the saves of the draft; a publish names the revision it validated.
    pub draft_revision: i32,
    pub published_version: Option<i32>,
    pub updated_by: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}

/// One immutable published version.
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyVersion {
    pub version: i32,
    pub rules: Vec<PolicyRule>,
    pub published_by: Option<Uuid>,
    pub published_at: OffsetDateTime,
}

type PolicyRow = (Uuid, Value, i32, Option<i32>, Option<Uuid>, OffsetDateTime);

type VersionRow = (i32, Value, Option<Uuid>, OffsetDateTime);

macro_rules! policy_columns {
    () => {
        "id, draft_rules, draft_revision, published_version, updated_by, updated_at"
    };
}

fn rules_of(stored: Value) -> Result<Vec<PolicyRule>, StoreError> {
    serde_json::from_value(stored)
        .map_err(|_| StoreError::query(sqlx::Error::Decode("malformed policy rules".into())))
}

fn rules_value(rules: &[PolicyRule]) -> Result<Value, StoreError> {
    serde_json::to_value(rules)
        .map_err(|_| StoreError::query(sqlx::Error::Decode("unwritable policy rules".into())))
}

fn policy_of(row: PolicyRow) -> Result<ConsentPolicy, StoreError> {
    Ok(ConsentPolicy {
        id: row.0,
        draft_rules: rules_of(row.1)?,
        draft_revision: row.2,
        published_version: row.3,
        updated_by: row.4,
        updated_at: row.5,
    })
}

fn version_of(row: VersionRow) -> Result<PolicyVersion, StoreError> {
    Ok(PolicyVersion {
        version: row.0,
        rules: rules_of(row.1)?,
        published_by: row.2,
        published_at: row.3,
    })
}

/// The workspace's policy row, none until a draft is first saved.
pub async fn find_consent_policy<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Option<ConsentPolicy>, StoreError> {
    let row: Option<PolicyRow> = sqlx::query_as(concat!(
        "SELECT ",
        policy_columns!(),
        " FROM consent_policies WHERE workspace_id = $1"
    ))
    .bind(workspace.as_uuid())
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(policy_of).transpose()
}

/// The draft rules of the workspace: the saved draft, or the built-in rule alone before one is
/// saved.
pub async fn consent_policy_draft<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Vec<PolicyRule>, StoreError> {
    Ok(find_consent_policy(executor, workspace)
        .await?
        .map_or_else(default_rules, |policy| policy.draft_rules))
}

/// Saves the draft in the caller's transaction, creating the policy row on the first save, and
/// returns the row with the revision this save made. The rules were checked by the caller.
pub async fn save_consent_policy_draft(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    rules: &[PolicyRule],
    saved_by: Option<UserId>,
) -> Result<ConsentPolicy, StoreError> {
    let row: PolicyRow = sqlx::query_as(concat!(
        "INSERT INTO consent_policies (id, workspace_id, draft_rules, updated_by) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (workspace_id) DO UPDATE SET draft_rules = EXCLUDED.draft_rules, \
            draft_revision = consent_policies.draft_revision + 1, \
            updated_by = EXCLUDED.updated_by, updated_at = now() \
         RETURNING ",
        policy_columns!()
    ))
    .bind(Uuid::now_v7())
    .bind(workspace.as_uuid())
    .bind(rules_value(rules)?)
    .bind(saved_by.map(|user| user.as_uuid()))
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    policy_of(row)
}

/// Saves the draft only if it is still at `expected` revision (0 when no draft was ever saved),
/// in the caller's transaction. None, with nothing written, when the draft was saved since.
pub async fn save_consent_policy_draft_at(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    rules: &[PolicyRule],
    saved_by: Option<UserId>,
    expected: i32,
) -> Result<Option<ConsentPolicy>, StoreError> {
    let row: Option<PolicyRow> = if expected == 0 {
        sqlx::query_as(concat!(
            "INSERT INTO consent_policies (id, workspace_id, draft_rules, updated_by) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (workspace_id) DO NOTHING RETURNING ",
            policy_columns!()
        ))
        .bind(Uuid::now_v7())
        .bind(workspace.as_uuid())
        .bind(rules_value(rules)?)
        .bind(saved_by.map(|user| user.as_uuid()))
        .fetch_optional(&mut *connection)
        .await
    } else {
        sqlx::query_as(concat!(
            "UPDATE consent_policies SET draft_rules = $2, draft_revision = draft_revision + 1, \
                updated_by = $3, updated_at = now() \
             WHERE workspace_id = $1 AND draft_revision = $4 RETURNING ",
            policy_columns!()
        ))
        .bind(workspace.as_uuid())
        .bind(rules_value(rules)?)
        .bind(saved_by.map(|user| user.as_uuid()))
        .bind(expected)
        .fetch_optional(&mut *connection)
        .await
    }
    .map_err(StoreError::query)?;
    row.map(policy_of).transpose()
}

/// Locks the workspace's policy row until the transaction ends, so two publishes take turns and
/// a publish reads the draft it publishes. None when no draft was ever saved.
pub async fn lock_consent_policy(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Option<ConsentPolicy>, StoreError> {
    let row: Option<PolicyRow> = sqlx::query_as(concat!(
        "SELECT ",
        policy_columns!(),
        " FROM consent_policies WHERE workspace_id = $1 FOR UPDATE"
    ))
    .bind(workspace.as_uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    row.map(policy_of).transpose()
}

/// Publishes the locked policy's draft as version n + 1 and makes it the one the gate reads, in
/// the caller's transaction, and appends a `consent_policy.publish` decision under `trace` (a
/// definition change). The caller validated the draft it holds under the lock.
pub async fn publish_consent_policy(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    locked: &ConsentPolicy,
    published_by: Option<UserId>,
    trace: &TraceContext,
) -> Result<PolicyVersion, StoreError> {
    let number = locked.published_version.unwrap_or(0) + 1;
    let row: VersionRow = sqlx::query_as(
        "INSERT INTO consent_policy_versions \
            (workspace_id, policy_id, version, rules, trace_id, published_by) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         RETURNING version, rules, published_by, published_at",
    )
    .bind(workspace.as_uuid())
    .bind(locked.id)
    .bind(number)
    .bind(rules_value(&locked.draft_rules)?)
    .bind(trace.trace_id.as_uuid())
    .bind(published_by.map(|user| user.as_uuid()))
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    sqlx::query(
        "UPDATE consent_policies SET published_version = $3, updated_at = now() \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(locked.id)
    .bind(number)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    append_decision(
        connection,
        &NewDecision {
            graph: None,
            workspace_id: workspace,
            trace: trace.clone(),
            action: "consent_policy.publish",
            outcome: "published",
            reason: None,
            subject_ids: vec![("consent_policy", locked.id)],
            job: None,
        },
    )
    .await?;
    version_of(row)
}

/// The published version the send gate evaluates; none when the workspace never published one.
pub async fn published_consent_policy<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Option<PolicyVersion>, StoreError> {
    let row: Option<VersionRow> = sqlx::query_as(
        "SELECT v.version, v.rules, v.published_by, v.published_at \
         FROM consent_policies p \
         JOIN consent_policy_versions v ON v.policy_id = p.id AND v.version = p.published_version \
         WHERE p.workspace_id = $1",
    )
    .bind(workspace.as_uuid())
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(version_of).transpose()
}

/// The published versions of the workspace's policy, newest first.
pub async fn list_consent_policy_versions<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Vec<PolicyVersion>, StoreError> {
    let rows: Vec<VersionRow> = sqlx::query_as(
        "SELECT version, rules, published_by, published_at FROM consent_policy_versions \
         WHERE workspace_id = $1 ORDER BY version DESC",
    )
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    rows.into_iter().map(version_of).collect()
}

/// How one rule is checked for a send.
#[derive(Clone, Debug, PartialEq)]
pub enum RuleCheck {
    /// The built-in rule: it matches when the recorded consent does not allow the send, which
    /// the caller has already read.
    RecordedConsent { denies: bool },
    /// A compiled rule: it matches when the query selects the person. `$1` is the workspace and
    /// the person batch is bound last, as for a segment's batch query.
    Query(SqlQuery),
    /// A rule the caller could not compile, for example because something it reads was archived
    /// since it was published. It matches, so the send is stopped rather than let through.
    Unevaluable,
}

/// A rule of the published version with the means to check it.
#[derive(Clone, Debug, PartialEq)]
pub struct RuleToCheck {
    pub id: String,
    pub name: String,
    pub check: RuleCheck,
}

/// The rule that decided a send.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecidingRule {
    pub id: String,
    pub name: String,
    /// The rule matched because it could not be compiled (`RuleCheck::Unevaluable`), not because
    /// its condition held.
    pub unevaluable: bool,
}

/// Evaluates the rules in order for one person, on the caller's connection (the send's), and
/// returns the first rule that matches, or none when the send proceeds. The person is followed
/// through `merged_into`; the later rules are not run once one matches.
pub async fn evaluate_consent_policy(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    rules: &[RuleToCheck],
) -> Result<Option<DecidingRule>, StoreError> {
    let Some(person) = current_person(&mut *connection, workspace, person).await? else {
        return Ok(None);
    };
    for rule in rules {
        let matched = match &rule.check {
            RuleCheck::RecordedConsent { denies } => *denies,
            RuleCheck::Unevaluable => true,
            RuleCheck::Query(query) => !run_person_batch(connection, workspace, query, &[person])
                .await?
                .is_empty(),
        };
        if matched {
            return Ok(Some(DecidingRule {
                id: rule.id.clone(),
                name: rule.name.clone(),
                unevaluable: rule.check == RuleCheck::Unevaluable,
            }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::consent::policy::PolicyRule;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::segments::SqlParam;
    use crate::{begin, commit, create_workspace};

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn custom(id: &str) -> PolicyRule {
        PolicyRule {
            id: id.to_owned(),
            name: format!("Rule {id}"),
            condition: Some(json!({"v": 1, "consent": {"status": "none"}})),
        }
    }

    fn trace() -> TraceContext {
        TraceContext::root(Cause::definition_change(Uuid::now_v7()))
    }

    async fn save(pool: &PgPool, ws: WorkspaceId, rules: &[PolicyRule]) -> ConsentPolicy {
        let mut tx = begin(pool).await.unwrap();
        let policy = save_consent_policy_draft(&mut tx, ws, rules, None)
            .await
            .unwrap();
        commit(tx).await.unwrap();
        policy
    }

    async fn publish(pool: &PgPool, ws: WorkspaceId) -> PolicyVersion {
        let mut tx = begin(pool).await.unwrap();
        let locked = lock_consent_policy(&mut tx, ws).await.unwrap().unwrap();
        let version = publish_consent_policy(&mut tx, ws, &locked, None, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();
        version
    }

    #[test]
    fn a_stored_rule_list_that_is_not_rules_is_a_decode_error() {
        assert!(rules_of(json!({"not": "a list"})).is_err());
        assert_eq!(
            rules_of(json!([{"id": "recorded_consent", "name": "Recorded consent"}])).unwrap(),
            default_rules()
        );
    }

    #[test]
    fn the_rules_are_written_in_order_without_an_absent_condition() {
        let value = rules_value(&[custom("b"), PolicyRule::recorded_consent()]).unwrap();

        assert_eq!(value[0]["id"], "b");
        assert_eq!(
            value[1],
            json!({"id": "recorded_consent", "name": "Recorded consent"})
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_workspace_with_no_policy_has_only_the_built_in_rule(pool: PgPool) {
        let ws = workspace(&pool, "one").await;

        assert_eq!(
            consent_policy_draft(&pool, ws).await.unwrap(),
            default_rules()
        );
        assert!(published_consent_policy(&pool, ws).await.unwrap().is_none());
        assert!(
            list_consent_policy_versions(&pool, ws)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn saving_bumps_the_revision_and_publishing_numbers_the_versions(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let first = save(&pool, ws, &[PolicyRule::recorded_consent()]).await;
        let second = save(&pool, ws, &[custom("a"), PolicyRule::recorded_consent()]).await;

        let one = publish(&pool, ws).await;
        save(&pool, ws, &[PolicyRule::recorded_consent()]).await;
        let two = publish(&pool, ws).await;

        assert_eq!((first.draft_revision, second.draft_revision), (1, 2));
        assert_eq!(first.id, second.id);
        assert_eq!((one.version, two.version), (1, 2));
        assert_eq!(one.rules.len(), 2);
        assert_eq!(two.rules, default_rules());
        let published = published_consent_policy(&pool, ws).await.unwrap().unwrap();
        assert_eq!(published.version, 2);
        let versions: Vec<i32> = list_consent_policy_versions(&pool, ws)
            .await
            .unwrap()
            .iter()
            .map(|version| version.version)
            .collect();
        assert_eq!(versions, [2, 1]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_conditional_save_writes_only_at_the_expected_revision(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let rules = [custom("a"), PolicyRule::recorded_consent()];
        let mut tx = begin(&pool).await.unwrap();

        let first = save_consent_policy_draft_at(&mut tx, ws, &rules, None, 0)
            .await
            .unwrap();
        let again_at_zero = save_consent_policy_draft_at(&mut tx, ws, &rules, None, 0)
            .await
            .unwrap();
        let stale = save_consent_policy_draft_at(&mut tx, ws, &rules, None, 5)
            .await
            .unwrap();
        let second = save_consent_policy_draft_at(&mut tx, ws, &rules, None, 1)
            .await
            .unwrap();

        assert_eq!(first.unwrap().draft_revision, 1);
        assert!(again_at_zero.is_none() && stale.is_none());
        assert_eq!(second.unwrap().draft_revision, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn editing_the_draft_leaves_the_published_version_alone(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        save(&pool, ws, &[PolicyRule::recorded_consent()]).await;
        publish(&pool, ws).await;

        save(&pool, ws, &[custom("a"), PolicyRule::recorded_consent()]).await;

        let published = published_consent_policy(&pool, ws).await.unwrap().unwrap();
        assert_eq!(published.rules, default_rules());
        assert_eq!(consent_policy_draft(&pool, ws).await.unwrap().len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_published_version_is_immutable_and_each_publish_is_a_definition_change_decision(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        save(&pool, ws, &[PolicyRule::recorded_consent()]).await;
        publish(&pool, ws).await;

        let update = sqlx::query("UPDATE consent_policy_versions SET rules = '[]'::jsonb")
            .execute(&pool)
            .await;

        assert!(update.is_err());
        let decisions: Vec<(String, String)> = sqlx::query_as(
            "SELECT action, outcome FROM decision_records WHERE workspace_id = $1 \
             AND action = 'consent_policy.publish'",
        )
        .bind(ws.as_uuid())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            decisions,
            [("consent_policy.publish".into(), "published".into())]
        );
        let cause: String = sqlx::query_scalar(
            "SELECT cause_kind FROM decision_records WHERE workspace_id = $1 \
             AND action = 'consent_policy.publish'",
        )
        .bind(ws.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(cause, "definition_change");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_first_matching_rule_decides_and_the_later_ones_are_not_run(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let person =
            crate::segments::tests_support::person(&pool, ws, "a@example.com", "pro").await;
        let always = SqlQuery {
            sql: "SELECT p.id FROM people p WHERE p.workspace_id = $1 AND p.id = ANY($2::uuid[])"
                .into(),
            params: vec![],
        };
        // A query that cannot run would fail the evaluation if it were reached.
        let broken = SqlQuery {
            sql: "SELECT nope".into(),
            params: vec![SqlParam::Text("x".into())],
        };
        let rule = |id: &str, check: RuleCheck| RuleToCheck {
            id: id.into(),
            name: format!("Rule {id}"),
            check,
        };
        let mut conn = pool.acquire().await.unwrap();

        let decided = evaluate_consent_policy(
            &mut conn,
            ws,
            person,
            &[
                rule(
                    "recorded_consent",
                    RuleCheck::RecordedConsent { denies: false },
                ),
                rule("a", RuleCheck::Query(always.clone())),
                rule("b", RuleCheck::Query(broken)),
            ],
        )
        .await
        .unwrap();
        let none = evaluate_consent_policy(
            &mut conn,
            ws,
            person,
            &[rule(
                "recorded_consent",
                RuleCheck::RecordedConsent { denies: false },
            )],
        )
        .await
        .unwrap();
        let reordered = evaluate_consent_policy(
            &mut conn,
            ws,
            person,
            &[
                rule("a", RuleCheck::Query(always)),
                rule(
                    "recorded_consent",
                    RuleCheck::RecordedConsent { denies: true },
                ),
            ],
        )
        .await
        .unwrap();
        let unevaluable =
            evaluate_consent_policy(&mut conn, ws, person, &[rule("c", RuleCheck::Unevaluable)])
                .await
                .unwrap();

        assert_eq!(decided.as_ref().map(|rule| rule.id.as_str()), Some("a"));
        assert_eq!(none, None);
        assert_eq!(reordered.map(|rule| rule.id), Some("a".to_owned()));
        assert_eq!(decided.map(|rule| rule.unevaluable), Some(false));
        let unevaluable = unevaluable.unwrap();
        assert_eq!(unevaluable.id, "c");
        assert!(
            unevaluable.unevaluable,
            "a rule that cannot be read says so"
        );
    }
}
