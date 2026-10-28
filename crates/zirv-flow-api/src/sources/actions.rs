//! Calls into a source: testing its connection and discovering its streams. Both open the
//! stored credential right before the call and pass it to the connector as a typed credential.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_connectors::http::HttpLimits;
use zirv_flow_connectors::runtime::RateLimit;
use zirv_flow_connectors::sources::{
    Credential, ErasedSource, Record, SourceContext, SourceError, StreamConfig, StreamDescriptor,
    SyncMode,
};
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{DiscoveredStream, Source, Stream};

use super::streams::{StreamBody, bodies, checked_mapping};
use super::{kind_unavailable, registry, tokens};
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::imports::MappingBody;
use crate::problem::{ApiError, Problem};
use crate::workspace_credentials::resolve;

/// The longest a test or a discovery may take.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Serialize, ToSchema)]
pub(crate) struct TestBody {
    /// Whether the connection works.
    ok: bool,
    /// Why it does not. Never holds a credential or a URL.
    error: Option<String>,
}

/// A source ready to call: its row, its type, its opened credential and its network context.
pub(crate) struct Prepared {
    pub(crate) source: Source,
    pub(crate) driver: &'static dyn ErasedSource,
    pub(crate) credential: Credential,
    pub(crate) context: SourceContext,
}

/// The network context of a call for source `id` of type `kind`. Its requests are counted by the
/// source type and limited by the process-wide bucket of `id` at `rate_limit` (the source
/// driver's [`rate_limit`](zirv_flow_connectors::sources::ErasedSource::rate_limit) for the
/// source's settings), so a test, a discovery and a sync run of the same source draw from one
/// bucket. Tokens are seeded and persisted by [`super::tokens::prepare`], not here.
pub(super) fn network_context(
    state: &AppState,
    kind: &'static str,
    rate_limit: Option<RateLimit>,
    id: Uuid,
) -> Result<SourceContext, ApiError> {
    SourceContext::new(
        state.egress_policy().clone(),
        HttpLimits::default(),
        kind,
        id.to_string(),
        Arc::clone(state.rate_limits()),
    )
    .map(|context| context.with_rate_limit(rate_limit))
    .map_err(|_| ApiError::internal())
}

pub(crate) async fn prepare(
    state: &AppState,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Prepared, ApiError> {
    let source = zirv_flow_store::find_source(&state.pool, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let driver = registry().get(&source.kind).ok_or_else(kind_unavailable)?;
    let mut credential = resolve::load(state, workspace, source.credential_id).await?;
    let context = network_context(
        state,
        driver.spec().kind,
        driver.rate_limit(&source.config),
        id,
    )?;
    // An account connected by consent: its tokens seed the context, and refreshes are stored.
    let context = tokens::prepare(state, workspace, &source, &mut credential, context).await?;
    Ok(Prepared {
        source,
        driver,
        credential,
        context,
    })
}

pub(crate) fn timed_out() -> ApiError {
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
    let denied = result.as_ref().err().is_some_and(egress_denied);
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
    if denied {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the source's address is not allowed by the egress policy",
        )
        .with_code("egress_denied"));
    }
    Ok(body)
}

/// The sync mode a new stream starts with: full refresh when offered, else incremental when
/// the stream names a cursor field (the database refuses incremental without one). The stream
/// starts disabled, and the user confirms the mode before enabling it.
fn initial(descriptor: &StreamDescriptor) -> SyncMode {
    if descriptor.sync_modes == [SyncMode::Stream] {
        return SyncMode::Stream;
    }
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
    let mut seen = hashbrown::HashSet::new();
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
        // PostgreSQL stores no NUL in `text` or `jsonb`.
        let mut texts = std::iter::once(stream.name.as_str())
            .chain(stream.fields.iter().map(|field| field.name.as_str()))
            .chain(stream.default_cursor_field.as_deref())
            .chain(stream.default_primary_key.iter().map(String::as_str));
        if texts.any(|text| text.contains('\0')) {
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "the source reported a stream with a NUL character",
            ));
        }
    }
    Ok(())
}

