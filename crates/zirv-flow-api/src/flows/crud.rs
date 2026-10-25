//! The flows of a workspace: create, edit the draft, validate, read, list and read the versions.
//! Each write is audited with ids and the name, never the graph's values.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use utoipa::openapi::schema::{ArrayBuilder, ObjectBuilder, SchemaType, Type};
use utoipa::openapi::{Ref, RefOr, Schema};
use uuid::Uuid;
use zirv_flow_connectors::destinations::{Registry, provider_profile_support};
use zirv_flow_core::ValidationError;
use zirv_flow_core::consent::Channel;
use zirv_flow_core::flow::{FlowGraph, FlowSettings, NodeKind};
use zirv_flow_store::{Flow, FlowPatch, FlowState, FlowVersion, NewFlow};

use super::schema::{GRAPH, ISSUE, SETTINGS};
use super::validation::{Environment, SendFacts, SyncFacts, check};
use super::{
    default_settings, draft_of, empty_graph, flow_error, issue_problems, name_taken, parse_graph,
    parse_settings,
};
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::consent::bool_param;
use crate::json::ApiJson;
use crate::problem::{ApiError, FlowIssueProblem, Problem};
use crate::segments::catalog_of;

const MAX_NAME_CHARS: usize = 100;

fn graph_schema() -> RefOr<Schema> {
    RefOr::Ref(Ref::from_schema_name(GRAPH))
}

fn settings_schema() -> RefOr<Schema> {
    RefOr::Ref(Ref::from_schema_name(SETTINGS))
}

pub(super) fn issues_schema() -> Schema {
    ArrayBuilder::new()
        .items(RefOr::Ref(Ref::from_schema_name(ISSUE)))
        .into()
}

