//! The admin operations on inbound webhooks (architecture sections 7.1 and 16.1): create, list,
//! read, change, rotate the secret and delete. A hook is a source of kind `webhook` with one
//! stream `payload` and a sealed shared secret, created and removed together in one
//! transaction. Members read; workspace admins change. The secret is returned once, by create
//! and by rotate, and never appears in a read, an audit entry or a log line.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_connectors::sources::{PAYLOAD_STREAM, StreamDescriptor, SyncMode};
use zirv_flow_core::signature::{check_header_name, generate_secret};
use zirv_flow_core::{SecretBytes, SecretId, SecretPurpose, SecretString, UserId, WorkspaceId};
use zirv_flow_store::{
    DiscoveredStream, NewSource, NewWebhook, SourceUpdate, StreamSettings, Webhook, WebhookUpdate,
};

use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::sources::crud::{check_name, name_taken};
use crate::sources::problems;

/// The signature header a hook starts with.
const DEFAULT_SIGNATURE_HEADER: &str = "X-Zirv-Signature";
/// The text before the digest a hook starts with.
const DEFAULT_SIGNATURE_PREFIX: &str = "sha256=";
/// The timestamp header a hook starts with.
const DEFAULT_TIMESTAMP_HEADER: &str = "X-Zirv-Timestamp";
/// The tolerance a hook starts with, in seconds.
const DEFAULT_TOLERANCE_SECONDS: i32 = 300;
const MAX_TOLERANCE_SECONDS: i32 = 86_400;
const MAX_PREFIX_CHARS: usize = 32;
const MAX_DEDUPE_FIELD_CHARS: usize = 256;

/// Tells an absent field from a `null` one: absent is `None`, `null` is `Some(None)`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A hook as the API shows it. Never holds the secret.
#[derive(Serialize, ToSchema)]
pub(crate) struct WebhookBody {
    id: Uuid,
    source_id: Uuid,
    stream_id: Uuid,
    /// The source's name.
    name: String,
    /// The path to send payloads to, relative to the server's origin.
    #[schema(example = "/hooks/3f2a9c1e-0b7d-4e55-9c1a-7d4e2b6a8f10")]
    path: String,
    signature_header: String,
    signature_prefix: String,
    /// The header holding the unix seconds the sender signed; null for a hook that signs the
    /// body alone.
    timestamp_header: Option<String>,
    tolerance_seconds: i32,
    /// The path in the payload whose value identifies it; a replay of the same value is
    /// recognised.
    dedupe_field: String,
    /// Whether the hook accepts payloads: the source and the stream are enabled and mapped.
    receiving: bool,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
}

impl From<&Webhook> for WebhookBody {
    fn from(hook: &Webhook) -> Self {
        Self {
            id: hook.id,
            source_id: hook.source_id,
            stream_id: hook.stream_id,
            name: hook.name.clone(),
            path: format!("/hooks/{}", hook.id),
            signature_header: hook.signature_header.clone(),
            signature_prefix: hook.signature_prefix.clone(),
            timestamp_header: hook.timestamp_header.clone(),
            tolerance_seconds: hook.tolerance_seconds,
            dedupe_field: hook.dedupe_field.clone(),
            receiving: hook.receiving,
            created_at: hook.created_at,
        }
    }
}

/// A hook with its shared secret, shown once.
#[derive(Serialize, ToSchema)]
pub(crate) struct CreatedWebhookBody {
    #[serde(flatten)]
    webhook: WebhookBody,
    /// The shared secret. Shown only in this response; keep it.
    secret: String,
}

