//! The golden-prompt harness (architecture 13.5): a corpus of prompts with the properties their
//! drafts should have, run through the same prompt builder, repair loop and pure validators as a
//! draft run, with a stand-in for the model.
//!
//! CI never calls a model. Each case carries a `replay`, the tool-call arguments a good model
//! gave, and CI answers with them through the fake provider. An operator runs the same cases
//! against the workspace's configured model with `zirv-flow admin evaluate-assistant`, through
//! the gated dispatcher: every prompt is a gated call that counts against the budget and the
//! rate limit. Scores are for the operator to read; they gate nothing.
//!
//! The expected properties are JSON pointer checks on the drafted artifact. A `*` segment stands
//! for any element of an array or value of an object: `equals` and `contains` hold when any match
//! satisfies them, and `absent` holds when nothing matches or what matches is null.

use std::sync::Mutex;

use hashbrown::{HashMap, HashSet};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use zirv_flow_ai::{
    AiError, BoxFuture, ChatRequest, ChatResponse, Privacy, RepairError, RepairOutcome, Sender,
    Verdict,
};
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::mapping::{Header, TraitState};
use zirv_flow_core::people::TraitType;
use zirv_flow_store::DraftKind;

use super::dispatch::{DispatchError, Dispatcher};
use super::drafts::{DraftIssue, expected_call, tool_name, wrong_answer};
use super::prompt::{
    ManifestContext, StreamContext, TraitInfo, build_request, render_manifest_context,
    render_stream_context,
};
use super::tools::{self, draft_manifest, draft_mapping};
use crate::AppState;
use crate::problem::ApiError;
use crate::sources::MappingCheck;

/// The corpus, compiled in.
const CORPUS: &str = include_str!("golden.json");

#[derive(Debug, Deserialize)]
struct Corpus {
    cases: Vec<GoldenCase>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CaseKind {
    Mapping,
    Manifest,
}

impl CaseKind {
    fn store(self) -> DraftKind {
        match self {
            Self::Mapping => DraftKind::Mapping,
            Self::Manifest => DraftKind::Manifest,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoldenField {
    name: String,
    #[serde(rename = "type")]
    field_type: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoldenTrait {
    key: String,
    #[serde(rename = "type")]
    trait_type: TraitType,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoldenList {
    id: Uuid,
    name: String,
}

/// One property the draft should have.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expect {
    /// A JSON pointer into the draft; `*` matches any element or value.
    path: String,
    /// The value at the path equals this.
    #[serde(default)]
    equals: Option<Value>,
    /// The array at the path holds this element, the string holds this text, or the object has
    /// this key.
    #[serde(default)]
    contains: Option<Value>,
    /// Nothing, or only null, is at the path.
    #[serde(default)]
    absent: bool,
}

impl Expect {
    /// The check as a name for a report: the path and the operation.
    fn label(&self) -> String {
        let operation = if self.absent {
            "absent".to_owned()
        } else if let Some(value) = &self.equals {
            format!("equals {value}")
        } else if let Some(value) = &self.contains {
            format!("contains {value}")
        } else {
            "unchecked".to_owned()
        };
        format!("{} {operation}", self.path)
    }

    fn holds(&self, draft: &Value) -> bool {
        let found = resolve(draft, &self.path);
        if self.absent {
            return found.iter().all(|value| value.is_null());
        }
        if let Some(wanted) = &self.equals {
            return found.contains(&wanted);
        }
        if let Some(wanted) = &self.contains {
            return found.iter().any(|value| match value {
                Value::Array(items) => items.contains(wanted),
                Value::String(text) => wanted.as_str().is_some_and(|part| text.contains(part)),
                Value::Object(map) => wanted.as_str().is_some_and(|key| map.contains_key(key)),
                _ => false,
            });
        }
        false
    }
}

/// The values a pointer with `*` segments reaches.
fn resolve<'v>(root: &'v Value, path: &str) -> Vec<&'v Value> {
    let mut current = vec![root];
    for segment in path.split('/').skip(1) {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        let mut next = Vec::new();
        for value in current {
            match (segment.as_str(), value) {
                ("*", Value::Array(items)) => next.extend(items),
                ("*", Value::Object(map)) => next.extend(map.values()),
                (_, Value::Array(items)) => {
                    if let Some(item) = segment.parse::<usize>().ok().and_then(|n| items.get(n)) {
                        next.push(item);
                    }
                }
                (_, Value::Object(map)) => next.extend(map.get(&segment)),
                _ => {}
            }
        }
        current = next;
    }
    current
}

/// A case of the corpus.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoldenCase {
    pub(crate) name: String,
    kind: CaseKind,
    prompt: String,
    #[serde(default)]
    documentation: Option<String>,
    /// The stream's fields, for a mapping.
    #[serde(default)]
    fields: Vec<GoldenField>,
    #[serde(default)]
    primary_key: Vec<String>,
    #[serde(default)]
    traits: Vec<GoldenTrait>,
    #[serde(default)]
    lists: Vec<GoldenList>,
    expect: Vec<Expect>,
    /// What a good model answered, as the arguments of its tool call, in order. CI answers with
    /// these; an operator's run does not use them.
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the CI tests"))]
    replay: Vec<Value>,
}

impl GoldenCase {
    fn stream_context(&self) -> StreamContext {
        StreamContext {
            fields: self
                .fields
                .iter()
                .map(|field| (field.name.clone(), field.field_type.clone()))
                .collect(),
            sync_mode: None,
            primary_key: self.primary_key.clone(),
            saved_mapping: None,
            traits: self
                .traits
                .iter()
                .map(|entry| TraitInfo {
                    key: entry.key.clone(),
                    trait_type: entry.trait_type.as_str().to_owned(),
                    description: entry.description.clone(),
                })
                .collect(),
            lists: self
                .lists
                .iter()
                .map(|list| (list.id, list.name.clone()))
                .collect(),
            samples: Vec::new(),
        }
    }

