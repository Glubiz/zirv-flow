//! Drafting a mapping or a manifest with the assistant (architecture 13.2, 13.3), and accepting
//! the result.
//!
//! `POST .../ai/drafts` runs one synchronous draft: the server reads what the model may know
//! through the `read_source` tool, asks the model to call the kind's one draft tool, checks each
//! attempt with the validator a save uses, feeds the problems back at most
//! `ai.max_repair_attempts` times, and answers with the validated draft, its field-by-field diff
//! against the saved artifact and a dry run. Only a validated draft is stored.
//!
//! `POST .../ai/drafts/{id}/accept` checks the stored draft again against the current state and
//! returns it for the editor, which holds it as the ordinary unsaved draft. Nothing here saves a
//! mapping or a manifest: that stays the human action with its own permission and audit.
//!
//! Every attempt and every tool call runs as the signed-in user. Nothing here puts a prompt, a
//! document, a sample, a draft or a dry run in a log, a metric label or the audit trail.

use std::fmt;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::Duration;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_ai::{
    BoxFuture, ChatResponse, RepairError, RepairOutcome, ToolError, ToolOutput, Verdict,
};
use zirv_flow_connectors::manifest;
use zirv_flow_core::mapping::Mapping;
use zirv_flow_core::token::sha256;
use zirv_flow_core::{UserId, WorkspaceId};
use zirv_flow_store::{
    AiDraft, CatalogDetails, DraftKind, DraftStatus, NewAiDraft, PgConnection, Segment,
    SegmentVersion, Source, Stream,
};

use super::diff::{self, Change, MANIFEST_KEYS, MAPPING_KEYS, SEGMENT_KEYS};
use super::dispatch::{DispatchError, Dispatcher};
use super::prompt::{self, ManifestContext, render_manifest_context};
use super::segment_context::{self, Placeholders};
use super::tools::draft_segment::check_definition;
use super::tools::{self, DraftTarget, ToolContext};
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::imports::MappingBody;
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem, SegmentIssueProblem};
use crate::segments::catalog_of;
use crate::sources::{
    MappingIssue, PreviewBody, SampleBody, check_sample, is_push, keeps_connection,
    mapping_check_of, preview_mapping, problems, sample_manifest, saved_mapping_body,
};

/// The longest prompt, in characters.
const MAX_PROMPT_CHARS: usize = 2000;
/// The most bytes of pasted documentation.
const MAX_DOCUMENTATION_BYTES: usize = 32 * 1024;
/// The most bytes a draft request may hold: documentation, a sample, the prompt and the framing.
pub(crate) const MAX_DRAFT_REQUEST_BYTES: usize = 160 * 1024;
/// The records of a dry run.
const DRY_RUN_RECORDS: u32 = 5;
/// How long a draft is kept.
const DRAFT_DAYS: i64 = 7;

/// What a draft is of.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DraftKindBody {
    Mapping,
    Manifest,
    Segment,
}

impl DraftKindBody {
    fn store(self) -> DraftKind {
        match self {
            Self::Mapping => DraftKind::Mapping,
            Self::Manifest => DraftKind::Manifest,
            Self::Segment => DraftKind::Segment,
        }
    }
}

/// What to draft.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DraftRequest {
    kind: DraftKindBody,
    /// The source. Required for a mapping. For a manifest it is the `http` source being edited;
    /// absent for a source not yet saved.
    #[serde(default)]
    source_id: Option<Uuid>,
    /// The stream whose mapping is drafted. Required for a mapping, refused for a manifest.
    #[serde(default)]
    stream: Option<String>,
    /// For a segment: the segment being edited; absent for a new one. Refused for the other kinds.
    #[serde(default)]
    segment_id: Option<Uuid>,
    /// What the user wants, 1 to 2000 characters. It is sent as written: do not paste personal
    /// data into it.
    prompt: String,
    /// For a manifest: the API documentation to base it on, at most 32 KiB. It is sent to the
    /// model as data, never as instructions.
    #[serde(default)]
    documentation: Option<String>,
    /// For a webhook stream, which has no source to read: 1 to 20 JSON objects (64 KiB in all).
    /// Its top-level keys are the stream's fields for this draft. Required for a webhook stream
    /// and refused for any other.
    #[serde(default)]
    #[schema(value_type = Option<Vec<std::collections::BTreeMap<String, serde_json::Value>>>)]
    sample: Option<Vec<Value>>,
}

