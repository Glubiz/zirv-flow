//! The retention windows of a workspace (architecture section 16.3): every effective window with
//! its source, bounds and last run, and the two a workspace admin may shorten (ruling d732a27c).

#[cfg(test)]
mod acceptance;

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{
    AUDIT_DAYS_BOUNDS, DeploymentRetention, IMPORT_FILE_DAYS_BOUNDS, JOB_HISTORY_DAYS_BOUNDS,
    RetentionRun, WorkspaceRetention,
};

use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

/// The last time a window's job finished.
#[derive(Serialize, ToSchema)]
pub(crate) struct LastRunBody {
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    ran_at: OffsetDateTime,
    /// Rows the run removed.
    deleted: i64,
}

/// One retention window.
#[derive(Serialize, ToSchema)]
pub(crate) struct WindowBody {
    /// `events`, `messages`, `decision_log`, `audit`, `ingest_rejects`, `import_files`,
    /// `job_history` or `assistant_transcripts`.
    key: &'static str,
    /// The effective window; null when the assistant stores no transcripts.
    value: Option<i64>,
    /// `days`.
    unit: &'static str,
    /// `workspace` when this workspace's own setting applies, else `deployment`.
    source: &'static str,
    /// The shortest window allowed; null when there is no lower bound.
    min: Option<i64>,
    /// The longest window allowed; null when there is no upper bound.
    max: Option<i64>,
    /// Whether this API changes the window for a workspace admin.
    editable: bool,
    /// The last run of the job that enforces the window; null before its first run.
    last_run: Option<LastRunBody>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RetentionBody {
    /// The eight windows, in a fixed order.
    windows: Vec<WindowBody>,
}

/// The windows a workspace admin may set. A field left out keeps the stored value; an explicit
/// `null` returns that window to the deployment value.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PutRetentionRequest {
    /// Days messages are kept; at least the deployment minimum and at most the deployment window.
    /// Left out keeps the stored value; `null` returns to the deployment value.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<i64>)]
    messages_days: Option<Option<i64>>,
    /// Days decision records are kept, within the same rule and with the same treatment of a
    /// missing field and `null`.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<i64>)]
    decision_days: Option<Option<i64>>,
}

/// Tells an absent field from a `null` one: absent is `None`, `null` is `Some(None)`.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Option<i64>>, D::Error> {
    Option::<i64>::deserialize(deserializer).map(Some)
}

fn window(
    key: &'static str,
    value: Option<u32>,
    bounds: (Option<u32>, Option<u32>),
    runs: &[RetentionRun],
) -> WindowBody {
    WindowBody {
        key,
        value: value.map(i64::from),
        unit: "days",
        source: "deployment",
        min: bounds.0.map(i64::from),
        max: bounds.1.map(i64::from),
        editable: false,
        last_run: runs
            .iter()
            .find(|run| run.window == key)
            .map(|run| LastRunBody {
                ran_at: run.ran_at,
                deleted: run.deleted,
            }),
    }
}

/// A window the workspace may shorten: its own value when set, else the deployment's.
fn editable_window(
    key: &'static str,
    own: Option<i32>,
    (deployment, min, max): (u32, u32, u32),
    runs: &[RetentionRun],
) -> WindowBody {
    let longest = deployment.min(max);
    let mut body = window(key, Some(deployment), (Some(min), Some(longest)), runs);
    body.editable = true;
    if let Some(days) = own.and_then(|days| u32::try_from(days).ok()) {
        body.value = Some(i64::from(days.min(longest)));
        body.source = "workspace";
    }
    body
}

fn body(
    deployment: &DeploymentRetention,
    own: Option<&WorkspaceRetention>,
    transcript_days: Option<i32>,
    runs: &[RetentionRun],
) -> RetentionBody {
    let events = &deployment.events;
    let mut windows = vec![window(
        "events",
        Some(events.default_days),
        (Some(events.min_days), Some(events.max_days)),
        runs,
    )];
    windows.push(editable_window(
        "messages",
        own.and_then(|own| own.messages_days),
        (
            deployment.messages.days,
            deployment.messages.min_days,
            deployment.messages.max_days,
        ),
        runs,
    ));
    windows.push(editable_window(
        "decision_log",
        own.and_then(|own| own.decision_days),
        (
            deployment.decisions.days,
            deployment.decisions.min_days,
            deployment.decisions.max_days,
        ),
        runs,
    ));
    windows.push(window(
        "audit",
        Some(deployment.audit_days),
        (Some(AUDIT_DAYS_BOUNDS.0), Some(AUDIT_DAYS_BOUNDS.1)),
        runs,
    ));
    windows.push(window(
        "ingest_rejects",
        Some(events.reject_days),
        (None, None),
        runs,
    ));
    windows.push(window(
        "import_files",
        Some(deployment.settings.import_file_days),
        (
            Some(IMPORT_FILE_DAYS_BOUNDS.0),
            Some(IMPORT_FILE_DAYS_BOUNDS.1),
        ),
        runs,
    ));
    windows.push(window(
        "job_history",
        Some(deployment.job_history_days),
        (
            Some(JOB_HISTORY_DAYS_BOUNDS.0),
            Some(JOB_HISTORY_DAYS_BOUNDS.1),
        ),
        runs,
    ));
    // The assistant keeps its own control (1 to 30 days, or none), in the AI settings.
    let mut transcripts = window("assistant_transcripts", None, (Some(1), Some(30)), runs);
    transcripts.value = transcript_days.map(i64::from);
    transcripts.source = "workspace";
    windows.push(transcripts);
    RetentionBody { windows }
}