/// The mapping a source suggests for a stream, when it passes the checks a saved mapping does
/// (the stream's fields, the trait registry, the lists of the workspace). A suggestion that
/// fails is dropped and the stream is left unmapped: it is a head start, never an error.
async fn checked_seed(
    state: &AppState,
    workspace: WorkspaceId,
    stream: &StreamDescriptor,
) -> Option<serde_json::Value> {
    let suggested = stream.suggested_mapping.as_ref()?;
    let mapping =
        serde_json::from_value::<zirv_flow_core::mapping::Mapping>(suggested.clone()).ok()?;
    let body = MappingBody::from_core(&mapping)?;
    let checked = checked_mapping(
        state,
        workspace,
        stream,
        &stream.default_primary_key,
        &body,
        false,
    )
    .await
    .ok()?;
    serde_json::to_value(checked).ok()
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
    let mut seeds: Vec<Option<serde_json::Value>> = Vec::with_capacity(found.len());
    for stream in &found {
        seeds.push(checked_seed(state, workspace, stream).await);
    }
    let discovered: Vec<DiscoveredStream<'_>> = found
        .iter()
        .zip(&descriptors)
        .zip(&seeds)
        .map(|((stream, descriptor), seed)| DiscoveredStream {
            name: &stream.name,
            sync_mode: initial(stream).as_str(),
            cursor_field: stream.default_cursor_field.as_deref(),
            primary_key: &stream.default_primary_key,
            descriptor,
            suggested_mapping: seed.as_ref(),
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
        json!({"streams": found.len()}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    // Discovery is for admins, who may see the high-water mark.
    bodies(state, workspace, id, &streams, true).await
}

/// Reads the first records of `stream` from its start, with the source's stored credential and
/// tokens, and writes nothing. At most `limit` are returned; the flag says the stream has more.
/// `config` is the saved settings, or a draft the caller has validated. The whole read is under
/// [`CALL_TIMEOUT`].
pub(super) async fn read_sample(
    call: &Prepared,
    config: &serde_json::Value,
    stream: &StreamConfig,
    limit: usize,
) -> Result<(Vec<Record>, bool), ApiError> {
    let read = async {
        let mut batches = call
            .driver
            .read(&call.context, config, &call.credential, stream, None);
        let mut records: Vec<Record> = Vec::new();
        // One more than shown, to know whether the stream has more.
        while records.len() <= limit {
            match batches.next().await {
                Some(Ok(batch)) => records.extend(batch.records),
                Some(Err(error)) => return Err(error),
                None => break,
            }
        }
        Ok(records)
    };
    let mut records = tokio::time::timeout(CALL_TIMEOUT, read)
        .await
        .map_err(|_| timed_out())?
        .map_err(upstream)?;
    let truncated = records.len() > limit;
    records.truncate(limit);
    Ok((records, truncated))
}

/// Reads the first `limit` records of a stored stream from its start, with the source's stored
/// credential and tokens, and writes nothing. The flag says the stream has more. The credential is
/// opened right before the read and dropped with it.
pub(super) async fn read_stream(
    state: &AppState,
    workspace: WorkspaceId,
    source_id: Uuid,
    stream: &Stream,
    limit: usize,
) -> Result<(Vec<Record>, bool), ApiError> {
    let call = prepare(state, workspace, source_id).await?;
    let config = StreamConfig {
        name: stream.name.clone(),
        sync_mode: SyncMode::parse(&stream.sync_mode).unwrap_or(SyncMode::FullRefresh),
        cursor_field: None,
        primary_key: stream.primary_key.clone(),
    };
    read_sample(&call, &call.source.config, &config, limit).await
}

/// As [`read_stream`], without the flag.
pub(crate) async fn read_stream_records(
    state: &AppState,
    workspace: WorkspaceId,
    source_id: Uuid,
    stream: &Stream,
    limit: usize,
) -> Result<Vec<Record>, ApiError> {
    Ok(read_stream(state, workspace, source_id, stream, limit)
        .await?
        .0)
}

fn egress_denied(error: &SourceError) -> bool {
    matches!(
        error,
        SourceError::Egress(zirv_flow_connectors::http::HttpError::EgressDenied { .. })
    )
}

/// A failed call to the source, shown as the source's own failure.
pub(super) fn upstream(error: SourceError) -> ApiError {
    if egress_denied(&error) {
        return ApiError::new(
            StatusCode::BAD_GATEWAY,
            "the source's address is not allowed by the egress policy",
        )
        .with_code("egress_denied");
    }
    ApiError::new(StatusCode::BAD_GATEWAY, error.to_string())
}

/// Tests the connection to the source with its stored credential. A failed test is a 200 with
/// `ok` false and the reason, except an egress denial (422). Workspace admins only. Audited as `source.tested`.
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
        (status = 422, description = "The egress policy refused the source (`egress_denied`)", body = Problem, content_type = "application/problem+json"),
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
    use zirv_flow_connectors::sources::{FieldDescriptor, FieldType, SourceSpec};

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
            suggested_mapping: None,
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
        assert_eq!(
            initial(&descriptor(vec![SyncMode::Stream])),
            SyncMode::Stream,
            "a stream that only streams starts in that mode"
        );
    }
    #[test]
    fn discovered_streams_need_1_to_256_character_unique_names_and_no_nul() {
        let named = |name: &str| StreamDescriptor {
            name: name.into(),
            ..descriptor(vec![SyncMode::FullRefresh])
        };
        let nul_field = StreamDescriptor {
            fields: vec![FieldDescriptor {
                name: "i\0d".into(),
                field_type: FieldType::Number,
            }],
            ..descriptor(vec![SyncMode::FullRefresh])
        };
        let nul_key = StreamDescriptor {
            default_primary_key: vec!["i\0d".into()],
            suggested_mapping: None,
            ..descriptor(vec![SyncMode::FullRefresh])
        };

        assert!(check_discovered(&[named("a"), named("b"), named(&"x".repeat(256))]).is_ok());
        for bad in [
            vec![named("")],
            vec![named(&"x".repeat(257))],
            vec![named("a"), named("a")],
            vec![named("a\0b")],
            vec![nul_field],
            vec![nul_key],
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

    static LIMITED: SourceSpec = SourceSpec {
        kind: "limited",
        label: "Limited",
        description: "",
        config_fields: &[],
        credential_fields: &[],
        capabilities: zirv_flow_connectors::sources::Capabilities {
            push: false,
            incremental: false,
            full_refresh: true,
            emits_deletes: false,
        },
        rate_limit: Some(zirv_flow_connectors::runtime::RateLimit {
            requests: 2,
            per: Duration::from_secs(1),
        }),
        config_check: None,
    };

    fn state() -> AppState {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap();
        AppState::new(pool, Arc::new(String::new))
    }

    /// A request the default egress policy refuses, so it never leaves the process; it still
    /// waits for the source's rate limit first.
    async fn refused_call(context: &SourceContext) {
        use zirv_flow_connectors::runtime::{Auth, RequestSpec};
        let base: url::Url = "http://127.0.0.1:9/".parse().unwrap();
        let result = context
            .send(&Auth::None, RequestSpec::get(&base, base.clone()))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_and_discovery_contexts_of_one_source_share_its_rate_limit_bucket() {
        let state = state();
        let id = Uuid::new_v4();
        let test = network_context(&state, LIMITED.kind, LIMITED.rate_limit, id).unwrap();
        let discovery = network_context(&state, LIMITED.kind, LIMITED.rate_limit, id).unwrap();
        let started = std::time::Instant::now();

        for round in 0..5 {
            refused_call(if round % 2 == 0 { &test } else { &discovery }).await;
        }

        // Two requests per second: the first two go at once, the other three take 1.5 s.
        assert!(
            started.elapsed() >= Duration::from_millis(1450),
            "{:?}",
            started.elapsed()
        );
    }

    /// The rendered wait metric for the `limited` source type.
    fn wait_metric(handle: &metrics_exporter_prometheus::PrometheusHandle) -> Vec<String> {
        handle
            .render()
            .lines()
            .filter(|line| line.starts_with("zirv_flow_source_rate_limit_wait_seconds_total"))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn two_sources_get_separate_buckets_so_a_fresh_one_records_no_wait() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // The recorder is local to this thread, and the current-thread runtime stays on it.
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                let state = state();
                let busy =
                    network_context(&state, LIMITED.kind, LIMITED.rate_limit, Uuid::new_v4())
                        .unwrap();
                let fresh =
                    network_context(&state, LIMITED.kind, LIMITED.rate_limit, Uuid::new_v4())
                        .unwrap();
                // Two requests per second: the third and fourth calls wait.
                for _ in 0..4 {
                    refused_call(&busy).await;
                }
                let after_busy = wait_metric(&handle);
                assert_eq!(after_busy.len(), 1, "{after_busy:?}");

                // The fresh source is inside its own burst, so it waits for nothing.
                refused_call(&fresh).await;
                refused_call(&fresh).await;

                assert_eq!(wait_metric(&handle), after_busy);
            });
        });
    }
}