/// Checks the shape of a request. Each problem names the field, never a value.
fn check_request(request: &DraftRequest) -> Result<(), ApiError> {
    let invalid = crate::sources::invalid;
    let prompt = request.prompt.trim().chars().count();
    if prompt == 0 || request.prompt.chars().count() > MAX_PROMPT_CHARS {
        return Err(invalid("prompt", "must be 1 to 2000 characters"));
    }
    if request.segment_id.is_some() && request.kind != DraftKindBody::Segment {
        return Err(invalid("segment_id", "belongs to a segment only"));
    }
    match request.kind {
        DraftKindBody::Mapping => {
            if request.source_id.is_none() {
                return Err(invalid("source_id", "is required for a mapping"));
            }
            if request.stream.as_deref().is_none_or(str::is_empty) {
                return Err(invalid("stream", "is required for a mapping"));
            }
            if request.documentation.is_some() {
                return Err(invalid("documentation", "belongs to a manifest only"));
            }
        }
        DraftKindBody::Manifest => {
            if request.stream.is_some() {
                return Err(invalid("stream", "belongs to a mapping only"));
            }
            if request.sample.is_some() {
                return Err(ApiError::unprocessable(
                    "sample: only a webhook stream takes a sample payload",
                )
                .with_code("sample_not_supported"));
            }
            if request
                .documentation
                .as_ref()
                .is_some_and(|text| text.len() > MAX_DOCUMENTATION_BYTES)
            {
                return Err(invalid("documentation", "must be at most 32 KiB"));
            }
        }
        DraftKindBody::Segment => {
            if request.source_id.is_some() {
                return Err(invalid(
                    "source_id",
                    "belongs to a mapping or manifest only",
                ));
            }
            if request.stream.is_some() {
                return Err(invalid("stream", "belongs to a mapping only"));
            }
            if request.documentation.is_some() {
                return Err(invalid("documentation", "belongs to a manifest only"));
            }
            if request.sample.is_some() {
                return Err(ApiError::unprocessable(
                    "sample: only a webhook stream takes a sample payload",
                )
                .with_code("sample_not_supported"));
            }
        }
    }
    Ok(())
}

/// What a run puts back into the model's answer before it is checked: the values it was not shown.
#[derive(Clone, Debug, Default)]
pub(crate) enum Restore {
    #[default]
    Nothing,
    /// The saved manifest, whose static header and query values were masked.
    Manifest(Value),
    /// The literals of a saved segment definition that were masked.
    Segment(Placeholders),
}

/// One problem of a draft, as a code and where it is: a column of a mapping or a JSON pointer in
/// a manifest. It never holds a value.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub(crate) struct DraftIssue {
    /// A stable code such as `unknown_column`, `unknown_list` or `path_invalid`.
    code: String,
    /// A column name or a JSON pointer; absent when the problem is on the whole draft.
    at: Option<String>,
    /// The line shown to the model as feedback.
    #[serde(skip)]
    line: String,
}

impl DraftIssue {
    /// The stable code of the problem.
    pub(crate) fn code(&self) -> &str {
        &self.code
    }

    /// Where the problem is, when it is not on the whole draft.
    pub(crate) fn at(&self) -> Option<&str> {
        self.at.as_deref()
    }

    /// From a tool's feedback line: `code (column "x")`, `code at /pointer` or `code`.
    pub(crate) fn from_line(line: &str) -> Self {
        let (code, at) = if let Some((code, rest)) = line.split_once(" (column \"") {
            (code, rest.strip_suffix("\")").map(str::to_owned))
        } else if let Some((code, rest)) = line.split_once(" at ") {
            (code, Some(rest.to_owned()))
        } else {
            (line, None)
        };
        Self {
            code: code.to_owned(),
            at,
            line: line.to_owned(),
        }
    }

    /// [`Self::from_line`] for each line of a refusal.
    pub(crate) fn from_lines(lines: &[String]) -> Vec<Self> {
        lines.iter().map(|line| Self::from_line(line)).collect()
    }
}

/// The line the model is shown.
impl fmt::Display for DraftIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.line)
    }
}

/// Why a dry run did not show records.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct DryRunProblem {
    status: u16,
    code: Option<String>,
    title: String,
}

/// What the draft does on real data: the #87 preview of a mapping, or the sample of a manifest.
#[derive(Serialize, ToSchema)]
pub(crate) struct DryRunBody {
    /// `ok`, `unavailable` (it cannot run, see `reason`) or `failed` (see `problem`).
    status: &'static str,
    /// For `unavailable`: `source_not_saved` or `connection_changed`.
    reason: Option<&'static str>,
    /// The stream the manifest sample read.
    stream: Option<String>,
    preview: Option<PreviewBody>,
    sample: Option<SampleBody>,
    problem: Option<DryRunProblem>,
}

impl DryRunBody {
    fn none(status: &'static str) -> Self {
        Self {
            status,
            reason: None,
            stream: None,
            preview: None,
            sample: None,
            problem: None,
        }
    }

    fn unavailable(reason: &'static str) -> Self {
        Self {
            reason: Some(reason),
            ..Self::none("unavailable")
        }
    }

    /// A dry run that failed does not fail the run: the draft stands, with the reason.
    fn failed(error: &ApiError) -> Self {
        let status = error.status_code();
        Self {
            problem: Some(DryRunProblem {
                status: status.as_u16(),
                code: error.code().map(str::to_owned),
                title: status.canonical_reason().unwrap_or("error").to_owned(),
            }),
            ..Self::none("failed")
        }
    }
}

