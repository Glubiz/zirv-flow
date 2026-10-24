//! The workspace consent policy (architecture 10.1, OQ-32): a draft that is edited and published
//! like a segment, with the ordered deny-only rules the send gate evaluates. Drafts need the
//! `write` role, publishing and evaluating a person need `admin`.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::consent::policy::{
    PolicyRule, RECORDED_CONSENT_RULE, RulesError, check_rules, default_rules,
};
use zirv_flow_core::consent::{Channel, Dimensions};
use zirv_flow_core::{Cause, TraceContext, UserId, WorkspaceId};
use zirv_flow_segments::{
    Catalog, Context as Mode, Definition, RuleKind, SendContext, TraitStorage, compile_rules,
    validate_in,
};
use zirv_flow_store::{ConsentPolicy, PgPool, PolicyVersion, ReadOutcome, RuleCheck, RuleToCheck};

use super::person::check_dimensions;
use crate::AppState;
use crate::admin::{self, AdminError, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem, SegmentIssueProblem};
use crate::segments::catalog_of;
use crate::trace::request_trace;

/// One rule of a policy as the API shows it.
#[derive(Serialize, ToSchema)]
pub(crate) struct ConsentPolicyRuleBody {
    /// `recorded_consent` for the built-in rule, otherwise a UUID.
    id: String,
    name: String,
    /// The built-in rule: it matches when the recorded consent does not allow the send. It can be
    /// moved but not removed, and has no condition.
    builtin: bool,
    /// A segment definition (`{"v": 1, ...}`) in which the `consent` leaf may leave out its
    /// channel (its `status` is then `granted`, `denied` or `none` for the message being sent)
    /// and the `messages` leaf is allowed. The rule matches, and so denies the send, when the
    /// condition holds for the person. Absent on the built-in rule.
    #[schema(value_type = Option<Object>)]
    condition: Option<Value>,
}

impl From<&PolicyRule> for ConsentPolicyRuleBody {
    fn from(rule: &PolicyRule) -> Self {
        Self {
            id: rule.id.clone(),
            name: rule.name.clone(),
            builtin: rule.is_builtin(),
            condition: rule.condition.clone(),
        }
    }
}

/// A rule in a request.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ConsentPolicyRuleRequest {
    /// `recorded_consent` for the built-in rule; for any other rule a UUID, or absent for a new
    /// rule, which is given one.
    id: Option<String>,
    /// 1 to 100 characters. The built-in rule keeps its own name.
    name: String,
    /// The condition of every rule but the built-in one, which has none.
    #[schema(value_type = Option<Object>)]
    condition: Option<Value>,
}

/// The draft rules in order. The built-in `recorded_consent` rule must be among them.
#[derive(Deserialize, ToSchema)]
pub(crate) struct SaveConsentPolicyDraftRequest {
    /// The revision of the draft being replaced, as the last read returned it (0 when no draft
    /// was ever saved). A draft saved since is a 409 `consent_policy_revision_mismatch`.
    revision: i32,
    rules: Vec<ConsentPolicyRuleRequest>,
}

/// The draft a publish is for.
#[derive(Deserialize, ToSchema)]
pub(crate) struct PublishConsentPolicyRequest {
    /// The revision of the draft the caller reviewed (0 when no draft was ever saved). A draft
    /// saved since is a 409 `consent_policy_revision_mismatch` and nothing is published.
    revision: i32,
}

/// The draft of the workspace's policy.
#[derive(Serialize, ToSchema)]
pub(crate) struct ConsentPolicyDraftBody {
    rules: Vec<ConsentPolicyRuleBody>,
    /// Counts the saves of the draft; 0 before one was saved.
    revision: i32,
    /// The version the send gate evaluates; none before the first publish.
    published_version: Option<i32>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    updated_at: Option<OffsetDateTime>,
}

impl ConsentPolicyDraftBody {
    fn of(policy: Option<&ConsentPolicy>) -> Self {
        match policy {
            Some(policy) => Self {
                rules: policy
                    .draft_rules
                    .iter()
                    .map(ConsentPolicyRuleBody::from)
                    .collect(),
                revision: policy.draft_revision,
                published_version: policy.published_version,
                updated_at: Some(policy.updated_at),
            },
            None => Self {
                rules: default_rules()
                    .iter()
                    .map(ConsentPolicyRuleBody::from)
                    .collect(),
                revision: 0,
                published_version: None,
                updated_at: None,
            },
        }
    }
}