    /// What a mapping draft is checked against, built from the case alone.
    fn mapping_check(&self) -> Result<MappingCheck, String> {
        let header = Header::new(self.fields.iter().map(|field| field.name.clone()).collect())
            .map_err(|error| format!("fields: {}", error.code()))?;
        let catalog: HashMap<String, TraitState> = self
            .traits
            .iter()
            .map(|entry| (entry.key.clone(), TraitState::Active(entry.trait_type)))
            .collect();
        let lists: HashSet<Uuid> = self.lists.iter().map(|list| list.id).collect();
        Ok(MappingCheck::new(
            header,
            catalog,
            lists,
            self.primary_key.clone(),
        ))
    }
}

/// The corpus, parsed.
pub(crate) fn corpus() -> Result<Vec<GoldenCase>, String> {
    serde_json::from_str::<Corpus>(CORPUS)
        .map(|corpus| corpus.cases)
        .map_err(|error| format!("the golden corpus is invalid: {error}"))
}

/// How one case went.
#[derive(Debug, Serialize)]
pub struct CaseReport {
    name: String,
    kind: &'static str,
    /// Whether a draft passed validation.
    valid: bool,
    /// How many times the model was asked.
    attempts: u32,
    /// The properties that held, and the ones that did not.
    passed: Vec<String>,
    failed: Vec<String>,
    input_tokens: u32,
    output_tokens: u32,
    /// What the case cost on the workspace's model, in micro-units; absent in CI.
    cost_micros: Option<i64>,
    /// The model that answered; absent in CI.
    model: Option<String>,
    /// Why the case could not be run: a problem code such as `ai_budget_exceeded`.
    error: Option<String>,
}

/// Why the corpus could not be run: the assistant is off, the case is unknown, or a gate refused
/// it. A low score is never an error.
#[derive(Debug)]
pub struct EvaluationError(String);

impl std::fmt::Display for EvaluationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EvaluationError {}

impl From<ApiError> for EvaluationError {
    fn from(error: ApiError) -> Self {
        Self(error.detail().to_owned())
    }
}

impl CaseReport {
    /// The report as one line of JSON.
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// A sender that adds up what the provider reports.
struct Metered<'a> {
    inner: &'a dyn Sender,
    seen: Mutex<Seen>,
}

#[derive(Default)]
struct Seen {
    input_tokens: u32,
    output_tokens: u32,
    model: Option<String>,
}

impl Sender for Metered<'_> {
    fn send<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, AiError>> {
        Box::pin(async move {
            let response = self.inner.send(request).await?;
            let mut seen = self
                .seen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            seen.input_tokens = seen
                .input_tokens
                .saturating_add(response.usage().input_tokens);
            seen.output_tokens = seen
                .output_tokens
                .saturating_add(response.usage().output_tokens);
            seen.model = Some(response.model().to_owned());
            Ok(response)
        })
    }
}

/// Grades one answer with the pure validators of the draft tools.
fn grade(
    case: &GoldenCase,
    check: Option<&MappingCheck>,
    response: &ChatResponse,
) -> Verdict<Value, DraftIssue, ()> {
    let want = tool_name(case.kind.store());
    let Some(arguments) = expected_call(response, want) else {
        return Verdict::Invalid(vec![wrong_answer(want)]);
    };
    let checked = match (case.kind, check) {
        (CaseKind::Mapping, Some(check)) => draft_mapping::check_draft(check, arguments),
        (CaseKind::Manifest, _) => draft_manifest::check_draft(arguments),
        (CaseKind::Mapping, None) => Err(tools::arguments_invalid_lines()),
    };
    match checked {
        Ok(draft) => Verdict::Valid(draft),
        Err(lines) => Verdict::Invalid(DraftIssue::from_lines(&lines)),
    }
}

/// Runs one case: builds the request as a draft run does, asks `sender`, repairs up to
/// `max_repairs` times and checks the properties.
pub(crate) async fn evaluate_case(
    sender: &dyn Sender,
    case: &GoldenCase,
    max_repairs: u32,
) -> CaseReport {
    let mut report = CaseReport {
        name: case.name.clone(),
        kind: case.kind.store().as_str(),
        valid: false,
        attempts: 0,
        passed: Vec::new(),
        failed: case.expect.iter().map(Expect::label).collect(),
        input_tokens: 0,
        output_tokens: 0,
        cost_micros: None,
        model: None,
        error: None,
    };
    let kind = case.kind.store();
    let check = match case.kind {
        CaseKind::Mapping => match case.mapping_check() {
            Ok(check) => Some(check),
            Err(reason) => {
                report.error = Some(format!("case_invalid: {reason}"));
                return report;
            }
        },
        CaseKind::Manifest => None,
    };
    let context = match case.kind {
        CaseKind::Mapping => render_stream_context(&case.stream_context(), Privacy::SchemaOnly),
        CaseKind::Manifest => render_manifest_context(&ManifestContext::default()),
    };
    let request = tools::find(tool_name(kind))
        .ok_or_else(|| "the draft tool is not registered".to_owned())
        .and_then(|tool| tool.spec().map_err(|error| error.to_string()))
        .and_then(|spec| {
            build_request(
                kind,
                Privacy::SchemaOnly,
                context,
                &case.prompt,
                case.documentation.as_deref(),
                spec,
            )
            .map_err(|error| error.to_string())
        });
    let request = match request {
        Ok(request) => request,
        Err(reason) => {
            report.error = Some(format!("case_invalid: {reason}"));
            return report;
        }
    };

    let metered = Metered {
        inner: sender,
        seen: Mutex::default(),
    };
    let outcome = zirv_flow_ai::repair(max_repairs, &request, &metered, |response| {
        let verdict = grade(case, check.as_ref(), response);
        Box::pin(async move { verdict })
    })
    .await;
    match outcome {
        Ok(RepairOutcome::Valid(draft, attempts)) => {
            report.valid = true;
            report.attempts = attempts;
            let (passed, failed): (Vec<_>, Vec<_>) =
                case.expect.iter().partition(|expect| expect.holds(&draft));
            report.passed = passed.iter().map(|expect| expect.label()).collect();
            report.failed = failed.iter().map(|expect| expect.label()).collect();
        }
        Ok(RepairOutcome::Exhausted(_, attempts)) => report.attempts = attempts,
        Err(RepairError::Dispatch(error)) => {
            report.error = Some(problem_code(error));
        }
        Err(RepairError::Aborted(())) => report.error = Some("aborted".to_owned()),
    }
    let seen = metered
        .seen
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    report.input_tokens = seen.input_tokens;
    report.output_tokens = seen.output_tokens;
    report.model = seen.model;
    report
}

/// The problem code a dispatch error answers with.
fn problem_code(error: AiError) -> String {
    let problem = ApiError::from(DispatchError::Ai(error));
    problem
        .code()
        .map_or_else(|| problem.status_code().as_u16().to_string(), str::to_owned)
}

/// Runs the corpus, or one case of it, against the workspace's configured model through the
/// gated dispatcher (feature `evaluation`): each prompt counts against the budget and the rate
/// limit, and an assistant that is off is refused. Scores are reported, never enforced.
pub async fn evaluate_assistant(
    state: &AppState,
    workspace: WorkspaceId,
    case: Option<&str>,
) -> Result<Vec<CaseReport>, EvaluationError> {
    let mut cases = corpus().map_err(EvaluationError)?;
    if let Some(name) = case {
        cases.retain(|candidate| candidate.name == name);
        if cases.is_empty() {
            return Err(EvaluationError("no such case".to_owned()));
        }
    }
    let dispatcher = Dispatcher::open(state, None, workspace, "evaluation")
        .await
        .map_err(|error| EvaluationError::from(ApiError::from(error)))?;
    let mut reports = Vec::with_capacity(cases.len());
    for case in &cases {
        let mut report =
            evaluate_case(&dispatcher, case, state.ai.config.max_repair_attempts).await;
        report.cost_micros =
            Some(dispatcher.cost_micros(report.input_tokens, report.output_tokens));
        reports.push(report);
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_ai::fake::{FakeProvider, tool_call_response};
    use zirv_flow_ai::{AiHttp, AiHttpLimits, Endpoint, ProviderSender};
    use zirv_flow_core::SecretString;
    use zirv_flow_core::egress::EgressPolicy;

    use super::*;

    /// Runs a case on the fake provider, which answers with the case's replay.
    async fn replayed(case: &GoldenCase) -> (CaseReport, FakeProvider) {
        let fake = FakeProvider::new();
        let tool = tool_name(case.kind.store());
        for arguments in &case.replay {
            fake.push(Ok(tool_call_response(tool, arguments.clone(), 100, 50)));
        }
        let http = AiHttp::new(
            std::sync::Arc::new(EgressPolicy::default()),
            AiHttpLimits::default(),
        )
        .unwrap();
        let endpoint = Endpoint {
            base_url: "http://unused.test".into(),
            model: "m".into(),
        };
        let key = SecretString::new("k".into());
        let report = {
            let sender = ProviderSender {
                provider: &fake,
                http: &http,
                endpoint: &endpoint,
                key: &key,
            };
            evaluate_case(&sender, case, 2).await
        };
        (report, fake)
    }

    #[tokio::test]
    async fn the_golden_corpus_passes_on_replayed_tool_calls() {
        let cases = corpus().unwrap();
        assert!(cases.len() >= 5);

        for case in &cases {
            let (report, fake) = replayed(case).await;
            assert_eq!(report.error, None, "{}", case.name);
            assert!(report.valid, "{} did not validate", case.name);
            assert_eq!(report.failed, Vec::<String>::new(), "{}", case.name);
            assert_eq!(report.passed.len(), case.expect.len(), "{}", case.name);
            assert_eq!(fake.call_count(), case.replay.len(), "{}", case.name);
            assert_eq!(
                (report.input_tokens, report.output_tokens),
                (
                    100 * u32::try_from(case.replay.len()).unwrap(),
                    50 * u32::try_from(case.replay.len()).unwrap()
                ),
                "{}",
                case.name
            );
        }
        let kinds: Vec<&str> = cases
            .iter()
            .map(|case| case.kind.store().as_str())
            .collect();
        assert!(kinds.iter().filter(|kind| **kind == "mapping").count() >= 2);
        assert!(kinds.iter().filter(|kind| **kind == "manifest").count() >= 2);
    }

    #[tokio::test]
    async fn each_kind_has_a_case_that_repairs() {
        let mut repairing = Vec::new();
        for case in &corpus().unwrap() {
            let (report, _) = replayed(case).await;
            if report.attempts == 2 {
                repairing.push(case.kind);
            }
        }
        assert!(repairing.contains(&CaseKind::Mapping));
        assert!(repairing.contains(&CaseKind::Manifest));
    }

    #[tokio::test]
    async fn a_failed_property_is_named_in_the_report() {
        let mut cases = corpus().unwrap();
        let mut case = cases.remove(0);
        case.expect.push(Expect {
            path: "/columns/*/target/key".into(),
            equals: Some(json!("not_a_trait")),
            contains: None,
            absent: false,
        });

        let (report, _) = replayed(&case).await;

        assert!(report.valid);
        assert_eq!(
            report.failed,
            ["/columns/*/target/key equals \"not_a_trait\""]
        );
        assert_eq!(report.passed.len(), case.expect.len() - 1);
        // The report is a line of JSON with the counts, and no draft in it.
        let line = serde_json::to_string(&report).unwrap();
        assert!(line.contains("\"failed\""), "{line}");
        assert!(!line.contains("Email\"}"), "{line}");
    }

    #[tokio::test]
    async fn a_model_that_never_validates_exhausts_the_repairs_and_fails_every_property() {
        let cases = corpus().unwrap();
        let case = &cases[0];
        let fake = FakeProvider::new();
        for _ in 0..3 {
            fake.push(Ok(tool_call_response(
                "draft_mapping",
                json!({"columns": []}),
                10,
                10,
            )));
        }
        let http = AiHttp::new(
            std::sync::Arc::new(EgressPolicy::default()),
            AiHttpLimits::default(),
        )
        .unwrap();
        let endpoint = Endpoint {
            base_url: "http://unused.test".into(),
            model: "m".into(),
        };
        let key = SecretString::new("k".into());
        let sender = ProviderSender {
            provider: &fake,
            http: &http,
            endpoint: &endpoint,
            key: &key,
        };

        let report = evaluate_case(&sender, case, 2).await;

        assert!(!report.valid);
        assert_eq!(report.attempts, 3);
        assert_eq!(report.failed.len(), case.expect.len());
        assert!(report.passed.is_empty());
    }

    #[test]
    fn pointers_expand_wildcards_and_the_three_checks_read_them() {
        let draft = json!({"columns": [{"column": "a", "n": ["x", "y"]}, {"column": "b"}],
            "name": "order_placed", "tags": {"k": 1}});
        let check = |path: &str, equals: Option<Value>, contains: Option<Value>, absent| Expect {
            path: path.into(),
            equals,
            contains,
            absent,
        };

        assert!(check("/columns/*/column", Some(json!("b")), None, false).holds(&draft));
        assert!(check("/columns/1/column", Some(json!("b")), None, false).holds(&draft));
        assert!(!check("/columns/0/column", Some(json!("b")), None, false).holds(&draft));
        assert!(check("/columns/*/n", None, Some(json!("y")), false).holds(&draft));
        assert!(check("/name", None, Some(json!("placed")), false).holds(&draft));
        assert!(check("/tags", None, Some(json!("k")), false).holds(&draft));
        assert!(check("/event", None, None, true).holds(&draft));
        assert!(!check("/name", None, None, true).holds(&draft));
        // A check with no operation holds for nothing.
        assert!(!check("/name", None, None, false).holds(&draft));
    }

    #[test]
    fn every_case_states_its_properties_and_replays_something() {
        for case in corpus().unwrap() {
            assert!(!case.expect.is_empty(), "{}", case.name);
            assert!(!case.replay.is_empty(), "{}", case.name);
            for expect in &case.expect {
                let operations = usize::from(expect.equals.is_some())
                    + usize::from(expect.contains.is_some())
                    + usize::from(expect.absent);
                assert_eq!(operations, 1, "{} {}", case.name, expect.path);
            }
        }
    }
}