/// The result of a draft run.
#[derive(Serialize, ToSchema)]
pub(crate) struct DraftBody {
    id: Uuid,
    kind: DraftKindBody,
    /// `valid`, or `invalid` when no attempt passed validation: then there is no `content` and
    /// the draft cannot be accepted.
    status: &'static str,
    /// How many times the model was asked.
    attempts: u32,
    /// The validated draft, in the form the editor takes.
    #[schema(value_type = Option<Object>)]
    content: Option<Value>,
    /// For an invalid draft, the problems of the last attempt.
    issues: Vec<DraftIssue>,
    /// The draft against the saved mapping or manifest, field by field.
    changes: Vec<Change>,
    dry_run: Option<DryRunBody>,
}

/// An accepted draft, for the editor to load.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct AcceptedDraftBody {
    id: Uuid,
    kind: DraftKindBody,
    source_id: Option<Uuid>,
    stream: Option<String>,
    segment_id: Option<Uuid>,
    #[schema(value_type = Object)]
    content: Value,
}

fn kind_body(kind: DraftKind) -> DraftKindBody {
    match kind {
        DraftKind::Mapping => DraftKindBody::Mapping,
        DraftKind::Manifest => DraftKindBody::Manifest,
        DraftKind::Segment => DraftKindBody::Segment,
    }
}

/// The one tool a run offers for a kind.
pub(crate) fn tool_name(kind: DraftKind) -> &'static str {
    match kind {
        DraftKind::Mapping => "draft_mapping",
        DraftKind::Manifest => "draft_manifest",
        DraftKind::Segment => "draft_segment",
    }
}

/// The feedback for an answer that is not exactly one call of the draft tool.
pub(crate) fn wrong_answer(want: &str) -> DraftIssue {
    DraftIssue::from_line(&format!("answer by calling {want} exactly once"))
}

/// The arguments of the answer's one call of `want`. None for anything else: no call, several, or
/// a tool of another name (a registered one or one the model made up), none of which is run.
pub(crate) fn expected_call<'r>(response: &'r ChatResponse, want: &str) -> Option<&'r Value> {
    let mut calls = response.tool_calls();
    let (_, name, arguments) = calls.next()?;
    (name == want && calls.next().is_none()).then_some(arguments)
}

/// Checks one answer of the model. It must be exactly one call of the kind's draft tool; the
/// call runs through the registry, as the signed-in user. Anything else, such as a call of a tool
/// the model made up, is feedback and executes nothing. A refusal (403, 404, an outage) ends the
/// run instead of becoming feedback.
///
/// A manifest draft gets the saved header and query values back first, wherever it kept the
/// placeholder the model was shown in their place, so the validator, the diff and the stored
/// draft all see the real manifest.
///
/// A segment draft gets the literals it was shown as placeholders back the same way, and a
/// placeholder that is not in the node it was sent in is feedback.
async fn validate_turn(
    context: &ToolContext,
    kind: DraftKind,
    restore: &Restore,
    response: &ChatResponse,
) -> Verdict<Value, DraftIssue, ApiError> {
    let want = tool_name(kind);
    let feedback = || Verdict::Invalid(vec![wrong_answer(want)]);
    let Some(arguments) = expected_call(response, want) else {
        return feedback();
    };
    let Some(tool) = tools::find(want) else {
        return feedback();
    };
    let mut restored = arguments.clone();
    if let (Restore::Manifest(saved), Some(draft)) = (restore, restored.get_mut("manifest")) {
        prompt::restore_kept(draft, saved);
    }
    if let (Restore::Segment(table), Some(draft)) = (restore, restored.get_mut("definition")) {
        let left = table.restore(draft);
        if !left.is_empty() {
            return Verdict::Invalid(DraftIssue::from_lines(&left));
        }
    }
    if let Some(draft) = restored.get("manifest") {
        let left = prompt::leftover_placeholders(draft);
        if !left.is_empty() {
            return Verdict::Invalid(DraftIssue::from_lines(&left));
        }
    }
    match tool.call(context, &restored).await {
        Ok(ToolOutput::Draft(draft)) => Verdict::Valid(draft),
        Ok(ToolOutput::Context(_)) => feedback(),
        Err(ToolError::Invalid(lines)) => Verdict::Invalid(DraftIssue::from_lines(&lines)),
        Err(ToolError::Failed(error)) => Verdict::Abort(error),
    }
}

/// [`validate_turn`] as the boxed future `repair` takes.
fn turn<'a>(
    context: Arc<ToolContext>,
    kind: DraftKind,
    restore: Arc<Restore>,
    response: &'a ChatResponse,
) -> BoxFuture<'a, Verdict<Value, DraftIssue, ApiError>> {
    // The future owns the context: it may not borrow what lives shorter than the response.
    Box::pin(async move { validate_turn(&context, kind, &restore, response).await })
}

/// The SHA-256 of a saved artifact's JSON.
fn digest(value: &Value) -> [u8; 32] {
    sha256(&value.to_string())
}

/// The saved mapping or manifest a draft is compared with.
fn saved_artifact(
    kind: DraftKind,
    stream: Option<&Stream>,
    source: Option<&Source>,
    version: Option<&SegmentVersion>,
) -> Option<Value> {
    match kind {
        DraftKind::Mapping => stream
            .and_then(saved_mapping_body)
            .and_then(|body| serde_json::to_value(body).ok()),
        DraftKind::Manifest => source
            .filter(|source| source.kind == "http")
            .and_then(|source| source.config.get("manifest").cloned()),
        DraftKind::Segment => version.map(|version| version.definition.clone()),
    }
}