/// A new shared secret, shown once.
#[derive(Serialize, ToSchema)]
pub(crate) struct RotatedSecretBody {
    /// The new shared secret. The old one stopped working when this was issued.
    secret: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateWebhookRequest {
    /// 1 to 128 characters after trimming; unique among the workspace's sources.
    #[schema(example = "Orders")]
    name: String,
    /// A dotted path into the payload whose value (a string or a number of 1 to 128
    /// characters) identifies it, such as `id` or `data.order.id`.
    #[schema(example = "id")]
    dedupe_field: String,
    /// Default `X-Zirv-Signature`.
    signature_header: Option<String>,
    /// The text before the 64 hex digits; default `sha256=`, may be empty.
    signature_prefix: Option<String>,
    /// Default `X-Zirv-Timestamp`; null selects the body-only scheme, whose only replay
    /// defense is the dedupe field.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>, nullable)]
    timestamp_header: Option<Option<String>>,
    /// 1 to 86400; default 300.
    tolerance_seconds: Option<i32>,
    /// Expect a payload at least this often, in seconds, or the hook is stale. Absent: never
    /// stale.
    staleness_threshold_seconds: Option<i32>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateWebhookRequest {
    name: Option<String>,
    dedupe_field: Option<String>,
    signature_header: Option<String>,
    signature_prefix: Option<String>,
    /// null selects the body-only scheme.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>, nullable)]
    timestamp_header: Option<Option<String>>,
    tolerance_seconds: Option<i32>,
    /// Stops or resumes receiving without losing the setup.
    enabled: Option<bool>,
}

/// The scheme of a hook as requested, before it is stored.
struct Scheme<'a> {
    signature_header: &'a str,
    signature_prefix: &'a str,
    timestamp_header: Option<&'a str>,
    tolerance_seconds: i32,
    dedupe_field: &'a str,
}

/// Checks a scheme, listing every broken rule: header names are HTTP tokens and differ, the
/// prefix is short and printable, the tolerance is in range and the dedupe field is a path.
fn check_scheme(scheme: &Scheme<'_>) -> Result<(), ApiError> {
    let mut problems_found: Vec<(String, String)> = Vec::new();
    let mut bad =
        |field: &str, reason: &str| problems_found.push((field.to_owned(), reason.to_owned()));
    if !check_header_name(scheme.signature_header) {
        bad(
            "signature_header",
            "must be an HTTP header name of at most 64 characters",
        );
    }
    if let Some(timestamp) = scheme.timestamp_header {
        if !check_header_name(timestamp) {
            bad(
                "timestamp_header",
                "must be an HTTP header name of at most 64 characters",
            );
        } else if timestamp.eq_ignore_ascii_case(scheme.signature_header) {
            bad("timestamp_header", "must differ from signature_header");
        }
    }
    if scheme.signature_prefix.chars().count() > MAX_PREFIX_CHARS
        || scheme.signature_prefix.chars().any(char::is_control)
    {
        bad(
            "signature_prefix",
            "must be at most 32 printable characters",
        );
    }
    if !(1..=MAX_TOLERANCE_SECONDS).contains(&scheme.tolerance_seconds) {
        bad("tolerance_seconds", "must be 1 to 86400");
    }
    let field = scheme.dedupe_field;
    if field.is_empty()
        || field.chars().count() > MAX_DEDUPE_FIELD_CHARS
        || field.chars().any(char::is_control)
        || field.split('.').any(str::is_empty)
    {
        bad(
            "dedupe_field",
            "must be a dotted path of 1 to 256 characters",
        );
    }
    if problems_found.is_empty() {
        Ok(())
    } else {
        Err(problems(problems_found))
    }
}

fn keyring(state: &AppState) -> Result<&std::sync::Arc<zirv_flow_core::Keyring>, ApiError> {
    state.keyring().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "webhook secrets are unavailable until the master key is configured",
        )
    })
}

fn plaintext(secret: &SecretString) -> SecretBytes {
    SecretBytes::new(secret.expose().as_bytes().to_vec())
}

fn descriptor() -> Result<serde_json::Value, ApiError> {
    serde_json::to_value(StreamDescriptor {
        name: PAYLOAD_STREAM.to_owned(),
        fields: Vec::new(),
        sync_modes: vec![SyncMode::FullRefresh],
        default_cursor_field: None,
        default_primary_key: Vec::new(),
        suggested_mapping: None,
    })
    .map_err(|_| ApiError::internal())
}

