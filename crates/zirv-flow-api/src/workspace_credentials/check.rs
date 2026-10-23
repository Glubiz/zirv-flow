//! Testing a workspace credential: it runs the check of a source or destination that uses it,
//! records when and how it went, and answers with a fixed sentence and a code. The answer names no
//! host, no URL and no value, as for the destination check.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_connectors::http::HttpError;
use zirv_flow_connectors::sources::SourceError;
use zirv_flow_core::WorkspaceId;

use super::archived;
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::destinations::actions::check_destination;
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::sources::actions::{CALL_TIMEOUT, prepare, timed_out};
use crate::sources::invalid;

/// Which source or destination runs the check. Exactly one id.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TestCredentialRequest {
    /// A source that uses this credential.
    source_id: Option<Uuid>,
    /// A destination that uses this credential.
    destination_id: Option<Uuid>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CredentialTestBody {
    /// Whether the check passed.
    ok: bool,
    /// A fixed sentence for `code`. Never holds a credential, a host, an address or a URL; null
    /// when the check passed.
    message: Option<String>,
    /// A stable code for the failure: `config_invalid`, `credential_refused`, `unreachable`,
    /// `egress_denied`, `network_error`, `upstream_error`, `rate_limited`, `unsupported` or
    /// `failed`. Null when the check passed.
    code: Option<String>,
}

/// The code and the fixed sentence for a failed source check. The connector's own text can name a
/// host (an egress denial does), so it is never passed on.
fn describe_source(error: &SourceError) -> (&'static str, &'static str) {
    match error {
        SourceError::Config { .. } => ("config_invalid", "a setting of the source is unusable"),
        SourceError::Auth => ("credential_refused", "the source refused the credential"),
        SourceError::Unreachable => ("unreachable", "the source could not be reached"),
        SourceError::Egress(HttpError::EgressDenied { .. }) => (
            "egress_denied",
            "the source's address is not allowed by the egress policy",
        ),
        SourceError::Egress(_) => ("network_error", "the request to the source failed"),
        SourceError::Upstream(_) => ("upstream_error", "the source answered with an error"),
        SourceError::Other(_) => ("failed", "the check failed"),
    }
}

fn outcome(failure: Option<(&str, &str)>) -> CredentialTestBody {
    CredentialTestBody {
        ok: failure.is_none(),
        message: failure.map(|(_, message)| message.to_owned()),
        code: failure.map(|(code, _)| code.to_owned()),
    }
}

/// Runs the check of the named source or destination, which must use the credential, records the
/// outcome on the credential and audits `credential.tested` with the outcome and its code.
pub(crate) async fn test_credential(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    request: &TestCredentialRequest,
) -> Result<CredentialTestBody, ApiError> {
    let credential = zirv_flow_store::find_credential(&state.pool, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if credential.archived_at.is_some() {
        return Err(archived());
    }
    let (owner, body) = match (request.source_id, request.destination_id) {
        (Some(source_id), None) => {
            let source = zirv_flow_store::find_source(&state.pool, workspace, source_id)
                .await?
                .ok_or_else(|| invalid("source_id", "is not a source of this workspace"))?;
            if source.credential_id != Some(id) {
                return Err(invalid("source_id", "does not use this credential"));
            }
            let call = prepare(state, workspace, source_id).await?;
            let result = tokio::time::timeout(
                CALL_TIMEOUT,
                call.driver
                    .check(&call.context, &call.source.config, &call.credential),
            )
            .await
            .map_err(|_| timed_out())?;
            (
                "source",
                outcome(result.err().as_ref().map(describe_source)),
            )
        }
        (None, Some(destination_id)) => {
            let destination =
                zirv_flow_store::find_destination(&state.pool, workspace, destination_id)
                    .await?
                    .ok_or_else(|| {
                        invalid("destination_id", "is not a destination of this workspace")
                    })?;
            if destination.credential_id != Some(id) {
                return Err(invalid("destination_id", "does not use this credential"));
            }
            let checked = check_destination(state, context, workspace, destination_id).await?;
            (
                "destination",
                CredentialTestBody {
                    ok: checked.ok,
                    message: checked.error,
                    code: checked.code,
                },
            )
        }
        _ => {
            return Err(invalid(
                "body",
                "must name exactly one of source_id and destination_id",
            ));
        }
    };

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    zirv_flow_store::record_credential_test(&mut *tx, workspace, id, body.ok).await?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "credential.tested",
        Some(("credential", id)),
        json!({"ok": body.ok, "code": body.code, "with": owner}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(body)
}

/// Tests a credential by running the check of a source or destination that uses it. A failed
/// check is a 200 with `ok` false, a fixed `message` and a `code`. Records `last_tested_at` and the
/// outcome. Workspace admins only. Audited as `credential.tested`.
#[utoipa::path(
    post,
    operation_id = "test_credential",
    path = "/api/v1/workspaces/{ws}/credentials/{id}/test",
    tag = "credentials",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Credential id")
    ),
    request_body = TestCredentialRequest,
    responses(
        (status = 200, description = "The outcome of the check", body = CredentialTestBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or credential, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The credential or the destination is archived, or the owner's type is not in this build", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Not exactly one of source_id and destination_id, or one that does not use this credential", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to open the credential", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "The source or destination did not answer in time", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn test(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<TestCredentialRequest>,
) -> Result<Json<CredentialTestBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let outcome = test_credential(
        &state,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        &body,
    )
    .await?;
    Ok(Json(outcome))
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::egress::AddressClass;

    use super::*;

    #[test]
    fn every_source_error_has_a_fixed_sentence_and_code_that_name_no_value() {
        let errors = [
            SourceError::Config {
                field: "host".into(),
                reason: "db.internal.example is wrong".into(),
            },
            SourceError::Auth,
            SourceError::Unreachable,
            SourceError::Egress(HttpError::EgressDenied {
                host: "db.internal.example".into(),
                class: AddressClass::Private,
            }),
            SourceError::Egress(HttpError::Transport("https://db.internal.example/x".into())),
            SourceError::Upstream(503),
            SourceError::Other("https://db.internal.example failed".into()),
        ];
        let mut codes = hashbrown::HashSet::new();

        for error in &errors {
            let (code, message) = describe_source(error);

            assert!(codes.insert(code), "{code} is used twice");
            for text in [code, message] {
                assert!(!text.contains("internal"), "{text}");
                assert!(!text.contains("http"), "{text}");
                assert!(!text.contains("503"), "{text}");
            }
        }
    }

    #[test]
    fn a_passed_check_has_neither_message_nor_code() {
        let passed = serde_json::to_value(outcome(None)).unwrap();
        let failed = serde_json::to_value(outcome(Some(("unreachable", "no")))).unwrap();

        assert_eq!(passed, json!({"ok": true, "message": null, "code": null}));
        assert_eq!(
            failed,
            json!({"ok": false, "message": "no", "code": "unreachable"})
        );
    }
}