fn keys(kind: DraftKind) -> &'static [(&'static str, &'static str)] {
    match kind {
        DraftKind::Mapping => MAPPING_KEYS,
        DraftKind::Manifest => MANIFEST_KEYS,
        DraftKind::Segment => SEGMENT_KEYS,
    }
}

fn run_outcome(kind: DraftKind, outcome: &'static str) {
    metrics::counter!(
        "zirv_flow_ai_draft_runs_total",
        "kind" => kind.as_str(),
        "outcome" => outcome,
    )
    .increment(1);
}

/// The session user of a request, or 403 `ai_user_required` for an API key.
fn session_user(principal: &Principal) -> Result<UserId, ApiError> {
    principal.user_id().ok_or_else(ApiError::ai_user_required)
}

/// What the dry run of a mapping draft shows.
async fn dry_run_mapping(
    context: &ToolContext,
    source_id: Uuid,
    stream: &Stream,
    content: &Value,
) -> DryRunBody {
    let run = async {
        let mapping: Mapping =
            serde_json::from_value(content.clone()).map_err(|_| ApiError::internal())?;
        preview_mapping(
            &context.state,
            context.workspace,
            source_id,
            stream,
            &mapping,
            DRY_RUN_RECORDS,
            context.sample.as_deref(),
        )
        .await
    };
    match run.await {
        Ok(preview) => DryRunBody {
            preview: Some(preview),
            ..DryRunBody::none("ok")
        },
        Err(error) => DryRunBody::failed(&error),
    }
}

/// What the dry run of a manifest draft shows: the first stream, read with the saved source's
/// credential, when the draft keeps the connection the credential was set up for.
async fn dry_run_manifest(
    context: &ToolContext,
    source: Option<&Source>,
    content: &Value,
) -> DryRunBody {
    let Some(source) = source else {
        return DryRunBody::unavailable("source_not_saved");
    };
    if !keeps_connection(&source.config, content) {
        return DryRunBody::unavailable("connection_changed");
    }
    let first = manifest::validate(content)
        .ok()
        .and_then(|manifest| manifest.streams.into_iter().next())
        .map(|stream| stream.name);
    let Some(first) = first else {
        return DryRunBody::unavailable("source_not_saved");
    };
    match sample_manifest(
        &context.state,
        context.workspace,
        source.id,
        Some(content),
        &first,
        DRY_RUN_RECORDS,
    )
    .await
    {
        Ok(sample) => DryRunBody {
            stream: Some(first),
            sample: Some(sample),
            ..DryRunBody::none("ok")
        },
        Err(error) => DryRunBody::failed(&error),
    }
}