/// Creates a hook: the sealed secret, the source, the stream and the `webhooks` row in one
/// transaction, audited as `webhook.created` (and `secret.created`). Returns the hook and its
/// secret, which exists nowhere else in plain text.
pub(crate) async fn create_webhook(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    created_by: Option<UserId>,
    request: &CreateWebhookRequest,
) -> Result<(Webhook, SecretString), ApiError> {
    let name = check_name(&request.name)?;
    let timestamp_header = match &request.timestamp_header {
        None => Some(DEFAULT_TIMESTAMP_HEADER),
        Some(header) => header.as_deref(),
    };
    let scheme = Scheme {
        signature_header: request
            .signature_header
            .as_deref()
            .unwrap_or(DEFAULT_SIGNATURE_HEADER),
        signature_prefix: request
            .signature_prefix
            .as_deref()
            .unwrap_or(DEFAULT_SIGNATURE_PREFIX),
        timestamp_header,
        tolerance_seconds: request
            .tolerance_seconds
            .unwrap_or(DEFAULT_TOLERANCE_SECONDS),
        dedupe_field: &request.dedupe_field,
    };
    check_scheme(&scheme)?;
    if request
        .staleness_threshold_seconds
        .is_some_and(|seconds| seconds < 1)
    {
        return Err(crate::sources::invalid(
            "staleness_threshold_seconds",
            "must be a positive number of seconds",
        ));
    }
    let keyring = keyring(state)?;
    let secret = generate_secret();

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let secret_id = admin::create_secret_in(
        &mut tx,
        context,
        keyring,
        workspace,
        SecretPurpose::WebhookSecret,
        &plaintext(&secret),
    )
    .await?;
    let config = json!({});
    let source = zirv_flow_store::create_source(
        &mut *tx,
        workspace,
        &NewSource {
            id: Uuid::now_v7(),
            kind: "webhook",
            name,
            config: &config,
            credential_secret_id: None,
            credential_fields: &[],
            schedule: None,
            created_by,
        },
    )
    .await
    .map_err(name_taken)?;
    let descriptor = descriptor()?;
    let streams = zirv_flow_store::upsert_discovered_streams(
        &mut tx,
        workspace,
        source.id,
        &[DiscoveredStream {
            name: PAYLOAD_STREAM,
            sync_mode: SyncMode::FullRefresh.as_str(),
            cursor_field: None,
            primary_key: &[],
            descriptor: &descriptor,
            suggested_mapping: None,
        }],
    )
    .await?
    .ok_or_else(ApiError::not_found)?;
    let stream = streams.first().ok_or_else(ApiError::internal)?;
    zirv_flow_store::configure_stream(
        &mut tx,
        workspace,
        source.id,
        PAYLOAD_STREAM,
        &StreamSettings {
            enabled: false,
            sync_mode: SyncMode::FullRefresh.as_str(),
            cursor_field: None,
            primary_key: &[],
            staleness_threshold_seconds: request.staleness_threshold_seconds,
            reconcile_schedule: None,
            sweep_limit: crate::sources::streams::DEFAULT_SWEEP_LIMIT,
        },
    )
    .await?
    .ok_or_else(ApiError::not_found)?;
    let hook = zirv_flow_store::create_webhook(
        &mut tx,
        workspace,
        &NewWebhook {
            id: Uuid::new_v4(),
            source_id: source.id,
            stream_id: stream.id,
            secret_id,
            signature_header: scheme.signature_header,
            signature_prefix: scheme.signature_prefix,
            timestamp_header: scheme.timestamp_header,
            tolerance_seconds: scheme.tolerance_seconds,
            dedupe_field: scheme.dedupe_field,
            created_by,
        },
    )
    .await?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "webhook.created",
        Some(("webhook", hook.id)),
        json!({
            "name": hook.name,
            "source_id": hook.source_id,
            "signature_header": hook.signature_header,
            "timestamp_header": hook.timestamp_header,
            "tolerance_seconds": hook.tolerance_seconds,
            "dedupe_field": hook.dedupe_field,
        }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((hook, secret))
}

/// Changes a hook and audits `webhook.updated` with the names of the changed fields.
pub(crate) async fn update_webhook(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    request: &UpdateWebhookRequest,
) -> Result<Webhook, ApiError> {
    let existing = zirv_flow_store::find_webhook(&state.pool, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let mut fields: Vec<&'static str> = Vec::new();
    let name = request.name.as_deref().map(check_name).transpose()?;
    if name.is_some() {
        fields.push("name");
    }
    for (field, set) in [
        ("dedupe_field", request.dedupe_field.is_some()),
        ("signature_header", request.signature_header.is_some()),
        ("signature_prefix", request.signature_prefix.is_some()),
        ("timestamp_header", request.timestamp_header.is_some()),
        ("tolerance_seconds", request.tolerance_seconds.is_some()),
        ("enabled", request.enabled.is_some()),
    ] {
        if set {
            fields.push(field);
        }
    }
    if fields.is_empty() {
        return Err(crate::sources::invalid(
            "body",
            "must change at least one field",
        ));
    }
    let timestamp_header = match &request.timestamp_header {
        None => existing.timestamp_header.as_deref(),
        Some(header) => header.as_deref(),
    };
    check_scheme(&Scheme {
        signature_header: request
            .signature_header
            .as_deref()
            .unwrap_or(&existing.signature_header),
        signature_prefix: request
            .signature_prefix
            .as_deref()
            .unwrap_or(&existing.signature_prefix),
        timestamp_header,
        tolerance_seconds: request
            .tolerance_seconds
            .unwrap_or(existing.tolerance_seconds),
        dedupe_field: request
            .dedupe_field
            .as_deref()
            .unwrap_or(&existing.dedupe_field),
    })?;

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    if name.is_some() || request.enabled.is_some() {
        zirv_flow_store::update_source(
            &mut tx,
            workspace,
            existing.source_id,
            &SourceUpdate {
                name,
                enabled: request.enabled,
                ..SourceUpdate::default()
            },
        )
        .await
        .map_err(name_taken)?
        .ok_or_else(ApiError::not_found)?;
    }
    zirv_flow_store::update_webhook(
        &mut tx,
        workspace,
        id,
        &WebhookUpdate {
            signature_header: request.signature_header.as_deref(),
            signature_prefix: request.signature_prefix.as_deref(),
            timestamp_header: request.timestamp_header.as_ref().map(Option::as_deref),
            tolerance_seconds: request.tolerance_seconds,
            dedupe_field: request.dedupe_field.as_deref(),
        },
    )
    .await?
    .ok_or_else(ApiError::not_found)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "webhook.updated",
        Some(("webhook", id)),
        json!({"fields": fields}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    zirv_flow_store::find_webhook(&state.pool, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)
}

/// Seals a new secret, points the hook at it and deletes the old one: the old secret stops
/// working at once. Audited as `webhook.secret_rotated` (and `secret.created`/`secret.deleted`).
pub(crate) async fn rotate_secret(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<SecretString, ApiError> {
    let keyring = keyring(state)?;
    let secret = generate_secret();
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let fresh = admin::create_secret_in(
        &mut tx,
        context,
        keyring,
        workspace,
        SecretPurpose::WebhookSecret,
        &plaintext(&secret),
    )
    .await?;
    let old = zirv_flow_store::replace_webhook_secret(&mut tx, workspace, id, fresh)
        .await?
        .ok_or_else(ApiError::not_found)?;
    zirv_flow_store::secrets::delete(&mut *tx, workspace, old).await?;
    admin::audit_secret_deleted(&mut tx, context, workspace, old).await?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "webhook.secret_rotated",
        Some(("webhook", id)),
        json!({}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(secret)
}

/// Deletes a hook with its source, stream, queued payloads and secret, audited as
/// `webhook.deleted` (and `secret.deleted`).
pub(crate) async fn delete_webhook(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<(), ApiError> {
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let hook = zirv_flow_store::find_webhook(&mut *tx, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    zirv_flow_store::delete_source(&mut tx, workspace, hook.source_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let secret = SecretId::from_uuid(hook.secret_id);
    zirv_flow_store::secrets::delete(&mut *tx, workspace, secret).await?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "webhook.deleted",
        Some(("webhook", id)),
        json!({"name": hook.name, "source_id": hook.source_id}),
    )
    .await?;
    admin::audit_secret_deleted(&mut tx, context, workspace, secret).await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

/// A response that holds a secret must not be cached.
fn no_store<T: IntoResponse>(status: StatusCode, body: T) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], body).into_response()
}

/// The workspace's webhooks, ordered by name.
#[utoipa::path(
    get,
    operation_id = "list_webhooks",
    path = "/api/v1/workspaces/{ws}/webhooks",
    tag = "webhooks",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The webhooks", body = Vec<WebhookBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<Vec<WebhookBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let hooks = zirv_flow_store::list_webhooks(&state.pool, workspace.id).await?;
    Ok(Json(hooks.iter().map(WebhookBody::from).collect()))
}

/// Creates a webhook with its source, its stream `payload` (disabled until mapped) and a shared
/// secret, returned once. Workspace admins only. Audited as `webhook.created`.
#[utoipa::path(
    post,
    operation_id = "create_webhook",
    path = "/api/v1/workspaces/{ws}/webhooks",
    tag = "webhooks",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateWebhookRequest,
    responses(
        (status = 201, description = "The new webhook and its secret, shown only now", body = CreatedWebhookBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A source with that name exists, or the master key is outdated", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name, header name, prefix, tolerance, dedupe field or threshold", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to seal the secret", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateWebhookRequest>,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let (hook, secret) = create_webhook(
        &state,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        &body,
    )
    .await?;
    Ok(no_store(
        StatusCode::CREATED,
        Json(CreatedWebhookBody {
            webhook: WebhookBody::from(&hook),
            secret: secret.expose().to_owned(),
        }),
    ))
}

/// One webhook.
#[utoipa::path(
    get,
    operation_id = "get_webhook",
    path = "/api/v1/workspaces/{ws}/webhooks/{id}",
    tag = "webhooks",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Webhook id")
    ),
    responses(
        (status = 200, description = "The webhook", body = WebhookBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or webhook, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<WebhookBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let hook = zirv_flow_store::find_webhook(&state.pool, workspace.id, path_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(WebhookBody::from(&hook)))
}

/// Changes a webhook: its name, its signature scheme, its dedupe field, or whether it receives.
/// Workspace admins only. Audited as `webhook.updated`.
#[utoipa::path(
    patch,
    operation_id = "update_webhook",
    path = "/api/v1/workspaces/{ws}/webhooks/{id}",
    tag = "webhooks",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Webhook id")
    ),
    request_body = UpdateWebhookRequest,
    responses(
        (status = 200, description = "The updated webhook", body = WebhookBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or webhook, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Another source has that name", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid field, or no field at all", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateWebhookRequest>,
) -> Result<Json<WebhookBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let hook = update_webhook(
        &state,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        &body,
    )
    .await?;
    Ok(Json(WebhookBody::from(&hook)))
}

/// Issues a new shared secret, returned once. The old secret stops working at once. Workspace
/// admins only. Audited as `webhook.secret_rotated`.
#[utoipa::path(
    post,
    operation_id = "rotate_webhook_secret",
    path = "/api/v1/workspaces/{ws}/webhooks/{id}/rotate-secret",
    tag = "webhooks",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Webhook id")
    ),
    responses(
        (status = 200, description = "The new secret, shown only now", body = RotatedSecretBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or webhook, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to seal the secret", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn rotate(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let secret = rotate_secret(&state, &principal.context(), workspace.id, path_id(&id)?).await?;
    Ok(no_store(
        StatusCode::OK,
        Json(RotatedSecretBody {
            secret: secret.expose().to_owned(),
        }),
    ))
}

/// Deletes a webhook with its source, stream, queued payloads and secret. Workspace admins
/// only. Audited as `webhook.deleted`.
#[utoipa::path(
    delete,
    operation_id = "delete_webhook",
    path = "/api/v1/workspaces/{ws}/webhooks/{id}",
    tag = "webhooks",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Webhook id")
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or webhook, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    delete_webhook(&state, &principal.context(), workspace.id, path_id(&id)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scheme<'a>() -> Scheme<'a> {
        Scheme {
            signature_header: DEFAULT_SIGNATURE_HEADER,
            signature_prefix: DEFAULT_SIGNATURE_PREFIX,
            timestamp_header: Some(DEFAULT_TIMESTAMP_HEADER),
            tolerance_seconds: DEFAULT_TOLERANCE_SECONDS,
            dedupe_field: "id",
        }
    }

    fn fields(error: ApiError) -> String {
        error.detail().to_owned()
    }

    #[test]
    fn the_defaults_are_a_valid_scheme_and_a_body_only_scheme_needs_no_timestamp() {
        assert!(check_scheme(&scheme()).is_ok());
        assert!(
            check_scheme(&Scheme {
                timestamp_header: None,
                signature_prefix: "",
                ..scheme()
            })
            .is_ok()
        );
    }

    #[test]
    fn every_broken_rule_is_reported_together() {
        let error = check_scheme(&Scheme {
            signature_header: "bad header",
            signature_prefix: "p\n",
            timestamp_header: Some("bad:header"),
            tolerance_seconds: 0,
            dedupe_field: "a..b",
        })
        .unwrap_err();

        let detail = fields(error);
        for field in [
            "signature_header",
            "timestamp_header",
            "signature_prefix",
            "tolerance_seconds",
            "dedupe_field",
        ] {
            assert!(detail.contains(field), "{detail}");
        }
    }

    #[test]
    fn the_two_header_names_must_differ_whatever_their_case() {
        let error = check_scheme(&Scheme {
            timestamp_header: Some("x-zirv-signature"),
            ..scheme()
        })
        .unwrap_err();

        assert!(fields(error).contains("timestamp_header must differ"));
    }

    #[test]
    fn the_tolerance_and_dedupe_field_have_edges() {
        for tolerance in [1, 86_400] {
            assert!(
                check_scheme(&Scheme {
                    tolerance_seconds: tolerance,
                    ..scheme()
                })
                .is_ok()
            );
        }
        assert!(
            check_scheme(&Scheme {
                tolerance_seconds: 86_401,
                ..scheme()
            })
            .is_err()
        );
        let long = "a".repeat(257);
        for field in ["", ".a", "a.", long.as_str()] {
            assert!(
                check_scheme(&Scheme {
                    dedupe_field: field,
                    ..scheme()
                })
                .is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn a_null_timestamp_header_is_told_from_an_absent_one() {
        let absent: UpdateWebhookRequest = serde_json::from_value(json!({})).unwrap();
        let null: UpdateWebhookRequest =
            serde_json::from_value(json!({"timestamp_header": null})).unwrap();
        let named: UpdateWebhookRequest =
            serde_json::from_value(json!({"timestamp_header": "X-Ts"})).unwrap();

        assert_eq!(absent.timestamp_header, None);
        assert_eq!(null.timestamp_header, Some(None));
        assert_eq!(named.timestamp_header, Some(Some("X-Ts".to_owned())));
    }

    #[test]
    fn a_body_names_the_path_and_never_holds_a_secret() {
        let hook = Webhook {
            id: Uuid::nil(),
            source_id: Uuid::nil(),
            stream_id: Uuid::nil(),
            secret_id: Uuid::nil(),
            name: "Orders".into(),
            signature_header: "X-Sig".into(),
            signature_prefix: String::new(),
            timestamp_header: None,
            tolerance_seconds: 300,
            dedupe_field: "id".into(),
            receiving: false,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };

        let body = serde_json::to_value(WebhookBody::from(&hook)).unwrap();

        assert_eq!(body["path"], format!("/hooks/{}", Uuid::nil()));
        assert!(body.get("secret").is_none());
        assert!(body.get("secret_id").is_none());
        assert_eq!(body["timestamp_header"], json!(null));
    }
}
