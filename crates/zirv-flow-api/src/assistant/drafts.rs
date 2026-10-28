//! Drafting a mapping, a manifest, a segment, a flow or a template with the assistant
//! (architecture 13.2, 13.3), and accepting the result.
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
//! A flow draft is accepted into the flow's ordinary draft (the same write as `PATCH .../flows/{id}`,
//! under the flow's lock, after the publish validation passes again). It never publishes and never
//! creates a version: publishing stays a separate human action under its own permission.
//!
//! A template draft is accepted into the template's ordinary draft the same way (the template's
//! lock, the draft's revision checked, the publish validation passed again) and never publishes.
//!
//! Every attempt and every tool call runs as the signed-in user. Nothing here puts a prompt, a
//! document, a sample, a draft or a dry run in a log, a metric label or the audit trail.

use std::fmt;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::Duration;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_ai::{
    BoxFuture, ChatResponse, Content, Privacy, RepairError, RepairOutcome, ToolError, ToolOutput,
    Verdict,
};
use zirv_flow_connectors::manifest;
use zirv_flow_core::mapping::Mapping;
use zirv_flow_core::token::sha256;
use zirv_flow_core::{UserId, WorkspaceId};
use zirv_flow_store::{
    AiDraft, CatalogDetails, DraftKind, DraftStatus, Flow, FlowPatch, FlowState, NewAiDraft,
    PgConnection, Segment, SegmentVersion, Source, Stream, Template, TemplateDraft,
};
use zirv_flow_templates::{
    FieldCatalog, Issue, Mode, RenderContext, TemplateDocument, content_type_of, migrate_to_current,
};

use super::diff::{
    self, Change, FLOW_KEYS, MANIFEST_KEYS, MAPPING_KEYS, SEGMENT_KEYS, TEMPLATE_KEYS,
};
use super::dispatch::{DispatchError, Dispatcher};
use super::flow_context;
use super::prompt::{self, ManifestContext, render_manifest_context};
use super::segment_context::{self, ClearKeys, Placeholders};
use super::template_context::{self, TemplateContext};
use super::tools::draft_segment::check_definition;
use super::tools::draft_template::TemplateTask;
use super::tools::{self, DraftTarget, ToolContext};
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::flows::crud::{FlowDetailBody, check_document};
use crate::flows::test_mode::simulation::{SimulationBody, simulate};
use crate::flows::{flow_error, issue_problems, parse_graph, parse_settings};
use crate::idempotency::{self, Entry, Keyed, RequestHash, lease};
use crate::imports::MappingBody;
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem, SegmentIssueProblem};
use crate::segments::catalog_of;
use crate::sources::{
    MappingIssue, PreviewBody, SampleBody, check_sample, is_push, keeps_connection,
    mapping_check_of, preview_mapping, problems, sample_manifest, saved_mapping_body,
};
use crate::templates::crud::{TemplateDetailBody, check_draft};
use crate::templates::inputs::{Inputs, Subject, resolve};
use crate::templates::preview::render;
use crate::templates::publish::{publish_inputs, publish_issues};
use crate::templates::{
    field_catalog, issue_problems as template_issue_problems, stored_document, template_error,
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
    Flow,
    Template,
}

impl DraftKindBody {
    fn store(self) -> DraftKind {
        match self {
            Self::Mapping => DraftKind::Mapping,
            Self::Manifest => DraftKind::Manifest,
            Self::Segment => DraftKind::Segment,
            Self::Flow => DraftKind::Flow,
            Self::Template => DraftKind::Template,
        }
    }
}