/// Drafts a mapping or a manifest with the assistant: the model is asked, its answer validated
/// with the rules a save applies, and problems are fed back for at most `ai.max_repair_attempts`
/// repairs. The answer is the validated draft with its diff against the saved version and a dry
/// run, or the last problems. Only a validated draft can be accepted. Workspace admins who are
/// signed in; an API key is refused. Every attempt is a gated model call: it counts against the
/// budget and the rate limit, and with the assistant off, nothing is read or sent. A reverse
/// proxy in front needs a read timeout of at least (1 + `ai.max_repair_attempts`) x 60 s + 30 s.
#[utoipa::path(
    post,
    operation_id = "create_ai_draft",
    path = "/api/v1/workspaces/{ws}/ai/drafts",
    tag = "ai",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = DraftRequest,
    responses(
        (status = 201, description = "The draft run: a validated draft with its changes and dry run, or the problems of the last attempt", body = DraftBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 402, description = "The workspace's monthly AI budget is spent (`ai_budget_exceeded`); nothing was sent", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or an API key (`ai_user_required`)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, source or stream, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The assistant is off (`ai_disabled`), or the source does not use a manifest", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is over 160 KiB", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A request that breaks a rule, naming the field, such as a missing prompt, `sample_required` or `sample_not_supported`", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many AI calls (`ai_rate_limited`); try again in a minute", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The model provider failed (`ai_provider_error`), or the source did", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to open the provider key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(request): ApiJson<DraftRequest>,
) -> Result<(StatusCode, Json<DraftBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let user = session_user(&principal)?;
    check_request(&request)?;
    let kind = request.kind.store();
    // The assistant being off is the first answer, before anything is read.
    let dispatcher = Dispatcher::open(&state, Some(user), workspace.id, kind.as_str())
        .await
        .map_err(ApiError::from)?;

    // The target, found within the workspace.
    let (source, stream) = match (kind, request.source_id) {
        (DraftKind::Mapping, Some(source_id)) => {
            let source = zirv_flow_store::find_source(&state.pool, workspace.id, source_id)
                .await?
                .ok_or_else(ApiError::not_found)?;
            let name = request.stream.as_deref().unwrap_or_default();
            let stream = zirv_flow_store::find_stream(&state.pool, workspace.id, source_id, name)
                .await?
                .ok_or_else(ApiError::not_found)?;
            (Some(source), Some(stream))
        }
        (DraftKind::Manifest, Some(source_id)) => {
            let source = zirv_flow_store::find_source(&state.pool, workspace.id, source_id)
                .await?
                .ok_or_else(ApiError::not_found)?;
            if source.kind != "http" {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "this source does not use a manifest",
                ));
            }
            (Some(source), None)
        }
        _ => (None, None),
    };
    // The segment being edited, within the workspace; a new segment has none.
    let segment = match (kind, request.segment_id) {
        (DraftKind::Segment, Some(segment_id)) => {
            Some(current_segment(&state, workspace.id, segment_id).await?)
        }
        _ => None,
    };
    let sample = match (&source, &request.sample) {
        (Some(source), sample) if kind == DraftKind::Mapping => {
            match (is_push(&source.kind), sample) {
                (true, None) => {
                    return Err(ApiError::unprocessable(
                        "sample: a webhook stream has no source to read; send a sample payload",
                    )
                    .with_code("sample_required"));
                }
                (false, Some(_)) => {
                    return Err(ApiError::unprocessable(
                        "sample: only a webhook stream takes a sample payload",
                    )
                    .with_code("sample_not_supported"));
                }
                (true, Some(sample)) => Some(check_sample(sample)?.to_vec()),
                (false, None) => None,
            }
        }
        _ => None,
    };

    let target = match kind {
        DraftKind::Mapping => DraftTarget::Mapping {
            source_id: request.source_id.unwrap_or_default(),
            stream: request.stream.clone().unwrap_or_default(),
        },
        DraftKind::Manifest => DraftTarget::Manifest,
        DraftKind::Segment => DraftTarget::Segment {
            segment_id: request.segment_id,
        },
    };
    let context = Arc::new(ToolContext {
        state: state.clone(),
        principal,
        workspace: workspace.id,
        privacy: dispatcher.privacy(),
        target,
        sample,
    });

    let saved = saved_artifact(
        kind,
        stream.as_ref(),
        source.as_ref(),
        segment.as_ref().map(|(_, version)| version),
    );
    // What the model may know, read by the server: through the registered tool for a source, from
    // the workspace's catalog for a segment.
    let mut placeholders = Placeholders::default();
    let known = match (kind, request.source_id) {
        (DraftKind::Segment, _) => {
            let details = segment_details(&state, workspace.id).await?;
            let (parts, table) = segment_context::draft_context(
                &details,
                request.segment_id,
                saved.as_ref(),
                context.privacy,
            );
            placeholders = table;
            parts
        }
        (DraftKind::Manifest, None) => render_manifest_context(&ManifestContext::default()),
        (_, Some(source_id)) => {
            let reader = tools::find("read_source").ok_or_else(ApiError::internal)?;
            let mut arguments = json!({"source_id": source_id});
            if let Some(name) = &request.stream {
                arguments["stream"] = json!(name);
            }
            match reader.call(&context, &arguments).await {
                Ok(ToolOutput::Context(parts)) => parts,
                Ok(ToolOutput::Draft(_)) | Err(ToolError::Invalid(_)) => {
                    return Err(ApiError::internal());
                }
                Err(ToolError::Failed(error)) => return Err(error),
            }
        }
        (DraftKind::Mapping, None) => return Err(ApiError::internal()),
    };
    let draft_tool = tools::find(tool_name(kind)).ok_or_else(ApiError::internal)?;
    let chat = prompt::build_request(
        kind,
        context.privacy,
        known,
        request.prompt.trim(),
        request.documentation.as_deref(),
        draft_tool.spec().map_err(|_| ApiError::internal())?,
    )
    .map_err(|error| ApiError::from(DispatchError::Ai(error)))?;

    let id = Uuid::now_v7();
    let dispatcher = dispatcher.for_draft(id);
    let owned = Arc::clone(&context);
    // A manifest and a segment have values they were not shown to put back.
    let restorable = Arc::new(match (kind, &saved) {
        (DraftKind::Manifest, Some(saved)) => Restore::Manifest(saved.clone()),
        (DraftKind::Segment, _) => Restore::Segment(placeholders),
        _ => Restore::Nothing,
    });
    let outcome = zirv_flow_ai::repair(
        state.ai.config.max_repair_attempts,
        &chat,
        &dispatcher,
        move |response| turn(Arc::clone(&owned), kind, Arc::clone(&restorable), response),
    )
    .await;

    let now = zirv_flow_store::database_now(&state.pool).await?;
    let expires_at = now + Duration::days(DRAFT_DAYS);
    let (source_id, stream_name) = (request.source_id, request.stream.as_deref());
    let segment_id = request.segment_id;
    let body = match outcome {
        Ok(RepairOutcome::Valid(content, attempts)) => {
            let changes = diff::changes(saved.as_ref(), &content, keys(kind));
            let mut tx = zirv_flow_store::begin(&state.pool).await?;
            zirv_flow_store::insert_ai_draft(
                &mut tx,
                &NewAiDraft {
                    id,
                    workspace_id: workspace.id,
                    user_id: user,
                    kind,
                    source_id,
                    stream_name,
                    segment_id,
                    status: DraftStatus::Valid,
                    content: Some(&content),
                    base_digest: saved.as_ref().map(digest),
                    expires_at,
                },
            )
            .await?;
            zirv_flow_store::commit(tx).await?;
            // A segment has no dry run on the server: the builder shows the live count of the draft.
            let dry_run = match (kind, &stream, source_id) {
                (DraftKind::Segment, _, _) => None,
                (_, Some(stream), Some(source_id)) => {
                    Some(dry_run_mapping(&context, source_id, stream, &content).await)
                }
                _ => Some(dry_run_manifest(&context, source.as_ref(), &content).await),
            };
            run_outcome(kind, "valid");
            DraftBody {
                id,
                kind: request.kind,
                status: "valid",
                attempts,
                content: Some(content),
                issues: Vec::new(),
                changes,
                dry_run,
            }
        }
        Ok(RepairOutcome::Exhausted(issues, attempts)) => {
            let mut tx = zirv_flow_store::begin(&state.pool).await?;
            zirv_flow_store::insert_ai_draft(
                &mut tx,
                &NewAiDraft {
                    id,
                    workspace_id: workspace.id,
                    user_id: user,
                    kind,
                    source_id,
                    stream_name,
                    segment_id,
                    status: DraftStatus::Invalid,
                    content: None,
                    base_digest: None,
                    expires_at,
                },
            )
            .await?;
            zirv_flow_store::commit(tx).await?;
            run_outcome(kind, "invalid");
            DraftBody {
                id,
                kind: request.kind,
                status: "invalid",
                attempts,
                content: None,
                issues,
                changes: Vec::new(),
                dry_run: None,
            }
        }
        Err(RepairError::Dispatch(error)) => {
            run_outcome(kind, "failed");
            return Err(DispatchError::Ai(error).into());
        }
        Err(RepairError::Aborted(error)) => {
            run_outcome(kind, "failed");
            return Err(error);
        }
    };
    Ok((StatusCode::CREATED, Json(body)))
}