async fn load(state: &AppState, workspace: WorkspaceId) -> Result<RetentionBody, ApiError> {
    let own = zirv_flow_store::find_workspace_retention(&state.pool, workspace).await?;
    let transcript_days = zirv_flow_store::find_ai_settings(&state.pool, workspace)
        .await?
        .and_then(|settings| settings.transcript_days);
    let runs = zirv_flow_store::retention_runs(&state.pool).await?;
    Ok(body(&state.retention, own.as_ref(), transcript_days, &runs))
}

/// Every retention window in force for the workspace, with its source, bounds, whether it can be
/// changed here, and the last run of the job that enforces it. Admins, the owner and `admin` keys
/// only.
#[utoipa::path(
    get,
    operation_id = "get_retention",
    path = "/api/v1/workspaces/{ws}/retention",
    tag = "retention",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The eight windows", body = RetentionBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<RetentionBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    Ok(Json(load(&state, workspace.id).await?))
}

/// Changes the workspace's own message and decision-log windows. A value must be at least the
/// deployment minimum and at most the deployment window; a missing field keeps the stored value
/// and null returns the window to the deployment value. Audit, ingest rejects, import files, job history and events are
/// the deployment's alone, and assistant transcripts stay under the AI settings: any other field
/// is a 422. Audited as `retention.updated`, with window names and day counts only.
#[utoipa::path(
    put,
    operation_id = "put_retention",
    path = "/api/v1/workspaces/{ws}/retention",
    tag = "retention",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = PutRetentionRequest,
    responses(
        (status = 200, description = "The windows after the change", body = RetentionBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An unknown field, or a window outside the deployment bounds or longer than the deployment window (code retention_out_of_bounds, with the bounds in the detail)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn put(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(request): ApiJson<PutRetentionRequest>,
) -> Result<Json<RetentionBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    admin::set_workspace_retention(
        &state.pool,
        &principal.context(),
        &state.retention,
        workspace.id,
        request.messages_days,
        request.decision_days,
    )
    .await?;
    Ok(Json(load(&state, workspace.id).await?))
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    fn run(window: &str) -> RetentionRun {
        RetentionRun {
            window: window.to_owned(),
            ran_at: datetime!(2026-06-01 4:00 UTC),
            deleted: 3,
        }
    }

    #[test]
    fn the_eight_windows_come_in_a_fixed_order_with_deployment_sources() {
        let shown = body(&DeploymentRetention::default(), None, None, &[]);

        let keys: Vec<_> = shown.windows.iter().map(|window| window.key).collect();
        assert_eq!(
            keys,
            [
                "events",
                "messages",
                "decision_log",
                "audit",
                "ingest_rejects",
                "import_files",
                "job_history",
                "assistant_transcripts"
            ]
        );
        let editable: Vec<_> = shown
            .windows
            .iter()
            .filter(|window| window.editable)
            .map(|window| window.key)
            .collect();
        assert_eq!(editable, ["messages", "decision_log"]);
        assert!(
            shown.windows[..7]
                .iter()
                .all(|window| window.source == "deployment" && window.last_run.is_none())
        );
    }

    #[test]
    fn a_workspace_window_is_the_value_and_the_source_and_never_exceeds_the_deployment() {
        let own = WorkspaceRetention {
            messages_days: Some(60),
            decision_days: Some(9999),
            updated_at: datetime!(2026-06-01 4:00 UTC),
            updated_by: None,
        };

        let shown = body(&DeploymentRetention::default(), Some(&own), None, &[]);

        let messages = &shown.windows[1];
        assert_eq!(
            (messages.value, messages.source, messages.min, messages.max),
            (Some(60), "workspace", Some(30), Some(395))
        );
        assert_eq!(shown.windows[2].value, Some(395));
    }

    #[test]
    fn the_transcript_row_follows_the_assistant_setting_and_the_last_run_is_matched_by_key() {
        let shown = body(
            &DeploymentRetention::default(),
            None,
            Some(7),
            &[run("audit"), run("assistant_transcripts")],
        );

        let transcripts = &shown.windows[7];
        assert_eq!(
            (transcripts.value, transcripts.source, transcripts.editable),
            (Some(7), "workspace", false)
        );
        assert!(transcripts.last_run.is_some() && shown.windows[3].last_run.is_some());
        assert!(shown.windows[1].last_run.is_none());
        assert_eq!(
            body(&DeploymentRetention::default(), None, None, &[]).windows[7].value,
            None
        );
    }

    #[test]
    fn the_request_refuses_a_field_it_does_not_own() {
        assert!(serde_json::from_str::<PutRetentionRequest>(r#"{"audit_days":30}"#).is_err());
        let ok: PutRetentionRequest =
            serde_json::from_str(r#"{"messages_days":60,"decision_days":null}"#).unwrap();
        assert_eq!(
            (ok.messages_days, ok.decision_days),
            (Some(Some(60)), Some(None))
        );
        let absent: PutRetentionRequest = serde_json::from_str(r#"{"messages_days":60}"#).unwrap();
        assert_eq!(absent.decision_days, None);
    }
}