/// One published version of the policy.
#[derive(Serialize, ToSchema)]
pub(crate) struct ConsentPolicyVersionBody {
    version: i32,
    rules: Vec<ConsentPolicyRuleBody>,
    published_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    published_at: OffsetDateTime,
}

impl From<&PolicyVersion> for ConsentPolicyVersionBody {
    fn from(version: &PolicyVersion) -> Self {
        Self {
            version: version.version,
            rules: version
                .rules
                .iter()
                .map(ConsentPolicyRuleBody::from)
                .collect(),
            published_by: version.published_by,
            published_at: version.published_at,
        }
    }
}

/// The send to evaluate the policy for, and the person.
#[derive(Deserialize, ToSchema)]
pub(crate) struct EvaluateConsentPolicyRequest {
    person_id: Uuid,
    /// `email`; the default.
    channel: Option<String>,
    /// The key of the message's topic, if it has one.
    topic_key: Option<String>,
    /// The message's other dimension values, by dimension key.
    #[serde(default)]
    dimensions: BTreeMap<String, String>,
}

/// The rule that denied a send.
#[derive(Serialize, ToSchema)]
pub(crate) struct DecidingRuleBody {
    id: String,
    name: String,
    /// The rule denies because it can no longer be evaluated (something it reads was archived
    /// since it was published), not because its condition holds. Sends are denied the same way.
    unevaluable: bool,
}

/// What the published policy decides for a person and a message. It holds no personal data.
#[derive(Serialize, ToSchema)]
pub(crate) struct ConsentPolicyEvaluationBody {
    /// Whether a marketing send would proceed.
    allowed: bool,
    /// The first rule that matches, which denies the send; none when the send proceeds.
    rule: Option<DecidingRuleBody>,
    /// The published version evaluated; none when the workspace has not published one, in which
    /// case only the built-in rule applies.
    policy_version: Option<i32>,
}

fn issue(pointer: String, code: &str) -> SegmentIssueProblem {
    SegmentIssueProblem {
        pointer,
        code: code.to_owned(),
    }
}

/// Every problem with the rules: the shape of the list first, and only then the conditions, each
/// with a pointer into the request (`/rules/{index}/condition/...`).
fn rule_issues(rules: &[PolicyRule], catalog: &Catalog) -> Vec<SegmentIssueProblem> {
    if let Err(error) = check_rules(rules) {
        let pointer = match error {
            RulesError::TooMany | RulesError::BuiltinMissing => "/rules".to_owned(),
            other => format!("/rules/{}", other.index().unwrap_or_default()),
        };
        return vec![issue(pointer, error.code())];
    }
    let mut issues = Vec::new();
    for (index, rule) in rules.iter().enumerate() {
        let Some(condition) = &rule.condition else {
            continue;
        };
        let at = |pointer: &str| format!("/rules/{index}/condition{pointer}");
        let found = match Definition::from_value(condition) {
            Err(found) => found,
            Ok(definition) => match validate_in(&definition, catalog, Mode::Policy) {
                Ok(()) => continue,
                Err(found) => found,
            },
        };
        issues.extend(
            found
                .into_iter()
                .map(|found| issue(at(&found.pointer), found.code)),
        );
    }
    issues
}

fn rules_of(request: Vec<ConsentPolicyRuleRequest>) -> Vec<PolicyRule> {
    request
        .into_iter()
        .map(|rule| {
            if rule.id.as_deref() == Some(RECORDED_CONSENT_RULE) {
                // The built-in rule keeps its own name; a condition on it is refused by the shape
                // check.
                let mut builtin = PolicyRule::recorded_consent();
                builtin.condition = rule.condition;
                builtin
            } else {
                PolicyRule {
                    id: rule.id.unwrap_or_else(|| Uuid::now_v7().to_string()),
                    name: rule.name,
                    condition: rule.condition,
                }
            }
        })
        .collect()
}

