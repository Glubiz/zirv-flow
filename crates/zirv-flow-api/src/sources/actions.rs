//! Calls into a source: testing its connection and discovering its streams. Both open the
//! stored credential right before the call and pass it to the connector as a typed credential.

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Serialize;
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_connectors::http::HttpLimits;
use zirv_flow_connectors::sources::{
    Credential, ErasedSource, SourceContext, SourceError, StreamDescriptor, SyncMode,
};
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{DiscoveredStream, Source};

use super::streams::{StreamBody, bodies};
use super::{credential, kind_unavailable, registry};
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};

/// The longest a test or a discovery may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Serialize, ToSchema)]
pub(crate) struct TestBody {
    /// Whether the connection works.
    ok: bool,
    /// Why it does not. Never holds a credential or a URL.
    error: Option<String>,
}

/// A source ready to call: its row, its type, its opened credential and its network context.
struct Prepared {
    source: Source,
    driver: &'static dyn ErasedSource,
    credential: Credential,
    context: SourceContext,
}

async fn prepare(state: &AppState, workspace: WorkspaceId, id: Uuid) -> Result<Prepared, ApiError> {
    let source = zirv_flow_store::find_source(&state.pool, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let driver = registry().get(&source.kind).ok_or_else(kind_unavailable)?;
    let credential = credential::load(state, workspace, source.credential_secret_id).await?;
    let context = SourceContext::new(state.egress_policy().clone(), HttpLimits::default())
        .map_err(|_| ApiError::internal())?;
    Ok(Prepared {
        source,
        driver,
        credential,
        context,
    })
}

fn timed_out() -> ApiError {
    ApiError::new(
        StatusCode::GATEWAY_TIMEOUT,
        "the source did not answer in time",
    )
}

/// Runs the source's connection test and audits `source.tested` with the outcome.
pub(crate) async fn test_source(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<TestBody, ApiError> {
    let call = prepare(state, workspace, id).await?;
    let result = tokio::time::timeout(
        CALL_TIMEOUT,
        call.driver
            .check(&call.context, &call.source.config, &call.credential),
    )
    .await
    .map_err(|_| timed_out())?;
    let body = TestBody {
        ok: result.is_ok(),
        error: result.err().map(|error| error.to_string()),
    };
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "source.tested",
        Some(("source", id)),
        json!({"ok": body.ok}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(body)
}

/// The sync mode a new stream starts with: full refresh when offered, else incremental when
/// the stream names a cursor field (the database refuses incremental without one). The stream
/// starts disabled, and the user confirms the mode before enabling it.
fn initial(descriptor: &StreamDescriptor) -> SyncMode {
    let incremental_only = !descriptor.sync_modes.contains(&SyncMode::FullRefresh)
        && descriptor.sync_modes.contains(&SyncMode::Incremental);
    if incremental_only && descriptor.default_cursor_field.is_some() {
        SyncMode::Incremental
    } else {
        SyncMode::FullRefresh
    }
}

const MAX_STREAM_NAME_CHARS: usize = 256;

/// Checks what discovery reported before anything is stored: a name of 1 to 256 characters and
/// no name twice. A source that reports otherwise has failed, whatever its streams are.
fn check_discovered(found: &[StreamDescriptor]) -> Result<(), ApiError> {
    let mut seen = std::collections::HashSet::new();
    for stream in found {
        let length = stream.name.chars().count();
        if length == 0 || length > MAX_STREAM_NAME_CHARS {
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "the source reported a stream name that is not 1 to 256 characters",
            ));
        }
        if !seen.insert(stream.name.as_str()) {
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "the source reported the same stream name twice",
            ));
        }
    }
    Ok(())
}