/// Re-checks a stored draft against the current state: the saved version is unchanged since the
/// draft was made, and the content still passes the rules a save applies.
///
/// It runs in the transaction that marks the draft accepted, and reads the stream (a mapping) or
/// the source (a manifest) under the row lock a save takes first. A save that commits after the
/// read waits for this transaction, so a draft is never accepted over a version it did not see.
async fn recheck(
    state: &AppState,
    tx: &mut PgConnection,
    draft: &AiDraft,
    content: &Value,
) -> Result<(), ApiError> {
    if draft.kind == DraftKind::Segment {
        return recheck_segment(tx, draft, content).await;
    }
    let Some(source_id) = draft.source_id else {
        // A manifest draft for a source not created yet has no saved version to lock or compare.
        return recheck_manifest(content);
    };
    let (source, stream) = match draft.kind {
        DraftKind::Mapping => {
            let name = draft.stream_name.as_deref().unwrap_or_default();
            let stream =
                zirv_flow_store::find_stream_for_update(tx, draft.workspace_id, source_id, name)
                    .await?
                    .ok_or_else(ApiError::not_found)?;
            let source = zirv_flow_store::find_source(&mut *tx, draft.workspace_id, source_id)
                .await?
                .ok_or_else(ApiError::not_found)?;
            (source, Some(stream))
        }
        DraftKind::Manifest => {
            let source = zirv_flow_store::find_source_for_update(tx, draft.workspace_id, source_id)
                .await?
                .ok_or_else(ApiError::not_found)?;
            (source, None)
        }
        // A segment draft was handled above.
        DraftKind::Segment => return Err(ApiError::internal()),
    };
    let saved = saved_artifact(draft.kind, stream.as_ref(), Some(&source), None);
    if saved.as_ref().map(digest) != draft.base_digest {
        return Err(ApiError::ai_draft_outdated());
    }
    match draft.kind {
        DraftKind::Mapping => {
            let body: MappingBody =
                serde_json::from_value(content.clone()).map_err(|_| ApiError::internal())?;
            let stream = stream.as_ref().ok_or_else(ApiError::internal)?;
            let check = mapping_check_of(state, draft.workspace_id, &source, stream).await?;
            check
                .check(&body)
                .map(|_| ())
                .map_err(|issue: MappingIssue| ApiError::from(issue))
        }
        DraftKind::Manifest => recheck_manifest(content),
        DraftKind::Segment => Err(ApiError::internal()),
    }
}

/// The segment a draft is for as it is saved now, and the workspace's catalog: read under the lock a
/// definition save takes first, so a save that commits after this read waits for the transaction
/// and a draft is never accepted over a version it did not see. The draft is outdated when the
/// current version is not the one it was made from, and refused with the issues when it no longer
/// passes the rules a save applies, such as a trait archived since. It writes nothing.
async fn recheck_segment(
    tx: &mut PgConnection,
    draft: &AiDraft,
    content: &Value,
) -> Result<(), ApiError> {
    let workspace = draft.workspace_id;
    zirv_flow_store::lock_segments(tx, workspace).await?;
    let saved = match draft.segment_id {
        Some(segment_id) => Some(current_definition(tx, workspace, segment_id).await?),
        None => None,
    };
    if saved.as_ref().map(digest) != draft.base_digest {
        return Err(ApiError::ai_draft_outdated());
    }
    let (catalog, storage) = catalog_of(zirv_flow_store::segment_catalog(tx, workspace).await?);
    let now = zirv_flow_store::database_now(&mut *tx).await?;
    check_definition(content, &catalog, &storage, draft.segment_id, now)
        .map(|_| ())
        .map_err(|issues| {
            ApiError::ai_draft_invalid_issues(
                issues
                    .into_iter()
                    .map(|issue| SegmentIssueProblem {
                        pointer: issue.pointer,
                        code: issue.code.to_owned(),
                    })
                    .collect(),
            )
        })
}

