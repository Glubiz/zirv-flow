//! The workspace event policy: unplanned names, time limits and ingest redaction.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use zirv_flow_core::events::{RedactionRules, UnplannedPolicy, validate_rules};
use zirv_flow_core::{ValidationError, WorkspaceId};
use zirv_flow_store::{EventPolicy, PgPool};

use crate::AppState;
use crate::admin::{self, AdminError, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

/// A day: the most a timestamp may lie ahead of the receipt time.
const MAX_FUTURE_SKEW_SECONDS: i32 = 86_400;
/// A year: the most the lateness window may be.
const MAX_LATENESS_SECONDS: i32 = 31_536_000;

/// Which properties are redacted before storage. Paths are dot-separated and reach into
/// nested objects.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub(crate) struct RedactionBody {
    /// Removed.
    #[serde(default)]
    drop: Vec<String>,
    /// Replaced by a keyed hash.
    #[serde(default)]
    hash: Vec<String>,
    /// IP addresses with the host part zeroed: the last IPv4 octet, or all but the first
    /// 48 bits of an IPv6 address. A value that is not an address is removed.
    #[serde(default)]
    truncate_ip: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub(crate) struct EventPolicyBody {
    /// What happens to an event whose name has no published schema: `draft` accepts it and
    /// drafts a schema from its shape; `reject` refuses it.
    #[schema(example = "draft")]
    unplanned: String,
    /// Seconds a timestamp may lie ahead of the receipt time before it is clamped, 0 to 86400.
    future_skew_seconds: i32,
    /// Seconds before an event counts as late, 0 to 31536000.
    lateness_seconds: i32,
    redaction: RedactionBody,
}

impl From<&EventPolicy> for EventPolicyBody {
    fn from(policy: &EventPolicy) -> Self {
        Self {
            unplanned: policy.unplanned.as_str().to_owned(),
            future_skew_seconds: policy.future_skew_seconds,
            lateness_seconds: policy.lateness_seconds,
            redaction: RedactionBody {
                drop: policy.redaction.drop.clone(),
                hash: policy.redaction.hash.clone(),
                truncate_ip: policy.redaction.truncate_ip.clone(),
            },
        }
    }
}

impl EventPolicyBody {
    fn into_policy(self) -> Result<EventPolicy, ValidationError> {
        let unplanned: UnplannedPolicy = self.unplanned.parse()?;
        if !(0..=MAX_FUTURE_SKEW_SECONDS).contains(&self.future_skew_seconds) {
            return Err(ValidationError::new(
                "future_skew_seconds",
                "must be from 0 to 86400",
            ));
        }
        if !(0..=MAX_LATENESS_SECONDS).contains(&self.lateness_seconds) {
            return Err(ValidationError::new(
                "lateness_seconds",
                "must be from 0 to 31536000",
            ));
        }
        let redaction = RedactionRules {
            drop: self.redaction.drop,
            hash: self.redaction.hash,
            truncate_ip: self.redaction.truncate_ip,
        };
        validate_rules(&redaction)?;
        Ok(EventPolicy {
            unplanned,
            future_skew_seconds: self.future_skew_seconds,
            lateness_seconds: self.lateness_seconds,
            redaction,
        })
    }
}

/// Replaces the policy and audits the names of the fields that changed.
pub(crate) async fn save_policy(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    request: EventPolicyBody,
) -> Result<EventPolicy, AdminError> {
    let policy = request.into_policy()?;
    let mut tx = zirv_flow_store::begin(pool).await?;
    let before = zirv_flow_store::get_event_policy(&mut *tx, workspace).await?;
    zirv_flow_store::set_event_policy(&mut *tx, workspace, &policy).await?;
    let fields: Vec<&str> = [
        ("unplanned", before.unplanned != policy.unplanned),
        (
            "future_skew_seconds",
            before.future_skew_seconds != policy.future_skew_seconds,
        ),
        (
            "lateness_seconds",
            before.lateness_seconds != policy.lateness_seconds,
        ),
        ("redaction", before.redaction != policy.redaction),
    ]
    .into_iter()
    .filter_map(|(field, changed)| changed.then_some(field))
    .collect();
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "event_policy.updated",
        None,
        json!({ "fields": fields }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(policy)
}

/// The workspace's event policy, or the defaults while none was saved.
#[utoipa::path(
    get,
    operation_id = "get_event_policy",
    path = "/api/v1/workspaces/{ws}/event-policy",
    tag = "events",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The policy", body = EventPolicyBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<EventPolicyBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let policy = zirv_flow_store::get_event_policy(&state.pool, workspace.id).await?;
    Ok(Json(EventPolicyBody::from(&policy)))
}

/// Replaces the policy.
#[utoipa::path(
    put,
    operation_id = "put_event_policy",
    path = "/api/v1/workspaces/{ws}/event-policy",
    tag = "events",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = EventPolicyBody,
    responses(
        (status = 200, description = "The saved policy", body = EventPolicyBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid policy value, time limit or redaction path", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn put(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<EventPolicyBody>,
) -> Result<Json<EventPolicyBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let policy = save_policy(&state.pool, &principal.context(), workspace.id, body).await?;
    Ok(Json(EventPolicyBody::from(&policy)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(json: serde_json::Value) -> EventPolicyBody {
        serde_json::from_value(json).unwrap()
    }

    fn valid() -> serde_json::Value {
        json!({"unplanned": "reject", "future_skew_seconds": 60, "lateness_seconds": 3600,
               "redaction": {"drop": ["card"]}})
    }

    #[test]
    fn a_valid_body_becomes_a_policy_and_back() {
        let policy = body(valid()).into_policy().unwrap();

        assert_eq!(policy.unplanned, UnplannedPolicy::Reject);
        assert_eq!(policy.redaction.drop, ["card"]);
        assert!(policy.redaction.truncate_ip.is_empty());
        assert_eq!(EventPolicyBody::from(&policy).lateness_seconds, 3600);
    }

    #[test]
    fn the_policy_value_the_time_limits_and_the_paths_are_checked() {
        let cases = [
            json!({"unplanned": "ignore", "future_skew_seconds": 1, "lateness_seconds": 1, "redaction": {}}),
            json!({"unplanned": "draft", "future_skew_seconds": -1, "lateness_seconds": 1, "redaction": {}}),
            json!({"unplanned": "draft", "future_skew_seconds": 86401, "lateness_seconds": 1, "redaction": {}}),
            json!({"unplanned": "draft", "future_skew_seconds": 1, "lateness_seconds": 31536001, "redaction": {}}),
            json!({"unplanned": "draft", "future_skew_seconds": 1, "lateness_seconds": 1, "redaction": {"hash": ["a..b"]}}),
        ];
        for case in cases {
            assert!(body(case.clone()).into_policy().is_err(), "{case}");
        }
    }
}