/// Asks the source for its streams, stores them (new ones disabled, known ones refreshed, the
/// user's settings untouched) and audits `source.discovered`.
pub(crate) async fn discover_source(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Vec<StreamBody>, ApiError> {
    let call = prepare(state, workspace, id).await?;
    let found = tokio::time::timeout(
        CALL_TIMEOUT,
        call.driver
            .discover(&call.context, &call.source.config, &call.credential),
    )
    .await
    .map_err(|_| timed_out())?
    .map_err(upstream)?;
    check_discovered(&found)?;

    let descriptors: Vec<serde_json::Value> = found
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()
        .map_err(|_| ApiError::internal())?;
    let discovered: Vec<DiscoveredStream<'_>> = found
        .iter()
        .zip(&descriptors)
        .map(|(stream, descriptor)| DiscoveredStream {
            name: &stream.name,
            sync_mode: initial(stream).as_str(),
            cursor_field: stream.default_cursor_field.as_deref(),
            primary_key: &stream.default_primary_key,
            descriptor,
        })
        .collect();
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let streams = zirv_flow_store::upsert_discovered_streams(&mut tx, workspace, id, &discovered)
        .await?
        .ok_or_else(ApiError::not_found)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "source.discovered",
        Some(("source", id)),
        json!({"name": call.source.name, "streams": found.len()}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    bodies(&streams)
}

/// A failed call to the source, shown as the source's own failure.
fn upstream(error: SourceError) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, error.to_string())
}

/// Tests the connection to the source with its stored credential. A failed test is a 200 with
/// `ok` false and the reason. Workspace admins only. Audited as `source.tested`.
#[utoipa::path(
    post,
    operation_id = "test_source",
    path = "/api/v1/workspaces/{ws}/sources/{id}/test",
    tag = "sources",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Source id")
    ),
    responses(
        (status = 200, description = "The outcome of the test", body = TestBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or source, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The source type is not in this build", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to open the credential", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "The source did not answer in time", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn test(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<TestBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let body = test_source(&state, &principal.context(), workspace.id, path_id(&id)?).await?;
    Ok(Json(body))
}

/// Asks the source for its streams and stores them. New streams start disabled; known ones keep
/// the user's settings. Workspace admins only. Audited as `source.discovered`.
#[utoipa::path(
    post,
    operation_id = "discover_source_streams",
    path = "/api/v1/workspaces/{ws}/sources/{id}/discover",
    tag = "sources",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Source id")
    ),
    responses(
        (status = 200, description = "Every stream of the source", body = Vec<StreamBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or source, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The source type is not in this build", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The source refused or failed the request", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to open the credential", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "The source did not answer in time", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn discover(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<Vec<StreamBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let streams =
        discover_source(&state, &principal.context(), workspace.id, path_id(&id)?).await?;
    Ok(Json(streams))
}

#[cfg(test)]
mod tests {
    use zirv_flow_connectors::sources::{FieldDescriptor, FieldType};

    use super::*;

    fn descriptor(modes: Vec<SyncMode>) -> StreamDescriptor {
        StreamDescriptor {
            name: "s".into(),
            fields: vec![FieldDescriptor {
                name: "id".into(),
                field_type: FieldType::Number,
            }],
            sync_modes: modes,
            default_cursor_field: None,
            default_primary_key: vec![],
        }
    }

    #[test]
    fn a_new_stream_starts_in_full_refresh_unless_only_incremental_with_a_cursor() {
        let both = descriptor(vec![SyncMode::Incremental, SyncMode::FullRefresh]);
        let mut with_cursor = descriptor(vec![SyncMode::Incremental]);
        let without_cursor = descriptor(vec![SyncMode::Incremental]);
        with_cursor.default_cursor_field = Some("id".into());

        assert_eq!(initial(&both), SyncMode::FullRefresh);
        assert_eq!(initial(&with_cursor), SyncMode::Incremental);
        assert_eq!(initial(&without_cursor), SyncMode::FullRefresh);
    }
    #[test]
    fn discovered_names_must_be_1_to_256_characters_and_unique() {
        let named = |name: &str| StreamDescriptor {
            name: name.into(),
            ..descriptor(vec![SyncMode::FullRefresh])
        };

        assert!(check_discovered(&[named("a"), named("b"), named(&"x".repeat(256))]).is_ok());
        for bad in [
            vec![named("")],
            vec![named(&"x".repeat(257))],
            vec![named("a"), named("a")],
        ] {
            let error = check_discovered(&bad).unwrap_err();
            assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
            assert!(!error.detail().contains("xxx"));
        }
    }

    #[test]
    fn a_failed_call_is_a_bad_gateway_with_the_sources_own_text() {
        let error = upstream(SourceError::Auth);

        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error.detail(), "the source refused the credential");
    }

    #[test]
    fn a_timeout_is_a_gateway_timeout() {
        assert_eq!(timed_out().status(), StatusCode::GATEWAY_TIMEOUT);
    }
}