fn state_schema() -> Schema {
    ObjectBuilder::new()
        .schema_type(SchemaType::Type(Type::String))
        .enum_values(Some(FlowState::ALL.map(FlowState::as_str)))
        .description(Some(
            "`draft` before the first publish, `published` while live, `paused` (no new \
             enrollments) and `archived` (final).",
        ))
        .into()
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct FlowBody {
    id: Uuid,
    #[schema(example = "Welcome series")]
    name: String,
    #[schema(schema_with = state_schema)]
    state: &'static str,
    /// The newest published version; null before the first publish.
    current_version: Option<i32>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    archived_at: Option<OffsetDateTime>,
    /// The user who created it; null when an API key did.
    created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl From<&Flow> for FlowBody {
    fn from(flow: &Flow) -> Self {
        Self {
            id: flow.id,
            name: flow.name.clone(),
            state: flow.state.as_str(),
            current_version: flow.current_version,
            archived_at: flow.archived_at,
            created_by: flow.created_by,
            created_at: flow.created_at,
            updated_at: flow.updated_at,
        }
    }
}

/// A flow with its draft. The draft is what the editor changes; published content is in the
/// versions.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct FlowDetailBody {
    #[serde(flatten)]
    flow: FlowBody,
    /// The draft graph. It may be incomplete: only a publish validates it.
    #[schema(schema_with = graph_schema)]
    graph: Value,
    #[schema(schema_with = settings_schema)]
    settings: Value,
}

impl From<&Flow> for FlowDetailBody {
    fn from(flow: &Flow) -> Self {
        Self {
            flow: FlowBody::from(flow),
            graph: flow.draft_graph.clone(),
            settings: flow.draft_settings.clone(),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct FlowVersionBody {
    version: i32,
    #[schema(schema_with = graph_schema)]
    graph: Value,
    #[schema(schema_with = settings_schema)]
    settings: Value,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    published_at: OffsetDateTime,
    /// The user who published it; null when an API key did.
    published_by: Option<Uuid>,
    /// The trace of the publish request.
    trace_id: Uuid,
}

impl From<&FlowVersion> for FlowVersionBody {
    fn from(version: &FlowVersion) -> Self {
        Self {
            version: version.version,
            graph: version.graph.clone(),
            settings: version.settings.clone(),
            published_at: version.published_at,
            published_by: version.published_by,
            trace_id: version.trace_id,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateFlowRequest {
    /// 1 to 100 characters after trimming; unique among the workspace's flows that are not
    /// archived, ignoring case.
    #[schema(example = "Welcome series")]
    name: String,
    /// The draft graph; an empty graph when omitted.
    #[schema(schema_with = graph_schema)]
    graph: Option<Value>,
    /// The draft settings; a manual trigger entered once when omitted.
    #[schema(schema_with = settings_schema)]
    settings: Option<Value>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateFlowRequest {
    name: Option<String>,
    /// Replaces the draft graph.
    #[schema(schema_with = graph_schema)]
    graph: Option<Value>,
    /// Replaces the draft settings.
    #[schema(schema_with = settings_schema)]
    settings: Option<Value>,
}

/// What validating a flow found.
#[derive(Serialize, ToSchema)]
pub(crate) struct ValidationBody {
    /// True exactly when there are no issues: the flow can be published.
    valid: bool,
    #[schema(schema_with = issues_schema)]
    issues: Vec<FlowIssueProblem>,
}

fn invalid(field: &'static str, message: &'static str) -> ApiError {
    ValidationError::new(field, message).into()
}

fn check_name(name: &str) -> Result<&str, ApiError> {
    let name = name.trim();
    let length = name.chars().count();
    if length == 0 || length > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        Err(invalid(
            "name",
            "must be 1 to 100 characters without control characters",
        ))
    } else {
        Ok(name)
    }
}

/// Reads the workspace facts the checks need on `connection` and checks the draft of `flow`.
pub(super) async fn check_flow(
    state: &AppState,
    connection: &mut zirv_flow_store::PgConnection,
    workspace: zirv_flow_core::WorkspaceId,
    flow: &Flow,
) -> Result<(super::validation::Checked, OffsetDateTime), ApiError> {
    let (graph, settings) = draft_of(flow)?;
    check_document(state, connection, workspace, &graph, &settings).await
}

/// Reads the workspace facts the checks need on `connection` and checks a graph and settings as a
/// publish would. It writes nothing.
pub(crate) async fn check_document(
    state: &AppState,
    connection: &mut zirv_flow_store::PgConnection,
    workspace: zirv_flow_core::WorkspaceId,
    graph: &FlowGraph,
    settings: &FlowSettings,
) -> Result<(super::validation::Checked, OffsetDateTime), ApiError> {
    let (catalog, storage) =
        catalog_of(zirv_flow_store::segment_catalog(connection, workspace).await?);
    let now = zirv_flow_store::database_now(&mut *connection).await?;
    let signing_secret = zirv_flow_store::has_webhook_signing_secret(connection, workspace).await?;
    let webhook_templates = usable_webhook_templates(connection, workspace, graph).await?;
    let send = send_facts(connection, workspace, graph).await?;
    let sync = sync_facts(connection, workspace, graph).await?;
    let environment = Environment {
        catalog: &catalog,
        storage: &storage,
        now,
        egress: state.egress_policy(),
        signing_secret,
        webhook_templates: &webhook_templates,
        send: &send,
        sync: &sync,
    };
    Ok((check(graph, settings, &environment), now))
}

/// The profile operations of the destinations the sync nodes of `graph` name: those of the
/// workspace that are not archived, from the manifest of each.
async fn sync_facts(
    connection: &mut zirv_flow_store::PgConnection,
    workspace: zirv_flow_core::WorkspaceId,
    graph: &FlowGraph,
) -> Result<SyncFacts, ApiError> {
    let registry = Registry::built_in();
    let mut facts = SyncFacts::default();
    for node in &graph.nodes {
        let NodeKind::ActionSync { destination, .. } = &node.kind else {
            continue;
        };
        if facts.destinations.contains_key(destination) {
            continue;
        }
        let found =
            zirv_flow_store::find_destination(&mut *connection, workspace, *destination).await?;
        if let Some(found) = found.filter(|found| {
            found.archived_at.is_none()
                // A provider stays in the facts even when its manifest has no profile operation:
                // the operation check then answers `operation_unsupported`.
                && registry
                    .get(&found.kind)
                    .is_some_and(|driver| driver.spec().capabilities.channels.contains(&"profile_sync"))
        }) {
            facts
                .destinations
                .insert(*destination, provider_profile_support(&found.config));
        }
    }
    Ok(facts)
}

/// The templates `graph` names that a webhook node may send: of the workspace, not archived, of the
/// `webhook` channel and with a published version.
async fn usable_webhook_templates(
    connection: &mut zirv_flow_store::PgConnection,
    workspace: zirv_flow_core::WorkspaceId,
    graph: &FlowGraph,
) -> Result<BTreeSet<Uuid>, ApiError> {
    let mut usable = BTreeSet::new();
    let named: BTreeSet<Uuid> = graph
        .nodes
        .iter()
        .filter_map(|node| match &node.kind {
            NodeKind::ActionWebhook { template, .. } => *template,
            _ => None,
        })
        .collect();
    for id in named {
        let found = zirv_flow_store::find_template(&mut *connection, workspace, id).await?;
        if found.is_some_and(|template| {
            template.channel == "webhook"
                && template.archived_at.is_none()
                && template.current_version.is_some()
        }) {
            usable.insert(id);
        }
    }
    Ok(usable)
}

/// What the workspace holds of the templates, destinations, topics and dimension values the send
/// nodes of `graph` name: a template that is not archived, of the `email` channel and published; a
/// destination that is not archived and of a type that delivers email; a topic of the email
/// channel; and, per node, whether its dimension values are all the workspace's.
async fn send_facts(
    connection: &mut zirv_flow_store::PgConnection,
    workspace: zirv_flow_core::WorkspaceId,
    graph: &FlowGraph,
) -> Result<SendFacts, ApiError> {
    let registry = Registry::built_in();
    let mut facts = SendFacts::default();
    for node in &graph.nodes {
        let NodeKind::ActionSend {
            template,
            destination,
            topic,
            dimensions,
        } = &node.kind
        else {
            continue;
        };
        let found = zirv_flow_store::find_template(&mut *connection, workspace, *template).await?;
        if found.is_some_and(|template| {
            template.channel == "email"
                && template.archived_at.is_none()
                && template.current_version.is_some()
        }) {
            facts.templates.insert(*template);
        }
        let found =
            zirv_flow_store::find_destination(&mut *connection, workspace, *destination).await?;
        if found.is_some_and(|destination| {
            destination.archived_at.is_none()
                && registry
                    .get(&destination.kind)
                    .is_some_and(|driver| driver.spec().abilities(&destination.config).send)
        }) {
            facts.destinations.insert(*destination);
        }
        if let Some(topic) = topic {
            let found = zirv_flow_store::find_topic(&mut *connection, workspace, *topic).await?;
            if found.is_some_and(|topic| topic.channel == Channel::Email) {
                facts.topics.insert(*topic);
            }
        }
        if zirv_flow_store::unknown_dimension_entry(connection, workspace, dimensions)
            .await?
            .is_some()
        {
            facts.unknown_dimensions.insert(node.id.clone());
        }
    }
    Ok(facts)
}

/// [`check_document`] in a read-only transaction of its own that is dropped, for a caller that
/// holds no connection: the assistant checks a drafted flow this way.
pub(crate) async fn check_document_read(
    state: &AppState,
    workspace: zirv_flow_core::WorkspaceId,
    graph: &FlowGraph,
    settings: &FlowSettings,
) -> Result<super::validation::Checked, ApiError> {
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let (checked, _) = check_document(state, &mut tx, workspace, graph, settings).await?;
    drop(tx);
    Ok(checked)
}

/// The workspace's flows by name; archived ones with `archived=true`.
#[utoipa::path(
    get,
    operation_id = "list_flows",
    path = "/api/v1/workspaces/{ws}/flows",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("archived" = Option<bool>, Query, description = "Include archived flows; default false")
    ),
    responses(
        (status = 200, description = "The flows", body = Vec<FlowBody>),
        (status = 400, description = "archived is not true or false", body = Problem, content_type = "application/problem+json"),
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
    RawQuery(query): RawQuery,
) -> Result<Json<Vec<FlowBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let archived = bool_param(query.as_deref().unwrap_or_default(), "archived")?;
    let flows = zirv_flow_store::list_flows(&state.pool, workspace.id, archived).await?;
    Ok(Json(flows.iter().map(FlowBody::from).collect()))
}

/// Creates a draft flow. The graph and the settings must be flow documents, but they need not be
/// complete or valid: that is checked by validate and publish. Audited as `flow.created`.
#[utoipa::path(
    post,
    operation_id = "create_flow",
    path = "/api/v1/workspaces/{ws}/flows",
    tag = "flows",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateFlowRequest,
    responses(
        (status = 201, description = "The new draft flow", body = FlowDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A flow that is not archived has that name", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name, or a graph or settings that are not flow documents (`code` is `flow_malformed`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateFlowRequest>,
) -> Result<(StatusCode, Json<FlowDetailBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let name = check_name(&body.name)?.to_owned();
    let (_, graph) = parse_graph(body.graph.unwrap_or_else(empty_graph))?;
    let (_, settings) = parse_settings(body.settings.unwrap_or_else(default_settings))?;
    let new = NewFlow {
        id: Uuid::now_v7(),
        name,
        graph,
        settings,
    };
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let flow = zirv_flow_store::create_flow(&mut tx, workspace.id, &new, principal.user_id())
        .await
        .map_err(name_taken)?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "flow.created",
        Some(("flow", flow.id)),
        json!({ "name": flow.name }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((StatusCode::CREATED, Json(FlowDetailBody::from(&flow))))
}

/// One flow with its draft graph and settings.
#[utoipa::path(
    get,
    operation_id = "get_flow",
    path = "/api/v1/workspaces/{ws}/flows/{id}",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id")
    ),
    responses(
        (status = 200, description = "The flow", body = FlowDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or flow, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<FlowDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let flow = zirv_flow_store::find_flow(&state.pool, workspace.id, path_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(FlowDetailBody::from(&flow)))
}

/// Changes the name, the draft graph or the draft settings of a flow that is not archived.
/// A published version is never touched. Audited as `flow.updated`.
#[utoipa::path(
    patch,
    operation_id = "update_flow",
    path = "/api/v1/workspaces/{ws}/flows/{id}",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id")
    ),
    request_body = UpdateFlowRequest,
    responses(
        (status = 200, description = "The updated flow", body = FlowDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or flow, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Another flow has that name, or the flow is archived (`code` is `flow_archived`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name, no field at all, or a graph or settings that are not flow documents (`flow_malformed`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateFlowRequest>,
) -> Result<Json<FlowDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let id = path_id(&id)?;
    let mut fields: Vec<&'static str> = Vec::new();
    let name = body
        .name
        .as_deref()
        .map(check_name)
        .transpose()?
        .map(str::to_owned);
    if name.is_some() {
        fields.push("name");
    }
    let graph = body
        .graph
        .map(parse_graph)
        .transpose()?
        .map(|(_, stored)| stored);
    if graph.is_some() {
        fields.push("graph");
    }
    let settings = body
        .settings
        .map(parse_settings)
        .transpose()?
        .map(|(_, stored)| stored);
    if settings.is_some() {
        fields.push("settings");
    }
    if fields.is_empty() {
        return Err(invalid("body", "must change at least one field"));
    }
    let patch = FlowPatch {
        name,
        graph,
        settings,
    };
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let flow = zirv_flow_store::update_flow_draft(&mut tx, workspace.id, id, &patch)
        .await
        .map_err(flow_error)?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "flow.updated",
        Some(("flow", flow.id)),
        json!({ "name": flow.name, "fields": fields }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(Json(FlowDetailBody::from(&flow)))
}

/// Checks the saved draft as a publish would and returns every issue, each with a stable code and
/// the node, edge and pointer it concerns. It writes nothing.
#[utoipa::path(
    post,
    operation_id = "validate_flow",
    path = "/api/v1/workspaces/{ws}/flows/{id}/validate",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id")
    ),
    responses(
        (status = 200, description = "The issues; none means the draft can be published", body = ValidationBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or flow, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn validate(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<ValidationBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let flow = zirv_flow_store::find_flow(&mut *tx, workspace.id, path_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let (checked, _) = check_flow(&state, &mut tx, workspace.id, &flow).await?;
    drop(tx);
    Ok(Json(ValidationBody {
        valid: checked.issues.is_empty(),
        issues: issue_problems(&checked.issues),
    }))
}

/// The versions of a flow, newest first.
#[utoipa::path(
    get,
    operation_id = "list_flow_versions",
    path = "/api/v1/workspaces/{ws}/flows/{id}/versions",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id")
    ),
    responses(
        (status = 200, description = "The versions", body = Vec<FlowVersionBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or flow, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn versions(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<Vec<FlowVersionBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id = path_id(&id)?;
    zirv_flow_store::find_flow(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let versions = zirv_flow_store::list_flow_versions(&state.pool, workspace.id, id).await?;
    Ok(Json(versions.iter().map(FlowVersionBody::from).collect()))
}

/// One version of a flow.
#[utoipa::path(
    get,
    operation_id = "get_flow_version",
    path = "/api/v1/workspaces/{ws}/flows/{id}/versions/{n}",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id"),
        ("n" = i32, Path, description = "Version number")
    ),
    responses(
        (status = 200, description = "The version", body = FlowVersionBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, flow or version, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn version(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id, n)): Path<(String, String, String)>,
) -> Result<Json<FlowVersionBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let version =
        zirv_flow_store::find_flow_version(&state.pool, workspace.id, path_id(&id)?, path_id(&n)?)
            .await?
            .ok_or_else(ApiError::not_found)?;
    Ok(Json(FlowVersionBody::from(&version)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_trimmed_and_bounded_to_100_characters() {
        assert_eq!(check_name("  Welcome ").unwrap(), "Welcome");
        assert!(check_name("   ").is_err());
        assert!(check_name(&"x".repeat(100)).is_ok());
        assert!(check_name(&"x".repeat(101)).is_err());
        assert!(check_name("two\nlines").is_err());
    }

    #[test]
    fn a_create_request_needs_a_name_and_defaults_the_documents() {
        let request: CreateFlowRequest = serde_json::from_str(r#"{"name":"A"}"#).unwrap();

        assert!(request.graph.is_none() && request.settings.is_none());
        assert!(serde_json::from_str::<CreateFlowRequest>(r#"{"graph":{}}"#).is_err());
        let (graph, _) = parse_graph(empty_graph()).unwrap();
        assert!(graph.nodes.is_empty());
        parse_settings(default_settings()).unwrap();
    }

    #[test]
    fn the_state_schema_lists_exactly_the_lifecycle_states() {
        let schema = serde_json::to_value(state_schema()).unwrap();

        assert_eq!(
            schema["enum"],
            json!(["draft", "published", "paused", "archived"])
        );
    }

    #[test]
    fn a_document_that_is_not_a_flow_document_is_malformed() {
        let wrong_type =
            parse_graph(json!({"v": 1, "nodes": [{"id": "a", "type": "nope"}], "edges": []}));
        let unknown_key = parse_settings(
            json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}, "extra": 1}),
        );
        let not_object = parse_graph(json!([1, 2]));

        for refused in [
            wrong_type.map(|_| ()),
            unknown_key.map(|_| ()),
            not_object.map(|_| ()),
        ] {
            let error = refused.unwrap_err();
            assert_eq!(error.code(), Some("flow_malformed"));
            assert!(error.detail().chars().count() < 400);
        }
    }
}