/// What to draft.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
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
    /// For a flow: the flow whose draft is edited. Required for a flow, refused for the others.
    #[serde(default)]
    flow_id: Option<Uuid>,
    /// For a template: the template whose draft is changed. Required for a template, refused for
    /// the others.
    #[serde(default)]
    template_id: Option<Uuid>,
    /// For a template: what to do. Required for a template, refused for the others.
    #[serde(default)]
    task: Option<TemplateTask>,
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
    if request.flow_id.is_some() && request.kind != DraftKindBody::Flow {
        return Err(invalid("flow_id", "belongs to a flow only"));
    }
    if request.template_id.is_some() && request.kind != DraftKindBody::Template {
        return Err(invalid("template_id", "belongs to a template only"));
    }
    if request.task.is_some() && request.kind != DraftKindBody::Template {
        return Err(invalid("task", "belongs to a template only"));
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
        DraftKindBody::Template => {
            if request.template_id.is_none() {
                return Err(invalid("template_id", "is required for a template"));
            }
            if request.task.is_none() {
                return Err(invalid("task", "is required for a template"));
            }
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
        DraftKindBody::Flow => {
            if request.flow_id.is_none() {
                return Err(invalid("flow_id", "is required for a flow"));
            }
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
    /// The literals of a flow's saved draft that were masked.
    Flow(flow_context::FlowTable),
    /// The literals of a template document's conditions that were masked.
    Template(Placeholders),
}

/// One problem of a draft, as a code and where it is: a column of a mapping or a JSON pointer in
/// a manifest. It never holds a value.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub(crate) struct DraftIssue {
    /// A stable code such as `unknown_column`, `unknown_list` or `path_invalid`.
    code: String,
    /// A column name or a JSON pointer; absent when the problem is on the whole draft.
    at: Option<String>,
    /// For `unknown_merge_field`: the merge field the document names, a key of the schema and
    /// never a person's value.
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
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

    /// From a tool's feedback line: `code (column "x")`, `code at /pointer` or `code`. A line may
    /// carry a hint for the model after ` | `, such as the merge field a template issue is about;
    /// the hint is part of the feedback and not of the code or the place.
    pub(crate) fn from_line(line: &str) -> Self {
        let head = line.split_once(" | ").map_or(line, |(head, _)| head);
        let (code, at) = if let Some((code, rest)) = head.split_once(" (column \"") {
            (code, rest.strip_suffix("\")").map(str::to_owned))
        } else if let Some((code, rest)) = head.split_once(" at ") {
            (code, Some(rest.to_owned()))
        } else {
            (head, None)
        };
        let field = (code == "unknown_merge_field")
            .then(|| line.split_once(" | field "))
            .flatten()
            .map(|(_, name)| name.to_owned());
        Self {
            code: code.to_owned(),
            at,
            field,
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
    /// For a flow: what a few synthetic people do in the candidate (test-mode walks, nothing
    /// executed or written). Absent for the other kinds and for an invalid draft.
    simulation: Option<SimulationBody>,
    /// For the template task `subject_variants`: the subjects offered, the first of which is in
    /// the document. Empty for every other draft.
    subject_variants: Vec<String>,
    /// For a template: the draft and the result rendered for the sample person with inert
    /// tracking, so the client can show them at desktop and mobile widths. Absent for the other
    /// kinds and for an invalid draft.
    renders: Option<TemplateRendersBody>,
    /// For a template draft that needed more than one attempt: the places in the document that an
    /// earlier attempt got wrong and a later one fixed, as JSON pointers. Absent otherwise.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    repaired: Vec<String>,
}

/// One rendered email: the parts of a preview.
#[derive(Serialize, ToSchema)]
pub(crate) struct RenderedEmailBody {
    subject: Option<String>,
    html: Option<String>,
    text: Option<String>,
}

/// A template draft before and after the change, both rendered for the sample person.
#[derive(Serialize, ToSchema)]
pub(crate) struct TemplateRendersBody {
    /// The template's draft as it is now; absent when it has blocking issues and does not render.
    before: Option<RenderedEmailBody>,
    /// The drafted document.
    after: Option<RenderedEmailBody>,
}

/// An accepted draft, for the editor to load.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct AcceptedDraftBody {
    id: Uuid,
    kind: DraftKindBody,
    source_id: Option<Uuid>,
    stream: Option<String>,
    segment_id: Option<Uuid>,
    flow_id: Option<Uuid>,
    template_id: Option<Uuid>,
    #[schema(value_type = Object)]
    content: Value,
    /// For a flow: the flow after the draft was copied into its ordinary draft. Nothing was
    /// published and no version was created.
    flow: Option<FlowDetailBody>,
    /// For a template: the template and its draft after the document was copied into the draft,
    /// with `lang` and the revision as they are now. Nothing was published or sent.
    template: Option<TemplateDetailBody>,
}

fn kind_body(kind: DraftKind) -> DraftKindBody {
    match kind {
        DraftKind::Mapping => DraftKindBody::Mapping,
        DraftKind::Manifest => DraftKindBody::Manifest,
        DraftKind::Segment => DraftKindBody::Segment,
        DraftKind::Flow => DraftKindBody::Flow,
        DraftKind::Template => DraftKindBody::Template,
    }
}

/// The one tool a run offers for a kind.
pub(crate) fn tool_name(kind: DraftKind) -> &'static str {
    match kind {
        DraftKind::Mapping => "draft_mapping",
        DraftKind::Manifest => "draft_manifest",
        DraftKind::Segment => "draft_segment",
        DraftKind::Flow => "draft_flow",
        DraftKind::Template => "draft_template",
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
/// placeholder that is not in the node it was sent in is feedback. A flow draft does the same for
/// every condition, event filter and trait value it holds.
pub(super) async fn validate_turn(
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
    if let Restore::Flow(table) = restore {
        let left = flow_context::restore_document(table, &mut restored);
        if !left.is_empty() {
            return Verdict::Invalid(DraftIssue::from_lines(&left));
        }
    }
    if let Restore::Template(table) = restore {
        let left = template_context::restore_fragment(table, &mut restored);
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

/// Adds the places of `issues` to `noted`, once each, in the order they came.
fn note_places(noted: &std::sync::Mutex<Vec<String>>, issues: &[DraftIssue]) {
    let Ok(mut noted) = noted.lock() else { return };
    for at in issues.iter().filter_map(DraftIssue::at) {
        if !noted.iter().any(|seen| seen == at) {
            noted.push(at.to_owned());
        }
    }
}

/// The places noted so far.
fn places(noted: &std::sync::Mutex<Vec<String>>) -> Vec<String> {
    noted.lock().map(|noted| noted.clone()).unwrap_or_default()
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
    flow: Option<&Flow>,
    template: Option<&Value>,
) -> Option<Value> {
    match kind {
        DraftKind::Mapping => stream
            .and_then(saved_mapping_body)
            .and_then(|body| serde_json::to_value(body).ok()),
        DraftKind::Manifest => source
            .filter(|source| source.kind == "http")
            .and_then(|source| source.config.get("manifest").cloned()),
        DraftKind::Segment => version.map(|version| version.definition.clone()),
        DraftKind::Flow => flow.map(flow_document),
        DraftKind::Template => template.cloned(),
    }
}

/// What a template draft is made from: the document and the revision it had. The digest of this
/// is what accepting checks, so a draft that was saved since, even to the same text, is outdated.
fn template_stamp(draft: &TemplateDraft) -> Value {
    json!({"revision": draft.revision, "document": draft.document})
}

/// A flow's draft as the document a flow draft holds: `{"graph", "settings"}`.
fn flow_document(flow: &Flow) -> Value {
    json!({"graph": flow.draft_graph, "settings": flow.draft_settings})
}

fn keys(kind: DraftKind) -> &'static [(&'static str, &'static str)] {
    match kind {
        DraftKind::Mapping => MAPPING_KEYS,
        DraftKind::Manifest => MANIFEST_KEYS,
        DraftKind::Segment => SEGMENT_KEYS,
        DraftKind::Flow => FLOW_KEYS,
        DraftKind::Template => TEMPLATE_KEYS,
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

/// Drafts a mapping, a manifest, a segment, a flow or a template (`kind`: `mapping`, `manifest`,
/// `segment`, `flow` or `template`; a flow draft names its flow in `flow_id` and also returns a
/// `simulation` of a few synthetic people; a template draft names its template in `template_id`
/// and its `task` (`copy`, `subject_variants`, `block_layout` or `translation`) and also returns
/// `renders`, the template's draft and the result rendered for the sample person) with the
/// assistant: the model is asked, its answer validated
/// with the rules a save applies, and problems are fed back for at most `ai.max_repair_attempts`
/// repairs. The answer is the validated draft with its diff against the saved version and a dry
/// run, or the last problems. Only a validated draft can be accepted. Editors and above who are
/// signed in (a mapping or manifest draft reads a source, so it takes an admin); an API key is
/// refused. Every attempt is a gated model call: it counts against the
/// budget and the rate limit, and with the assistant off, nothing is read or sent. A reverse
/// proxy in front needs a read timeout of at least (1 + `ai.max_repair_attempts`) x 60 s + 30 s.
///
/// Send an `Idempotency-Key` header (1 to 255 characters) to make a retry safe for 24 hours, so a
/// retry after a timeout does not spend the budget twice. While the first request runs, the same
/// key answers `409 idempotency_key_in_flight` with `Retry-After`; once it has stored a draft
/// (valid or not), the same key and body answer `409 idempotency_key_replayed` with the draft's id
/// and no body, as the draft may hold values from the workspace; the same key with another body
/// is `422 idempotency_key_reused`. The key belongs to the caller that sent it. A failed run
/// stores nothing and leaves the key free.
#[utoipa::path(
    post,
    operation_id = "create_ai_draft",
    path = "/api/v1/workspaces/{ws}/ai/drafts",
    tag = "ai",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 255 characters; makes a retry safe for 24 hours. A key first used for another request answers 422 `idempotency_key_reused`")
    ),
    request_body = DraftRequest,
    responses(
        (status = 201, description = "The draft run: a validated draft with its changes and dry run, or the problems of the last attempt", body = DraftBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 402, description = "The workspace's monthly AI budget is spent (`ai_budget_exceeded`); nothing was sent", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not an editor, not an admin for a mapping or manifest draft, or an API key (`ai_user_required`)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, source or stream, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The assistant is off (`ai_disabled`), the source does not use a manifest, or the template is archived (`template_archived`)", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is over 160 KiB", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A request that breaks a rule, naming the field, such as a missing prompt, `sample_required` or `sample_not_supported`; for a template `ai_template_too_large` (the document is over 96 KiB) or `ai_template_unsupported` (not an email template, a block layout of a code email, or, without the personal-data opt-in, a `visible_if` too deep or too large to mask)", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many AI calls (`ai_rate_limited`); try again in a minute", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The model provider failed (`ai_provider_error`), the source did, or (a template draft) the render host answered unusably (`render_protocol`)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to open the provider key, or (a template draft) the render host stopped (`render_child_exited`) or rendering is not available (`render_unavailable`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ApiJson(request): ApiJson<DraftRequest>,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, draft_need(&principal)).await?;
    let user = session_user(&principal)?;
    let kind = request.kind.store();
    // A mapping or manifest draft reads a source and sends a sample of it, which is the
    // admin's to see; the other kinds draft the editor's own work. The request is judged only
    // once the caller is.
    if matches!(kind, DraftKind::Mapping | DraftKind::Manifest) {
        authorize(&state, &principal, workspace.id, Need::Admin).await?;
    }
    check_request(&request)?;
    // A retry of a draft that already ran answers from the key and spends nothing.
    let request_hash = RequestHash::of_json(&method, &uri, &request)?;
    let keyed = Keyed::of(&headers, "create_ai_draft", &principal)?;
    let reservation = match idempotency::reserve(
        &state,
        workspace.id,
        keyed,
        request_hash,
        lease::draft(state.ai.config.max_repair_attempts),
    )
    .await?
    {
        Entry::Go(reservation) => reservation,
        Entry::Answer(response) => return Ok(response),
    };
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
                )
                .with_code("source_not_manifest"));
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
    // The flow being edited, within the workspace.
    let flow = match (kind, request.flow_id) {
        (DraftKind::Flow, Some(flow_id)) => {
            Some(current_flow(&state, workspace.id, flow_id).await?)
        }
        _ => None,
    };
    // The template being edited, within the workspace, with the document the model starts from.
    let template = match (kind, request.template_id, request.task) {
        (DraftKind::Template, Some(template_id), Some(task)) => {
            Some(current_template(&state, workspace.id, template_id, task).await?)
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
        DraftKind::Flow => DraftTarget::Flow,
        DraftKind::Template => {
            let held = template.as_ref().ok_or_else(ApiError::internal)?;
            DraftTarget::Template {
                template: Box::new(held.template.clone()),
                task: held.task,
                base: held.base.clone(),
            }
        }
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
        flow.as_ref(),
        template.as_ref().map(|held| &held.base),
    );
    // What the model may know, read by the server: through the registered tool for a source, from
    // the workspace's catalog for a segment.
    let mut placeholders = Placeholders::default();
    let mut flow_table = flow_context::FlowTable::default();
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
        (DraftKind::Flow, _) => {
            let details = segment_details(&state, workspace.id).await?;
            let current = saved.as_ref().ok_or_else(ApiError::internal)?;
            let (parts, table) = flow_context::flow_context(
                &details,
                "The current draft of the flow",
                current,
                context.privacy,
            )
            .map_err(|_| ApiError::ai_flow_too_large())?;
            flow_table = table;
            parts
        }
        (DraftKind::Template, _) => {
            let held = template.as_ref().ok_or_else(ApiError::internal)?;
            let details = segment_details(&state, workspace.id).await?;
            let (parts, table) =
                template_known(&state, workspace.id, held, &details, context.privacy).await?;
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
        (DraftKind::Flow, _) => Restore::Flow(flow_table),
        (DraftKind::Template, _) => Restore::Template(placeholders),
        _ => Restore::Nothing,
    });
    // The places of a template that earlier attempts got wrong, for the ones that fixed them.
    let earlier: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let noted = Arc::clone(&earlier);
    let outcome = zirv_flow_ai::repair(
        state.ai.config.max_repair_attempts,
        &chat,
        &dispatcher,
        move |response| {
            let verdict = turn(Arc::clone(&owned), kind, Arc::clone(&restorable), response);
            let noted = Arc::clone(&noted);
            let track = kind == DraftKind::Template;
            Box::pin(async move {
                let verdict = verdict.await;
                if let (true, Verdict::Invalid(issues)) = (track, &verdict) {
                    note_places(&noted, issues);
                }
                verdict
            }) as BoxFuture<'_, Verdict<Value, DraftIssue, ApiError>>
        },
    )
    .await;

    let now = zirv_flow_store::database_now(&state.pool).await?;
    let expires_at = now + Duration::days(DRAFT_DAYS);
    let (source_id, stream_name) = (request.source_id, request.stream.as_deref());
    let segment_id = request.segment_id;
    let flow_id = request.flow_id;
    let template_id = request.template_id;
    let body = match outcome {
        Ok(RepairOutcome::Valid(content, attempts)) => {
            // A template answer carries the document and the subjects it offered; the draft that
            // is stored and diffed is the document.
            let (content, subject_variants) = if kind == DraftKind::Template {
                split_template_answer(&content)?
            } else {
                (content, Vec::new())
            };
            let changes = diff::changes(saved.as_ref(), &content, keys(kind));
            // Both renders are made before the draft is stored, so a failure leaves no draft.
            let renders = match (&template, kind) {
                (Some(held), DraftKind::Template) => {
                    Some(template_renders(&state, workspace.id, &held.base, &content).await?)
                }
                _ => None,
            };
            // A flow has the walks of a few synthetic people: test mode, nothing written. They are
            // computed before the draft is stored, so a failure leaves no draft behind.
            let simulation = match (kind, flow_id) {
                (DraftKind::Flow, Some(flow_id)) => Some(
                    simulation_of(&state, &workspace.timezone, workspace.id, flow_id, &content)
                        .await?,
                ),
                _ => None,
            };
            // A segment has no dry run on the server: the builder shows the live count of the draft.
            let dry_run = match (kind, &stream, source_id) {
                (DraftKind::Segment | DraftKind::Flow, _, _) => None,
                (_, Some(stream), Some(source_id)) => {
                    Some(dry_run_mapping(&context, source_id, stream, &content).await)
                }
                _ => Some(dry_run_manifest(&context, source.as_ref(), &content).await),
            };
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
                    flow_id,
                    template_id,
                    status: DraftStatus::Valid,
                    content: Some(&content),
                    base_digest: match &template {
                        Some(held) => Some(digest(&template_stamp(&held.draft))),
                        None => saved.as_ref().map(digest),
                    },
                    expires_at,
                },
            )
            .await?;
            let repaired = if attempts > 1 {
                places(&earlier)
            } else {
                Vec::new()
            };
            let body = DraftBody {
                id,
                kind: request.kind,
                status: "valid",
                attempts,
                content: Some(content),
                issues: Vec::new(),
                changes,
                dry_run,
                simulation,
                subject_variants,
                renders,
                repaired,
            };
            idempotency::complete(
                reservation.as_ref(),
                &mut tx,
                StatusCode::CREATED,
                id,
                &body,
            )
            .await?;
            zirv_flow_store::commit(tx).await?;
            run_outcome(kind, "valid");
            body
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
                    flow_id,
                    template_id,
                    status: DraftStatus::Invalid,
                    content: None,
                    base_digest: None,
                    expires_at,
                },
            )
            .await?;
            let body = DraftBody {
                id,
                kind: request.kind,
                status: "invalid",
                attempts,
                content: None,
                issues,
                changes: Vec::new(),
                dry_run: None,
                simulation: None,
                subject_variants: Vec::new(),
                renders: None,
                repaired: Vec::new(),
            };
            idempotency::complete(
                reservation.as_ref(),
                &mut tx,
                StatusCode::CREATED,
                id,
                &body,
            )
            .await?;
            zirv_flow_store::commit(tx).await?;
            run_outcome(kind, "invalid");
            body
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
    idempotency::finish(reservation);
    Ok((StatusCode::CREATED, Json(body)).into_response())
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
    if matches!(draft.kind, DraftKind::Flow | DraftKind::Template) {
        // A flow or template draft is applied, not only re-checked: see `apply_flow` and
        // `apply_template`.
        return Err(ApiError::internal());
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
        // A segment, flow or template draft was handled above.
        DraftKind::Segment | DraftKind::Flow | DraftKind::Template => {
            return Err(ApiError::internal());
        }
    };
    let saved = saved_artifact(draft.kind, stream.as_ref(), Some(&source), None, None, None);
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
        DraftKind::Segment | DraftKind::Flow | DraftKind::Template => Err(ApiError::internal()),
    }
}

/// The flow being edited, within the workspace. An archived flow takes no draft.
async fn current_flow(
    state: &AppState,
    workspace: WorkspaceId,
    flow_id: Uuid,
) -> Result<Flow, ApiError> {
    let flow = zirv_flow_store::find_flow(&state.pool, workspace, flow_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if flow.state == FlowState::Archived {
        return Err(ApiError::flow_archived());
    }
    Ok(flow)
}

/// What a few synthetic people do in the candidate flow: test-mode walks over the catalog read for
/// the call, at the database's time. Nothing is read of any person and nothing is written.
async fn simulation_of(
    state: &AppState,
    timezone: &str,
    workspace: WorkspaceId,
    flow_id: Uuid,
    content: &Value,
) -> Result<SimulationBody, ApiError> {
    let documents = |name: &str| content.get(name).cloned().ok_or_else(ApiError::internal);
    let (graph, _) = parse_graph(documents("graph")?)?;
    let (settings, _) = parse_settings(documents("settings")?)?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let (catalog, _) = catalog_of(zirv_flow_store::segment_catalog(&mut tx, workspace).await?);
    let now = zirv_flow_store::database_now(&mut *tx).await?;
    drop(tx);
    Ok(simulate(
        &graph,
        &settings,
        &catalog,
        timezone.parse().unwrap_or(chrono_tz::Tz::UTC),
        flow_id,
        now,
    ))
}

/// What a template draft run edits: the template, its draft as the run began, that draft's
/// document at the current schema (what the model starts from and what the diff is against), and
/// the task.
struct TemplateTarget {
    template: Template,
    draft: TemplateDraft,
    base: Value,
    task: TemplateTask,
}

/// The template being edited, within the workspace. An archived template takes no draft, and the
/// assistant drafts email only; the task must fit the document (a block layout is for a block
/// email).
async fn current_template(
    state: &AppState,
    workspace: WorkspaceId,
    template_id: Uuid,
    task: TemplateTask,
) -> Result<TemplateTarget, ApiError> {
    let template = zirv_flow_store::find_template(&state.pool, workspace, template_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if template.archived_at.is_some() {
        return Err(ApiError::template_archived());
    }
    if template.channel != "email" {
        return Err(ApiError::ai_template_unsupported(
            "the assistant drafts email templates only",
        ));
    }
    let draft = zirv_flow_store::find_template_draft(&state.pool, workspace, template_id)
        .await?
        .ok_or_else(ApiError::internal)?;
    let base = migrate_to_current(draft.document.clone())
        .map_err(|_| ApiError::template_unsupported_schema())?;
    if task == TemplateTask::BlockLayout && stored_document(&base)?.mode != Mode::Blocks {
        return Err(ApiError::ai_template_unsupported(
            "a block layout is for a template in blocks mode",
        ));
    }
    Ok(TemplateTarget {
        template,
        draft,
        base,
        task,
    })
}

/// What the model may know of a template: its document, the names of the merge fields, themes and
/// snippets of the workspace, and the task. No person and no value of a trait.
async fn template_known(
    state: &AppState,
    workspace: WorkspaceId,
    held: &TemplateTarget,
    details: &CatalogDetails,
    privacy: Privacy,
) -> Result<(Vec<Content>, Placeholders), ApiError> {
    let mut connection = state
        .pool
        .acquire()
        .await
        .map_err(|_| ApiError::internal())?;
    let read = zirv_flow_store::read_template_field_catalog(&mut connection, workspace).await?;
    let themes = zirv_flow_store::list_themes(&mut *connection, workspace, false).await?;
    let snippets = zirv_flow_store::list_snippets(&mut *connection, workspace, false).await?;
    drop(connection);
    template_context::render_template_context(
        &TemplateContext {
            name: &held.template.name,
            channel: &held.template.channel,
            category: &held.template.category,
            document: &held.base,
            traits: read
                .traits
                .iter()
                .map(|(key, kind)| (key.clone(), kind.as_str().to_owned()))
                .collect(),
            event_properties: read
                .event_properties
                .iter()
                .map(|(name, kind)| (name.clone(), kind.as_str().to_owned()))
                .collect(),
            themes: themes
                .into_iter()
                .map(|theme| (theme.id, theme.name))
                .collect(),
            snippets: snippets
                .into_iter()
                .map(|snippet| (snippet.id, snippet.name))
                .collect(),
            task: held.task,
        },
        &ClearKeys::of(details),
        privacy,
    )
}

/// The document and the subjects offered, from the one answer `draft_template` accepted.
fn split_template_answer(answer: &Value) -> Result<(Value, Vec<String>), ApiError> {
    let document = answer
        .get("document")
        .cloned()
        .ok_or_else(ApiError::internal)?;
    let variants = answer
        .get("subject_variants")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok((document, variants))
}

/// `doc` rendered for the sample person with inert tracking: the preview pipeline, through the
/// render pool. A document with blocking issues does not render.
async fn render_sample(
    pool: &zirv_flow_render::RenderPool,
    doc: &TemplateDocument,
    catalog: &FieldCatalog,
    context: &RenderContext,
    inputs: &Inputs,
) -> Result<Option<RenderedEmailBody>, ApiError> {
    let content = content_type_of(doc).map_err(|_| ApiError::template_content_unsupported())?;
    let mut issues: Vec<Issue> = pool
        .validate_with(doc, catalog, &inputs.extras)
        .await
        .map_err(|failure| ApiError::render_failed(&failure))?;
    issues.extend(inputs.issues.iter().cloned());
    if issues.iter().any(Issue::is_blocking) {
        return Ok(None);
    }
    let rendered = render(
        pool,
        content.render_kind(),
        doc,
        context,
        &inputs.extras,
        None,
    )
    .await?;
    Ok(Some(RenderedEmailBody {
        subject: rendered.subject,
        html: rendered.html,
        text: rendered.text,
    }))
}

/// The template's draft and the drafted document, rendered for the sample person.
async fn template_renders(
    state: &AppState,
    workspace: WorkspaceId,
    before: &Value,
    after: &Value,
) -> Result<TemplateRendersBody, ApiError> {
    let pool = state
        .render_pool()
        .ok_or_else(ApiError::render_unavailable)?;
    let (before_doc, after_doc) = (stored_document(before)?, stored_document(after)?);
    let (catalog, context, before_inputs, after_inputs) = {
        let mut connection = state
            .pool
            .acquire()
            .await
            .map_err(|_| ApiError::internal())?;
        let catalog = field_catalog(
            zirv_flow_store::read_template_field_catalog(&mut connection, workspace).await?,
        );
        let context = RenderContext::sample_person(&catalog);
        let now = state.clock.now(&mut connection).await?;
        let subject = Subject::Sample(&context);
        let before_inputs = resolve(
            &mut connection,
            workspace,
            &before_doc,
            subject,
            state.public_url(),
            now,
        )
        .await?;
        let after_inputs = resolve(
            &mut connection,
            workspace,
            &after_doc,
            subject,
            state.public_url(),
            now,
        )
        .await?;
        (catalog, context, before_inputs, after_inputs)
    };
    Ok(TemplateRendersBody {
        before: render_sample(pool, &before_doc, &catalog, &context, &before_inputs).await?,
        after: render_sample(pool, &after_doc, &catalog, &context, &after_inputs).await?,
    })
}

/// Checks a template draft's document against the publish validation as it is now, with no lock
/// and no connection held, because the check waits on the render host. A blocking issue is the
/// publish's `template_invalid` with the issues.
async fn recheck_template(
    state: &AppState,
    draft: &AiDraft,
    content: &Value,
) -> Result<(), ApiError> {
    let doc = stored_document(content)?;
    let sender = doc
        .email
        .as_ref()
        .and_then(|email| email.sender.as_deref())
        .or_else(|| {
            doc.blocks
                .as_ref()
                .and_then(|blocks| blocks.sender.as_deref())
        });
    if let Some(sender) = sender {
        let unknown =
            || ApiError::sender_unverified("the sender is not a sender identity of this workspace");
        let id = sender.trim().parse::<Uuid>().map_err(|_| unknown())?;
        zirv_flow_store::find_sender_identity(&state.pool, draft.workspace_id, id)
            .await?
            .ok_or_else(unknown)?;
    }
    let render = state
        .render_pool()
        .ok_or_else(ApiError::render_unavailable)?;
    let (catalog, inputs) = {
        let mut connection = state
            .pool
            .acquire()
            .await
            .map_err(|_| ApiError::internal())?;
        publish_inputs(state, &mut connection, draft.workspace_id, &doc).await?
    };
    let issues = publish_issues(render, &doc, &catalog, &inputs).await?;
    if issues.iter().any(Issue::is_blocking) {
        return Err(ApiError::template_invalid(template_issue_problems(&issues)));
    }
    Ok(())
}

/// Copies a template draft's document into the template's ordinary draft, in the transaction that
/// marks it accepted.
///
/// The template is locked first, as a draft save or a publish locks it: the template's draft is
/// the revision the assistant's draft was made from (else 409 `template_draft_changed`), the
/// template is not archived, and the document is one of the template's channel and category.
/// Only then is the draft replaced, the same write as `PUT .../templates/{id}/draft`, which sets
/// `lang` with the rest of the document. No version is created and nothing is published; the
/// published version, the flows and the sends are not touched.
async fn apply_template(
    tx: &mut PgConnection,
    principal: &Principal,
    draft: &AiDraft,
    content: &Value,
) -> Result<(Template, TemplateDraft), ApiError> {
    let template_id = draft.template_id.ok_or_else(ApiError::internal)?;
    let workspace = draft.workspace_id;
    let locked = zirv_flow_store::lock_template(tx, workspace, template_id)
        .await
        .map_err(template_error)?;
    if Some(digest(&template_stamp(locked.draft()))) != draft.base_digest {
        return Err(ApiError::template_draft_changed_since_assistant_draft());
    }
    let document = check_draft(locked.template(), content)?;
    let saved = zirv_flow_store::save_template_draft(
        tx,
        workspace,
        template_id,
        &document,
        principal.user_id(),
    )
    .await
    .map_err(template_error)?;
    admin::audit(
        tx,
        &principal.context(),
        Some(workspace),
        "template.draft_saved",
        Some(("template", template_id)),
        json!({
            "revision": saved.revision,
            "via": "ai_draft",
        }),
    )
    .await?;
    Ok((locked.template().clone(), saved))
}

/// Copies a flow draft into the flow's ordinary draft, in the transaction that marks it accepted.
///
/// The flow is locked first, as a save or a publish locks it: the flow's draft is unchanged since
/// the assistant's draft was made (else `ai_draft_outdated`), the flow is not archived, and the
/// content passes the publish validation again against the catalog as it is now (else 422
/// `flow_invalid` with the issues). Only then the graph and settings of the flow's draft are
/// replaced, the same write as a `PATCH` of the flow. No version is created and nothing is
/// published; the flow's state does not change.
async fn apply_flow(
    state: &AppState,
    tx: &mut PgConnection,
    principal: &Principal,
    draft: &AiDraft,
    content: &Value,
) -> Result<Flow, ApiError> {
    let flow_id = draft.flow_id.ok_or_else(ApiError::internal)?;
    let workspace = draft.workspace_id;
    let locked = zirv_flow_store::lock_flow(tx, workspace, flow_id)
        .await
        .map_err(flow_error)?;
    if locked.flow().state == FlowState::Archived {
        return Err(ApiError::flow_archived());
    }
    if Some(digest(&flow_document(locked.flow()))) != draft.base_digest {
        return Err(ApiError::ai_draft_outdated());
    }
    let documents = |name: &str| content.get(name).cloned().ok_or_else(ApiError::internal);
    let (graph, stored_graph) = parse_graph(documents("graph")?)?;
    let (settings, stored_settings) = parse_settings(documents("settings")?)?;
    let (checked, _) = check_document(state, tx, workspace, &graph, &settings).await?;
    if !checked.issues.is_empty() {
        return Err(ApiError::flow_invalid(issue_problems(&checked.issues)));
    }
    let patch = FlowPatch {
        name: None,
        graph: Some(stored_graph),
        settings: Some(stored_settings),
    };
    let flow = zirv_flow_store::update_flow_draft(tx, workspace, flow_id, &patch)
        .await
        .map_err(flow_error)?;
    admin::audit(
        tx,
        &principal.context(),
        Some(workspace),
        "flow.updated",
        Some(("flow", flow.id)),
        json!({ "fields": ["graph", "settings"], "via": "ai_draft" }),
    )
    .await?;
    Ok(flow)
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
        Some(segment_id) => Some(
            current_version(tx, workspace, segment_id)
                .await?
                .1
                .definition,
        ),
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

/// A segment of the workspace and its current version. A segment that is gone or archived is
/// refused as a save would refuse it.
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
        DraftKind::Flow => json!({"kind": draft.kind.as_str(), "flow_id": draft.flow_id}),
        DraftKind::Template => {
            json!({"kind": draft.kind.as_str(), "template_id": draft.template_id})
        }
        DraftKind::Mapping | DraftKind::Manifest => json!({
            "kind": draft.kind.as_str(),
            "source_id": draft.source_id,
            "has_stream": draft.stream_name.is_some(),
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
/// stays a separate action. Audited as `ai.draft.accepted`. Editors and above who are signed in.
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
        (status = 403, description = "Not an editor, not an admin for a mapping or manifest draft, or an API key (`ai_user_required`)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such draft of this user in this workspace, or it expired", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The draft never validated (`ai_draft_invalid`), the saved version changed since (`ai_draft_outdated`), the flow of a flow draft is archived (`flow_archived`), the template of a template draft is archived (`template_archived`), or its draft changed or was replaced since this draft was made (`template_draft_changed`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The draft no longer passes the rules a save applies, such as a trait archived since (the same codes as a save; for a flow draft `flow_invalid` with the publish validation issues; for a template draft `template_invalid` with the publish validation issues)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn accept(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<AcceptedDraftBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, draft_need(&principal)).await?;
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

    if draft.kind == DraftKind::Template {
        recheck_template(&state, &draft, &content).await?;
    }
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let (flow, template) = match draft.kind {
        DraftKind::Flow => (
            Some(apply_flow(&state, &mut tx, &principal, &draft, &content).await?),
            None,
        ),
        DraftKind::Template => (
            None,
            Some(apply_template(&mut tx, &principal, &draft, &content).await?),
        ),
        _ => {
            recheck(&state, &mut tx, &draft, &content).await?;
            (None, None)
        }
    };
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
        flow_id: draft.flow_id,
        template_id: draft.template_id,
        content,
        flow: flow.as_ref().map(FlowDetailBody::from),
        template: template
            .as_ref()
            .map(|(template, saved)| TemplateDetailBody::new(template, saved)),
    }))
}

/// An API key never drafts: it is refused with `ai_user_required` once it passes the scope
/// check, which stays the admin scope so that answer does not change. A session needs Edit.
fn draft_need(principal: &Principal) -> Need {
    match principal {
        Principal::Key(_) => Need::Admin,
        Principal::Session(_) => Need::Edit,
    }
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
        // Only the code and the place reach the response, and for a merge field its name.
        assert_eq!(
            serde_json::to_value(&column).unwrap(),
            json!({"code": "unknown_column", "at": "Ghost"})
        );
    }

    #[test]
    fn a_hint_after_the_bar_is_feedback_and_not_the_code_or_the_place() {
        let issue = DraftIssue::from_line("unknown_merge_field at /email/html | field person.nope");

        assert_eq!(
            (issue.code(), issue.at()),
            ("unknown_merge_field", Some("/email/html"))
        );
        assert_eq!(
            issue.to_string(),
            "unknown_merge_field at /email/html | field person.nope"
        );
        assert_eq!(
            serde_json::to_value(&issue).unwrap(),
            json!({"code": "unknown_merge_field", "at": "/email/html", "field": "person.nope"})
        );
        let other = DraftIssue::from_line("unknown_column (column \"x\") | field y");
        assert_eq!(
            serde_json::to_value(&other).unwrap(),
            json!({"code": "unknown_column", "at": "x"})
        );
    }

    #[test]
    fn a_template_request_names_its_template_and_task_and_nothing_of_another_kind() {
        let template = Uuid::now_v7();

        assert_eq!(
            refused(json!({"kind": "template", "prompt": "x", "task": "copy"})),
            "template_id is required for a template"
        );
        assert_eq!(
            refused(json!({"kind": "template", "prompt": "x", "template_id": template})),
            "task is required for a template"
        );
        assert_eq!(
            refused(
                json!({"kind": "template", "prompt": "x", "template_id": template,
                    "task": "copy", "documentation": "docs"})
            ),
            "documentation belongs to a manifest only"
        );
        assert_eq!(
            refused(json!({"kind": "flow", "prompt": "x", "flow_id": template, "task": "copy"})),
            "task belongs to a template only"
        );
        assert_eq!(
            refused(json!({"kind": "segment", "prompt": "x", "template_id": template})),
            "template_id belongs to a template only"
        );
        for task in ["copy", "subject_variants", "block_layout", "translation"] {
            assert!(
                check_request(&request(json!({"kind": "template", "prompt": "x",
                    "template_id": template, "task": task})))
                .is_ok(),
                "{task}"
            );
        }
        // A task the assistant does not have is refused outright.
        assert!(
            serde_json::from_value::<DraftRequest>(json!({"kind": "template", "prompt": "x",
                "template_id": template, "task": "publish"}))
            .is_err()
        );
    }

    #[test]
    fn a_template_stamp_follows_the_revision_as_well_as_the_document() {
        let draft = |revision| TemplateDraft {
            template_id: Uuid::nil(),
            document: json!({"email": {"subject": "Hi"}}),
            revision,
            updated_by: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };

        assert_eq!(
            digest(&template_stamp(&draft(1))),
            digest(&template_stamp(&draft(1)))
        );
        assert_ne!(
            digest(&template_stamp(&draft(1))),
            digest(&template_stamp(&draft(2)))
        );
    }

    #[test]
    fn a_template_answer_splits_into_the_document_and_the_subjects() {
        let answer = json!({"document": {"email": {}}, "subject_variants": ["a", "b"]});

        let (document, variants) = split_template_answer(&answer).unwrap();

        assert_eq!(document, json!({"email": {}}));
        assert_eq!(variants, ["a", "b"]);
        assert_eq!(
            split_template_answer(&json!({"document": {}})).unwrap().1,
            Vec::<String>::new()
        );
        assert!(split_template_answer(&json!({})).is_err());
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
        assert_eq!(tool_name(DraftKind::Template), "draft_template");
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