/// The definition of a segment's current version. A segment that is gone or archived is refused
/// as a save would refuse it.
async fn current_definition(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment_id: Uuid,
) -> Result<Value, ApiError> {
    let (_, version) = current_version(connection, workspace, segment_id).await?;
    Ok(version.definition)
}

/// A segment of the workspace and its current version.
async fn current_version(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment_id: Uuid,
) -> Result<(Segment, SegmentVersion), ApiError> {
    let segment = zirv_flow_store::find_segment(&mut *connection, workspace, segment_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if segment.archived_at.is_some() {
        return Err(ApiError::segment_archived());
    }
    let version = zirv_flow_store::find_segment_version(
        &mut *connection,
        workspace,
        segment_id,
        segment.current_version,
    )
    .await?
    .ok_or_else(ApiError::internal)?;
    Ok((segment, version))
}

/// The segment being edited and its current version, for a draft run.
async fn current_segment(
    state: &AppState,
    workspace: WorkspaceId,
    segment_id: Uuid,
) -> Result<(Segment, SegmentVersion), ApiError> {
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    current_version(&mut tx, workspace, segment_id).await
}

/// What the model may be told about the workspace's model of the world.
pub(crate) async fn segment_details(
    state: &AppState,
    workspace: WorkspaceId,
) -> Result<CatalogDetails, ApiError> {
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    Ok(zirv_flow_store::segment_catalog_details(&mut tx, workspace).await?)
}

/// What the audit trail records of an accepted draft: its kind and where it points, never its
/// content.
fn audit_summary(draft: &AiDraft) -> Value {
    match draft.kind {
        DraftKind::Segment => json!({"kind": draft.kind.as_str(), "segment_id": draft.segment_id}),
        DraftKind::Mapping | DraftKind::Manifest => json!({
            "kind": draft.kind.as_str(),
            "source_id": draft.source_id,
            "stream": draft.stream_name,
        }),
    }
}

fn recheck_manifest(content: &Value) -> Result<(), ApiError> {
    let left = prompt::leftover_placeholders(content);
    if !left.is_empty() {
        return Err(problems(
            left.into_iter()
                .map(|line| {
                    let issue = DraftIssue::from_line(&line);
                    (
                        format!("manifest{}", issue.at.unwrap_or_default()),
                        issue.code,
                    )
                })
                .collect(),
        ));
    }
    manifest::validate(content).map(|_| ()).map_err(|issues| {
        problems(
            issues
                .into_iter()
                .map(|issue| (format!("manifest{}", issue.path), issue.code.to_owned()))
                .collect(),
        )
    })
}

/// Accepts a draft of the signed-in user: checks that it is valid, that the saved version has not
/// changed since and that it still passes the rules a save applies, then returns it for the
/// editor to load as its unsaved draft. Nothing is written to the stream or the source; saving
/// stays a separate action. Audited as `ai.draft.accepted`. Workspace admins who are signed in.
#[utoipa::path(
    post,
    operation_id = "accept_ai_draft",
    path = "/api/v1/workspaces/{ws}/ai/drafts/{id}/accept",
    tag = "ai",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Draft id")
    ),
    responses(
        (status = 200, description = "The draft, to load into the editor", body = AcceptedDraftBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or an API key (`ai_user_required`)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such draft of this user in this workspace, or it expired", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The draft never validated (`ai_draft_invalid`), or the saved version changed since (`ai_draft_outdated`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The draft no longer passes the rules a save applies, such as a trait archived since (the same codes as a save)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn accept(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<AcceptedDraftBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let user = session_user(&principal)?;
    let id = path_id::<Uuid>(&id)?;
    let now = zirv_flow_store::database_now(&state.pool).await?;
    let draft = zirv_flow_store::find_ai_draft(&state.pool, workspace.id, user, id, now)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if draft.status == DraftStatus::Invalid {
        return Err(ApiError::ai_draft_invalid());
    }
    let content = draft
        .content
        .clone()
        .ok_or_else(ApiError::ai_draft_invalid)?;

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    recheck(&state, &mut tx, &draft, &content).await?;
    if !zirv_flow_store::mark_ai_draft_accepted(&mut tx, workspace.id, user, id, now).await? {
        return Err(ApiError::not_found());
    }
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "ai.draft.accepted",
        Some(("ai_draft", id)),
        audit_summary(&draft),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    metrics::counter!("zirv_flow_ai_drafts_accepted_total", "kind" => draft.kind.as_str())
        .increment(1);
    Ok(Json(AcceptedDraftBody {
        id,
        kind: kind_body(draft.kind),
        source_id: draft.source_id,
        stream: draft.stream_name,
        segment_id: draft.segment_id,
        content,
    }))
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;

    fn request(value: Value) -> DraftRequest {
        serde_json::from_value(value).unwrap()
    }

    fn refused(value: Value) -> String {
        check_request(&request(value))
            .unwrap_err()
            .detail()
            .to_owned()
    }

    #[test]
    fn the_request_rules_name_each_field() {
        let source = Uuid::now_v7();

        assert_eq!(
            refused(json!({"kind": "mapping", "prompt": "  "})),
            "prompt must be 1 to 2000 characters"
        );
        assert_eq!(
            refused(json!({"kind": "mapping", "prompt": "x".repeat(2001)})),
            "prompt must be 1 to 2000 characters"
        );
        assert_eq!(
            refused(json!({"kind": "mapping", "prompt": "map it"})),
            "source_id is required for a mapping"
        );
        assert_eq!(
            refused(json!({"kind": "mapping", "prompt": "map it", "source_id": source})),
            "stream is required for a mapping"
        );
        assert_eq!(
            refused(
                json!({"kind": "mapping", "prompt": "map it", "source_id": source,
                "stream": "contacts", "documentation": "docs"})
            ),
            "documentation belongs to a manifest only"
        );
        assert_eq!(
            refused(json!({"kind": "manifest", "prompt": "read", "stream": "contacts"})),
            "stream belongs to a mapping only"
        );
        assert!(
            refused(json!({"kind": "manifest", "prompt": "read", "sample": [{}]}))
                .starts_with("sample:")
        );
        assert_eq!(
            refused(json!({"kind": "manifest", "prompt": "read",
                "documentation": "x".repeat(MAX_DOCUMENTATION_BYTES + 1)})),
            "documentation must be at most 32 KiB"
        );
        for fine in [
            json!({"kind": "mapping", "prompt": "map it", "source_id": source, "stream": "s"}),
            json!({"kind": "manifest", "prompt": "read", "documentation": "docs"}),
            json!({"kind": "manifest", "prompt": "read", "source_id": source}),
        ] {
            assert!(check_request(&request(fine)).is_ok());
        }
        // Unknown fields are refused outright.
        assert!(
            serde_json::from_value::<DraftRequest>(
                json!({"kind": "manifest", "prompt": "x", "tool": "publish"})
            )
            .is_err()
        );
    }

    #[test]
    fn an_issue_reads_a_column_or_a_pointer_from_its_line() {
        let column = DraftIssue::from_line("unknown_column (column \"Ghost\")");
        assert_eq!(
            (column.code.as_str(), column.at.as_deref()),
            ("unknown_column", Some("Ghost"))
        );
        assert_eq!(column.to_string(), "unknown_column (column \"Ghost\")");
        let pointer = DraftIssue::from_line("path_invalid at /streams/0/path");
        assert_eq!(
            (pointer.code.as_str(), pointer.at.as_deref()),
            ("path_invalid", Some("/streams/0/path"))
        );
        let plain = DraftIssue::from_line("no_identifier");
        assert_eq!((plain.code.as_str(), plain.at), ("no_identifier", None));
        // Only the code and the place reach the response.
        assert_eq!(
            serde_json::to_value(&column).unwrap(),
            json!({"code": "unknown_column", "at": "Ghost"})
        );
    }

    #[test]
    fn a_digest_follows_the_content() {
        let a = json!({"columns": []});
        assert_eq!(digest(&a), digest(&json!({"columns": []})));
        assert_ne!(digest(&a), digest(&json!({"columns": [1]})));
    }

    #[test]
    fn a_failed_dry_run_names_the_status_and_code_and_nothing_else() {
        let body = DryRunBody::failed(&ApiError::new(
            StatusCode::BAD_GATEWAY,
            "the source said SECRET-DETAIL",
        ));

        let rendered = serde_json::to_value(&body).unwrap();
        assert_eq!(rendered["status"], "failed");
        assert_eq!(rendered["problem"]["status"], 502);
        assert_eq!(rendered["problem"]["title"], "Bad Gateway");
        assert!(!rendered.to_string().contains("SECRET"));
        assert_eq!(
            serde_json::to_value(DryRunBody::unavailable("source_not_saved")).unwrap()["reason"],
            "source_not_saved"
        );
    }

    #[test]
    fn each_kind_offers_its_own_tool() {
        assert_eq!(tool_name(DraftKind::Mapping), "draft_mapping");
        assert_eq!(tool_name(DraftKind::Manifest), "draft_manifest");
        assert!(
            keys(DraftKind::Mapping)
                .iter()
                .any(|(path, _)| *path == "/columns")
        );
        assert!(
            keys(DraftKind::Manifest)
                .iter()
                .any(|(path, _)| *path == "/streams")
        );
    }

    #[test]
    fn the_expiry_is_a_week() {
        assert_eq!(DRAFT_DAYS, 7);
        let now = OffsetDateTime::UNIX_EPOCH;
        assert_eq!(
            (now + Duration::days(DRAFT_DAYS) - now).whole_hours(),
            7 * 24
        );
    }
}