/// Saves the draft after checking the rules and their conditions against the workspace's
/// catalog, and audits it as `consent_policy.saved`.
pub(crate) async fn save_policy_draft(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    saved_by: Option<UserId>,
    request: SaveConsentPolicyDraftRequest,
) -> Result<ConsentPolicy, ApiError> {
    let rules = rules_of(request.rules);
    let mut tx = zirv_flow_store::begin(pool).await?;
    let (catalog, _) = catalog_of(zirv_flow_store::segment_catalog(&mut tx, workspace).await?);
    let issues = rule_issues(&rules, &catalog);
    if !issues.is_empty() {
        return Err(ApiError::consent_policy_invalid(issues));
    }
    let policy = zirv_flow_store::save_consent_policy_draft_at(
        &mut tx,
        workspace,
        &rules,
        saved_by,
        request.revision,
    )
    .await?
    .ok_or_else(ApiError::consent_policy_revision_mismatch)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "consent_policy.saved",
        Some(("consent_policy", policy.id)),
        json!({ "rules": rules.len(), "revision": policy.draft_revision }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(policy)
}

/// Publishes the draft as the next version. The draft is read under the policy's lock and
/// validated again against the catalog of the same transaction, so a condition that read
/// something archived since it was saved is refused. Audited as `consent_policy.published`, and a
/// `consent_policy.publish` decision is appended, caused by a definition change.
pub(crate) async fn publish_policy(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    published_by: Option<UserId>,
    expected_revision: i32,
) -> Result<PolicyVersion, ApiError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let locked = match zirv_flow_store::lock_consent_policy(&mut tx, workspace).await? {
        // The revision is compared under the lock, so the draft that is published is the one the
        // caller reviewed.
        Some(locked) if locked.draft_revision != expected_revision => {
            return Err(ApiError::consent_policy_revision_mismatch());
        }
        Some(locked) => locked,
        None if expected_revision != 0 => {
            return Err(ApiError::consent_policy_revision_mismatch());
        }
        None => {
            // Nothing was ever saved: publishing makes the default policy explicit. A first draft
            // saved since the lock found no row is not overwritten: it is a revision mismatch.
            zirv_flow_store::save_consent_policy_draft_at(
                &mut tx,
                workspace,
                &default_rules(),
                published_by,
                0,
            )
            .await?
            .ok_or_else(ApiError::consent_policy_revision_mismatch)?;
            zirv_flow_store::lock_consent_policy(&mut tx, workspace)
                .await?
                .ok_or_else(ApiError::internal)?
        }
    };
    let (catalog, _) = catalog_of(zirv_flow_store::segment_catalog(&mut tx, workspace).await?);
    let issues = rule_issues(&locked.draft_rules, &catalog);
    if !issues.is_empty() {
        return Err(ApiError::consent_policy_invalid(issues));
    }
    let trace = TraceContext {
        trace_id: request_trace().trace_id,
        cause: Cause::definition_change(locked.id),
    };
    let version =
        zirv_flow_store::publish_consent_policy(&mut tx, workspace, &locked, published_by, &trace)
            .await?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "consent_policy.published",
        Some(("consent_policy", locked.id)),
        json!({ "version": version.version, "rules": version.rules.len() }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(version)
}

/// What the published policy decides for `person` and the message the request names: the same
/// read, compilation and ordered evaluation as gate 3 of a send, without sending.
pub(crate) async fn evaluate_policy(
    pool: &PgPool,
    workspace: WorkspaceId,
    request: &EvaluateConsentPolicyRequest,
) -> Result<ConsentPolicyEvaluationBody, AdminError> {
    let channel: Channel = request.channel.as_deref().unwrap_or("email").parse()?;
    let dimensions: Dimensions = check_dimensions(&request.dimensions)?;
    let topic = match request.topic_key.as_deref() {
        None => None,
        Some(key) => Some(
            zirv_flow_store::find_topic_by_key(pool, workspace, key)
                .await?
                .ok_or_else(|| super::invalid("topic_key", "is not a topic of this workspace"))?
                .id,
        ),
    };
    let (verdict, recorded) = match zirv_flow_store::consent_read(
        pool,
        workspace,
        request.person_id,
        channel,
        topic,
        &dimensions,
    )
    .await?
    {
        ReadOutcome::Read {
            verdict, recorded, ..
        } => (verdict, recorded),
        ReadOutcome::NoSuchPerson => return Err(AdminError::NotFound("person")),
        ReadOutcome::NoSuchTopic => {
            return Err(super::invalid(
                "topic_key",
                "is not a topic of this channel",
            ));
        }
        ReadOutcome::NoSuchDimension => {
            return Err(super::invalid(
                "dimensions",
                "names a dimension or value that is not live in this workspace",
            ));
        }
    };
    let recorded_denies = !verdict.allowed;
    let published = zirv_flow_store::published_consent_policy(pool, workspace).await?;
    let rules = match &published {
        Some(version) => version.rules.clone(),
        None => default_rules(),
    };
    let mut connection = zirv_flow_store::begin(pool).await?;
    let now = zirv_flow_store::database_now(&mut *connection).await?;
    let (catalog, storage) = if rules.iter().any(|rule| rule.condition.is_some()) {
        catalog_of(zirv_flow_store::segment_catalog(&mut connection, workspace).await?)
    } else {
        (Catalog::default(), TraitStorage::default())
    };
    let send = SendContext {
        channel,
        dimensions,
        consent: recorded,
    };
    let checks: Vec<RuleToCheck> = compile_rules(&rules, &catalog, &storage, now, &send)
        .into_iter()
        .map(|rule| RuleToCheck {
            id: rule.id,
            name: rule.name,
            check: match rule.kind {
                RuleKind::RecordedConsent => RuleCheck::RecordedConsent {
                    denies: recorded_denies,
                },
                RuleKind::Query(query) => {
                    RuleCheck::Query(crate::segments::preview::sql_query(query))
                }
                RuleKind::Unevaluable => RuleCheck::Unevaluable,
            },
        })
        .collect();
    let deciding = zirv_flow_store::evaluate_consent_policy(
        &mut connection,
        workspace,
        request.person_id,
        &checks,
    )
    .await?;
    Ok(ConsentPolicyEvaluationBody {
        allowed: deciding.is_none(),
        rule: deciding.map(|rule| DecidingRuleBody {
            id: rule.id,
            name: rule.name,
            unevaluable: rule.unevaluable,
        }),
        policy_version: published.map(|version| version.version),
    })
}

/// The draft of the workspace's consent policy: the saved draft, or the built-in rule alone when
/// none was saved.
#[utoipa::path(
    get,
    operation_id = "get_consent_policy_draft",
    path = "/api/v1/workspaces/{ws}/consent/policy/draft",
    tag = "consent",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The draft", body = ConsentPolicyDraftBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get_draft(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<ConsentPolicyDraftBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let policy = zirv_flow_store::find_consent_policy(&state.pool, workspace.id).await?;
    Ok(Json(ConsentPolicyDraftBody::of(policy.as_ref())))
}

