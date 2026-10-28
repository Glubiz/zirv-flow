//! Forwarding rules: create, read, change and delete. A rule names an exact event name, an
//! optional property filter (the segment `where` list), a mapping from the event to the
//! destination's payload, and a destination that declares the `event` channel.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::events::validate_event_name;
use zirv_flow_core::forwarding::{Mapping, parse_filter};
use zirv_flow_core::segment::{Issue, parse};
use zirv_flow_core::{UserId, WorkspaceId};
use zirv_flow_segments::validate;
use zirv_flow_store::{
    ForwardingRule, NewRule, PgConnection, PgPool, RuleUpdate, RuleWrite, segment_catalog,
};

use super::{destination_unsupported, issues_problem, too_many_rules};
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::destinations::registry;
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::segments::catalog_of;
use crate::sources::crud::check_name;
use crate::sources::{invalid, problems};

#[derive(Serialize, ToSchema)]
pub(crate) struct RuleBody {
    id: Uuid,
    /// The destination the selected events are delivered to.
    destination_id: Uuid,
    #[schema(example = "Paid orders")]
    name: String,
    /// The exact name of the events the rule selects.
    #[schema(example = "order.completed")]
    event_name: String,
    /// Property predicates that must all hold, in the segment `where` form.
    #[schema(value_type = Vec<serde_json::Value>)]
    filter: Value,
    /// Output key to source. A source is `{"from": "event.id"}`, `event.name`, `event.time`,
    /// `event.received_at`, `person.id`, `{"from": "event.properties", "pointer": "/a/b"}`, or
    /// `{"value": <string|number|boolean>}`.
    #[schema(value_type = BTreeMap<String, serde_json::Value>)]
    mapping: Value,
    active: bool,
    created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl From<ForwardingRule> for RuleBody {
    fn from(rule: ForwardingRule) -> Self {
        Self {
            id: rule.id,
            destination_id: rule.destination_id,
            name: rule.name,
            event_name: rule.event_name,
            filter: rule.filter,
            mapping: rule.mapping,
            active: rule.active,
            created_by: rule.created_by,
            created_at: rule.created_at,
            updated_at: rule.updated_at,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateRuleRequest {
    destination_id: Uuid,
    /// 1 to 128 characters; unique in the workspace, ignoring case.
    name: String,
    event_name: String,
    /// Default: no predicate.
    #[schema(value_type = Option<Vec<serde_json::Value>>)]
    filter: Option<Value>,
    #[schema(value_type = BTreeMap<String, serde_json::Value>)]
    mapping: Value,
    /// Default true. At most 20 rules are active in a workspace.
    active: Option<bool>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateRuleRequest {
    name: Option<String>,
    event_name: Option<String>,
    #[schema(value_type = Option<Vec<serde_json::Value>>)]
    filter: Option<Value>,
    #[schema(value_type = Option<BTreeMap<String, serde_json::Value>>)]
    mapping: Option<Value>,
    active: Option<bool>,
}

/// Checks the destination of a rule: live, and of a type that declares the `event` channel.
async fn check_destination(
    pool: &PgPool,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<(), ApiError> {
    let destination = zirv_flow_store::find_destination(pool, workspace, id)
        .await?
        .filter(|destination| destination.archived_at.is_none())
        .ok_or_else(|| {
            invalid(
                "destination_id",
                "is not a live destination of this workspace",
            )
        })?;
    let carries_events = registry()
        .get(&destination.kind)
        .is_some_and(|driver| driver.spec().capabilities.channels.contains(&"event"));
    if carries_events {
        Ok(())
    } else {
        Err(destination_unsupported())
    }
}

fn check_event_name(name: &str) -> Result<(), ApiError> {
    validate_event_name(name).map_err(|_| {
        invalid(
            "event_name",
            "must be 1 to 128 characters without control characters, and not . or ..",
        )
    })
}

fn check_mapping(mapping: &Value) -> Result<(), ApiError> {
    Mapping::parse(mapping)
        .map(|_| ())
        .map_err(|issues| issues_problem("mapping", issues))
}

/// Checks the filter: the operators forwarding supports, then the segment validator against the
/// workspace's event schema. A filter with predicates needs the event to have a published schema,
/// which says which properties exist and their types.
async fn check_filter(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    event_name: &str,
    filter: &Value,
) -> Result<(), ApiError> {
    let filters =
        parse_filter(event_name, filter).map_err(|issues| issues_problem("filter", issues))?;
    if filters.is_empty() {
        return Ok(());
    }
    let snapshot = segment_catalog(connection, workspace).await?;
    let (catalog, _) = catalog_of(snapshot);
    let definition = parse(&zirv_flow_core::forwarding::filter_definition(
        event_name, filter,
    ))
    .map_err(filter_issues)?;
    validate(&definition, &catalog).map_err(filter_issues)
}

fn filter_issues(issues: Vec<Issue>) -> ApiError {
    problems(
        issues
            .into_iter()
            .map(|issue| (format!("filter{}", issue.pointer), issue.code.to_owned()))
            .collect(),
    )
}

/// Creates a rule and audits `forwarding.rule_created`, in one transaction.
pub(crate) async fn create_rule(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    created_by: Option<UserId>,
    request: &CreateRuleRequest,
) -> Result<ForwardingRule, ApiError> {
    let name = check_name(&request.name)?;
    check_event_name(&request.event_name)?;
    check_mapping(&request.mapping)?;
    let filter = request.filter.clone().unwrap_or_else(|| json!([]));
    let mut connection = state
        .pool
        .acquire()
        .await
        .map_err(|_| ApiError::internal())?;
    check_filter(&mut connection, workspace, &request.event_name, &filter).await?;
    drop(connection);
    check_destination(&state.pool, workspace, request.destination_id).await?;
    crate::destinations::actions::check_destination(
        state,
        context,
        workspace,
        request.destination_id,
    )
    .await?;

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let written = zirv_flow_store::create_rule(
        &mut tx,
        workspace,
        &NewRule {
            id: Uuid::now_v7(),
            destination_id: request.destination_id,
            name,
            event_name: &request.event_name,
            filter: &filter,
            mapping: &request.mapping,
            active: request.active.unwrap_or(true),
            created_by,
        },
    )
    .await
    .map_err(name_taken)?;
    let rule = match written {
        RuleWrite::Saved(rule) => rule,
        RuleWrite::TooManyRules => return Err(too_many_rules()),
        RuleWrite::NotFound => return Err(ApiError::not_found()),
    };
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "forwarding.rule_created",
        Some(("forwarding_rule", rule.id)),
        json!({"destination_id": rule.destination_id, "active": rule.active}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(rule)
}

fn name_taken(error: zirv_flow_store::StoreError) -> ApiError {
    if error.is_conflict() {
        ApiError::new(
            StatusCode::CONFLICT,
            "a forwarding rule with that name already exists",
        )
    } else {
        error.into()
    }
}

/// Changes a rule and audits `forwarding.rule_updated` with the names of the changed fields.
///
/// The rule is read and locked in the transaction that writes it, the patch is merged into that
/// state, and the merged rule is validated before the write. Two concurrent changes (one to the
/// filter, one to the event name) therefore cannot leave a filter that was never checked against
/// the final event name.
pub(crate) async fn update_rule(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    request: &UpdateRuleRequest,
) -> Result<ForwardingRule, ApiError> {
    // What the request alone can show wrong needs no lock.
    let mut fields: Vec<&'static str> = Vec::new();
    let name = request.name.as_deref().map(check_name).transpose()?;
    if name.is_some() {
        fields.push("name");
    }
    if let Some(event_name) = &request.event_name {
        check_event_name(event_name)?;
        fields.push("event_name");
    }
    if let Some(mapping) = &request.mapping {
        check_mapping(mapping)?;
        fields.push("mapping");
    }
    if request.filter.is_some() {
        fields.push("filter");
    }
    if request.active.is_some() {
        fields.push("active");
    }
    if fields.is_empty() {
        return Err(invalid("body", "must change at least one field"));
    }

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let current = zirv_flow_store::lock_rule(&mut tx, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    // A changed event name or filter is checked against the rule as it will be.
    if request.filter.is_some() || request.event_name.is_some() {
        let event_name = request.event_name.as_deref().unwrap_or(&current.event_name);
        let filter = request.filter.as_ref().unwrap_or(&current.filter);
        check_filter(&mut tx, workspace, event_name, filter).await?;
    }
    let written = zirv_flow_store::update_rule(
        &mut tx,
        workspace,
        id,
        &RuleUpdate {
            name,
            event_name: request.event_name.as_deref(),
            filter: request.filter.as_ref(),
            mapping: request.mapping.as_ref(),
            active: request.active,
        },
    )
    .await
    .map_err(name_taken)?;
    let rule = match written {
        RuleWrite::Saved(rule) => rule,
        RuleWrite::TooManyRules => return Err(too_many_rules()),
        RuleWrite::NotFound => return Err(ApiError::not_found()),
    };
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "forwarding.rule_updated",
        Some(("forwarding_rule", rule.id)),
        json!({"fields": fields}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(rule)
}

/// The workspace's rules, ordered by name.
#[utoipa::path(
    get,
    operation_id = "list_forwarding_rules",
    path = "/api/v1/workspaces/{ws}/forwarding/rules",
    tag = "forwarding",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The rules", body = Vec<RuleBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a missing scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<Vec<RuleBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let mut connection = state
        .pool
        .acquire()
        .await
        .map_err(|_| ApiError::internal())?;
    let rules = zirv_flow_store::list_rules(&mut connection, workspace.id).await?;
    Ok(Json(rules.into_iter().map(RuleBody::from).collect()))
}

/// Creates a rule. Audited as `forwarding.rule_created`.
#[utoipa::path(
    post,
    operation_id = "create_forwarding_rule",
    path = "/api/v1/workspaces/{ws}/forwarding/rules",
    tag = "forwarding",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateRuleRequest,
    responses(
        (status = 201, description = "The new rule", body = RuleBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A rule with that name exists", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid field; `too_many_rules` when 20 rules are active; `destination_unsupported` when the destination cannot carry events; `egress_denied` when its address is refused", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateRuleRequest>,
) -> Result<(StatusCode, Json<RuleBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let rule = create_rule(
        &state,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        &body,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(RuleBody::from(rule))))
}

/// One rule.
#[utoipa::path(
    get,
    operation_id = "get_forwarding_rule",
    path = "/api/v1/workspaces/{ws}/forwarding/rules/{id}",
    tag = "forwarding",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Rule id")
    ),
    responses(
        (status = 200, description = "The rule", body = RuleBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a missing scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or rule, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<RuleBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let mut connection = state
        .pool
        .acquire()
        .await
        .map_err(|_| ApiError::internal())?;
    let rule = zirv_flow_store::find_rule(&mut connection, workspace.id, path_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(RuleBody::from(rule)))
}

/// Changes a rule. Audited as `forwarding.rule_updated`.
#[utoipa::path(
    patch,
    operation_id = "update_forwarding_rule",
    path = "/api/v1/workspaces/{ws}/forwarding/rules/{id}",
    tag = "forwarding",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Rule id")
    ),
    request_body = UpdateRuleRequest,
    responses(
        (status = 200, description = "The updated rule", body = RuleBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or rule, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Another rule has that name", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid field, no field at all, or `too_many_rules` when turning a rule on", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateRuleRequest>,
) -> Result<Json<RuleBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let rule = update_rule(
        &state,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        &body,
    )
    .await?;
    Ok(Json(RuleBody::from(rule)))
}

/// Deletes a rule. Rows it already queued are still delivered. Audited as
/// `forwarding.rule_deleted`.
#[utoipa::path(
    delete,
    operation_id = "delete_forwarding_rule",
    path = "/api/v1/workspaces/{ws}/forwarding/rules/{id}",
    tag = "forwarding",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Rule id")
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or rule, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let rule = zirv_flow_store::delete_rule(&mut tx, workspace.id, path_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "forwarding.rule_deleted",
        Some(("forwarding_rule", rule.id)),
        json!({}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_name_is_checked_like_an_event_name() {
        assert!(check_event_name("order.completed").is_ok());
        assert!(check_event_name("segment.entered").is_ok());
        for bad in ["", ".", "..", "a\nb"] {
            assert_eq!(
                check_event_name(bad).unwrap_err().status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_mapping_is_checked_by_the_core_parser() {
        assert!(check_mapping(&json!({"id": {"from": "event.id"}})).is_ok());
        let error = check_mapping(&json!({"id": {"from": "event.nope"}})).unwrap_err();
        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(error.detail().contains("mapping/id unknown_source"));
    }

    #[test]
    fn filter_issues_carry_a_pointer_and_a_code_only() {
        let error = filter_issues(vec![Issue::new("/where/0/prop", "unknown_property")]);

        assert!(
            error
                .detail()
                .contains("filter/where/0/prop unknown_property")
        );
    }

    #[test]
    fn a_rule_body_has_no_secret_and_keeps_the_wire_forms() {
        let body = RuleBody::from(ForwardingRule {
            id: Uuid::nil(),
            destination_id: Uuid::nil(),
            name: "n".into(),
            event_name: "e".into(),
            filter: json!([]),
            mapping: json!({"a": {"value": 1}}),
            active: true,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        });
        let json = serde_json::to_value(body).unwrap();

        assert_eq!(json["mapping"], json!({"a": {"value": 1}}));
        assert_eq!(json["filter"], json!([]));
    }
}