/// Replaces the draft. The rules are checked as a list (the built-in `recorded_consent` rule is
/// present, ids are unique, names are 1 to 100 characters) and each condition is validated as a
/// policy condition against the workspace's catalog; any problem is a 422
/// `consent_policy_invalid` that lists every issue with its pointer and writes nothing. The
/// published version is not changed. Audited as `consent_policy.saved`.
#[utoipa::path(
    put,
    operation_id = "save_consent_policy_draft",
    path = "/api/v1/workspaces/{ws}/consent/policy/draft",
    tag = "consent",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = SaveConsentPolicyDraftRequest,
    responses(
        (status = 200, description = "The saved draft", body = ConsentPolicyDraftBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The draft is not at the revision named", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The rules or a condition are not valid", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn put_draft(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<SaveConsentPolicyDraftRequest>,
) -> Result<Json<ConsentPolicyDraftBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let policy = save_policy_draft(
        &state.pool,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        body,
    )
    .await?;
    Ok(Json(ConsentPolicyDraftBody::of(Some(&policy))))
}

/// Publishes the draft as the next immutable version, which the send gate then evaluates. The
/// draft is validated again; a problem is a 422 `consent_policy_invalid` and nothing is written.
/// Appends a `consent_policy.publish` decision caused by a definition change. Audited as
/// `consent_policy.published`.
#[utoipa::path(
    post,
    operation_id = "publish_consent_policy",
    path = "/api/v1/workspaces/{ws}/consent/policy/publish",
    tag = "consent",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = PublishConsentPolicyRequest,
    responses(
        (status = 200, description = "The new version", body = ConsentPolicyVersionBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The draft is not at the revision named", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The draft is not valid", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn publish(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<PublishConsentPolicyRequest>,
) -> Result<Json<ConsentPolicyVersionBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let version = publish_policy(
        &state.pool,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        body.revision,
    )
    .await?;
    Ok(Json(ConsentPolicyVersionBody::from(&version)))
}

/// The published versions of the policy, newest first.
#[utoipa::path(
    get,
    operation_id = "list_consent_policy_versions",
    path = "/api/v1/workspaces/{ws}/consent/policy/versions",
    tag = "consent",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The versions, newest first", body = Vec<ConsentPolicyVersionBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a key without the admin scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn versions(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<Vec<ConsentPolicyVersionBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let versions = zirv_flow_store::list_consent_policy_versions(&state.pool, workspace.id).await?;
    Ok(Json(
        versions
            .iter()
            .map(ConsentPolicyVersionBody::from)
            .collect(),
    ))
}

/// Evaluates the published policy for one person and one message, as gate 3 of a send would, and
/// names the rule that denies it. The reply holds a rule id and name or none, and no personal
/// data. A workspace without a published policy is evaluated with the built-in rule alone.
#[utoipa::path(
    post,
    operation_id = "evaluate_consent_policy",
    path = "/api/v1/workspaces/{ws}/consent/policy/evaluate",
    tag = "consent",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = EvaluateConsentPolicyRequest,
    responses(
        (status = 200, description = "What the policy decides", body = ConsentPolicyEvaluationBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or person, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid channel, topic or dimension", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn evaluate(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<EvaluateConsentPolicyRequest>,
) -> Result<Json<ConsentPolicyEvaluationBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    Ok(Json(
        evaluate_policy(&state.pool, workspace.id, &body).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: Option<&str>, name: &str, condition: Option<Value>) -> ConsentPolicyRuleRequest {
        ConsentPolicyRuleRequest {
            id: id.map(str::to_owned),
            name: name.to_owned(),
            condition,
        }
    }

    fn policy_condition() -> Value {
        json!({"v": 1, "consent": {"status": "none"}})
    }

    #[test]
    fn a_request_becomes_rules_with_ids_for_the_new_ones_and_the_built_in_rule_renamed() {
        let rules = rules_of(vec![
            request(None, "Soft opt-in", Some(policy_condition())),
            request(Some("recorded_consent"), "Whatever", None),
        ]);

        assert!(Uuid::parse_str(&rules[0].id).is_ok());
        assert_eq!(rules[0].name, "Soft opt-in");
        assert_eq!(rules[1], PolicyRule::recorded_consent());
    }

    #[test]
    fn a_list_without_the_built_in_rule_is_refused_at_the_rules() {
        let rules = rules_of(vec![request(None, "A", Some(policy_condition()))]);

        let issues = rule_issues(&rules, &Catalog::default());

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].pointer, "/rules");
        assert_eq!(issues[0].code, "builtin_rule_required");
    }

    #[test]
    fn an_invalid_condition_is_refused_with_a_pointer_into_the_request() {
        let rules = rules_of(vec![
            request(
                None,
                "Bad",
                Some(json!({"v": 1, "trait": "ghost", "op": "exists"})),
            ),
            request(
                None,
                "Segment style",
                Some(json!({"v": 1, "consent": {"channel": "email", "status": "granted"}})),
            ),
            request(Some("recorded_consent"), "", None),
        ]);

        let issues = rule_issues(&rules, &Catalog::default());

        let pairs: Vec<(&str, &str)> = issues
            .iter()
            .map(|found| (found.pointer.as_str(), found.code.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("/rules/0/condition/trait", "unknown_trait"),
                ("/rules/1/condition/consent/status", "value_not_allowed"),
            ]
        );
    }

    #[test]
    fn a_policy_condition_with_the_policy_leaves_is_accepted() {
        let rules = rules_of(vec![
            request(
                None,
                "Cooldown",
                Some(json!({"v": 1, "messages": {"channel": "email",
                    "agg": {"fn": "last_time", "op": "within_last", "value": "P1D"}}})),
            ),
            request(Some("recorded_consent"), "", None),
        ]);

        assert!(rule_issues(&rules, &Catalog::default()).is_empty());
    }

    #[test]
    fn a_condition_that_is_not_a_definition_is_a_parse_issue_at_its_place() {
        let rules = rules_of(vec![
            request(None, "Junk", Some(json!({"nope": true}))),
            request(Some("recorded_consent"), "", None),
        ]);

        let issues = rule_issues(&rules, &Catalog::default());

        assert!(
            issues
                .iter()
                .all(|found| found.pointer.starts_with("/rules/0/condition"))
        );
        assert!(!issues.is_empty());
    }

    #[test]
    fn the_draft_before_any_save_is_the_built_in_rule_alone() {
        let body = serde_json::to_value(ConsentPolicyDraftBody::of(None)).unwrap();

        assert_eq!(body["revision"], 0);
        assert_eq!(body["rules"][0]["id"], "recorded_consent");
        assert_eq!(body["rules"][0]["builtin"], true);
        assert_eq!(body["rules"][0].get("condition"), Some(&Value::Null));
        assert_eq!(body["published_version"], Value::Null);
    }
}
