use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

use crate::{request_id, trace};

/// An RFC 9457 problem document.
#[derive(Debug, Serialize, ToSchema)]
pub struct Problem {
    /// Problem type URI; `about:blank` until specific types are defined.
    #[serde(rename = "type")]
    #[schema(rename = "type", example = "about:blank")]
    pub kind: String,
    pub title: String,
    pub status: u16,
    pub detail: String,
    /// The `x-request-id` of the failed request.
    pub request_id: Option<String>,
    /// The `x-trace-id` of the failed request; look it up with
    /// `GET /api/v1/workspaces/{ws}/traces/{trace_id}`.
    pub trace_id: Option<String>,
    /// The invalid fields of an ingestion request: field names and reason codes, never values.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<ItemProblem>>,
    /// A stable machine-readable code for a failure a client acts on, such as
    /// `search_timeout`; absent for the rest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// What is wrong with a segment definition (`segment_invalid`): a JSON pointer into the
    /// definition and a fixed code for each issue, never a value from it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issues: Option<Vec<SegmentIssueProblem>>,
    /// The segments of a reference cycle (`segment_cycle`) in path order, the first one
    /// repeated at the end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cycle: Option<Vec<SegmentRefProblem>>,
    /// What is wrong with a flow (`flow_invalid`): a fixed code and the node, edge and pointer it
    /// concerns, never a value from the flow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_issues: Option<Vec<FlowIssueProblem>>,
    /// What is wrong with a template document (`template_invalid`): a fixed code, how bad it is
    /// and where, never a value from the document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_issues: Option<Vec<TemplateIssueProblem>>,
    /// The templates whose drafts hold a block for a snippet (`snippet_in_use`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet_templates: Option<Vec<SegmentRefProblem>>,
    /// The themes whose published version uses a snippet as its footer (`snippet_in_use`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet_themes: Option<Vec<SegmentRefProblem>>,
    /// The live templates a snippet or theme version would stop from sending
    /// (`snippet_breaks_templates`, `theme_breaks_templates`), at most 20.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub templates: Option<Vec<BrokenTemplateProblem>>,
    /// How many templates `templates` is the first of.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    /// The active flows whose running version sends a template (`template_in_use`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flows: Option<Vec<FlowRefProblem>>,
}

/// A live template that a new snippet or theme version would stop from sending.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct BrokenTemplateProblem {
    pub id: uuid::Uuid,
    pub name: String,
    /// The codes of its blocking issues with the new version.
    pub codes: Vec<String>,
}

/// The most templates a problem lists.
const MAX_LISTED_TEMPLATES: usize = 20;

/// One issue of a flow.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct FlowIssueProblem {
    /// A fixed issue code such as `port_not_wired` or `unknown_segment`.
    pub code: String,
    /// The id of the node it concerns, when it does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// The id of the edge it concerns, when it does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge: Option<String>,
    /// A JSON pointer into the node, the edge or the settings object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

/// One issue of a template document.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct TemplateIssueProblem {
    /// A fixed issue code such as `unknown_merge_field` or `missing_unsubscribe`.
    pub code: String,
    /// `blocking` or `warning`.
    pub severity: String,
    /// A JSON pointer into the document.
    pub at: String,
    /// The merge field the issue concerns, such as `person.first_name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// A short explanation for a problem the code alone does not explain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A flow named in a problem.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct FlowRefProblem {
    pub flow_id: uuid::Uuid,
    pub flow_name: String,
}

/// One issue of a segment definition.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct SegmentIssueProblem {
    /// A JSON pointer into the definition.
    pub pointer: String,
    /// A fixed issue code such as `unknown_trait`.
    pub code: String,
}

/// A segment named in a problem.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct SegmentRefProblem {
    pub id: uuid::Uuid,
    pub name: String,
}

/// One invalid field of one item of an ingestion request.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ItemProblem {
    /// Position of the item in the request; 0 for a single-item endpoint.
    pub index: u32,
    /// The field, or empty when the item itself is the wrong type.
    pub field: String,
    /// A stable code such as `unknown_field`, `required` or `invalid_email`.
    pub reason: String,
}

/// The fields only a segment problem carries.#[derive(Debug)]struct SegmentExtra {    issues: Option<Vec<SegmentIssueProblem>>,    cycle: Option<Vec<SegmentRefProblem>>,}
/// The fields only a segment problem carries.
#[derive(Debug, Default)]
struct SegmentExtra {
    issues: Option<Vec<SegmentIssueProblem>>,
    cycle: Option<Vec<SegmentRefProblem>>,
    /// The issues of a flow (`flow_invalid`); it shares the box so the error stays small.
    flow_issues: Option<Vec<FlowIssueProblem>>,
    /// The issues of a template document (`template_invalid`).
    template_issues: Option<Vec<TemplateIssueProblem>>,
    /// The templates that use a snippet (`snippet_in_use`).
    snippet_templates: Option<Vec<SegmentRefProblem>>,
    /// The themes that use a snippet as their footer (`snippet_in_use`).
    snippet_themes: Option<Vec<SegmentRefProblem>>,
    /// The templates a new snippet or theme version would break, and how many there are.
    templates: Option<(Vec<BrokenTemplateProblem>, u32)>,
    /// The active flows that send a template (`template_in_use`).
    flows: Option<Vec<FlowRefProblem>>,
}

/// An error that renders as `application/problem+json`.
#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    detail: String,
    errors: Option<Vec<ItemProblem>>,
    retry_after: Option<u64>,
    code: Option<&'static str>,
    /// The extra fields of a segment problem, boxed to keep the error small.
    segment: Option<Box<SegmentExtra>>,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
            errors: None,
            retry_after: None,
            code: None,
            segment: None,
        }
    }

    /// Sets the stable machine-readable code of the problem.
    pub(crate) fn with_code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }

    /// Lists the invalid fields in the body.
    pub(crate) fn with_errors(mut self, errors: Vec<ItemProblem>) -> Self {
        self.errors = Some(errors);
        self
    }

    /// Adds `Retry-After`, in whole seconds.
    pub(crate) fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds);
        self
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn status_code(&self) -> StatusCode {
        self.status
    }

    /// The stable code of the problem, when it has one.
    pub(crate) fn code(&self) -> Option<&'static str> {
        self.code
    }

    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }

    /// Missing, wrong, expired or revoked credentials. One body for every cause, so a
    /// response never says which part of a credential was wrong.
    pub(crate) fn unauthenticated() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "authentication required")
    }

    pub(crate) fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, detail)
    }

    pub(crate) fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, detail)
    }

    /// Also the answer for a resource the caller may not know exists.
    pub(crate) fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "no such resource")
    }

    /// A validation failure. The detail names the field and the rule, never the value.
    pub(crate) fn unprocessable(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, detail)
    }

    /// An unexpected failure; the cause is deliberately not shown to the caller.
    pub(crate) fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }

    /// The source reported field names no mapping can address (blank or duplicate). The code names
    /// the rule, never a field.
    pub(crate) fn source_fields_invalid(code: &'static str) -> Self {
        Self {
            code: Some(code),
            ..Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "the source reported field names that cannot be mapped",
            )
        }
    }

    /// A caller without access to values asked for an order or prefix search on a PII trait.
    pub(crate) fn pii_trait_not_searchable() -> Self {
        Self {
            code: Some("pii_trait_not_searchable"),
            ..Self::new(
                StatusCode::FORBIDDEN,
                "trait.op: only eq and exists can search a PII trait without access to values",
            )
        }
    }

    /// An `Idempotency-Key` that was first used for a different request.
    pub(crate) fn idempotency_key_reused() -> Self {
        Self {
            code: Some("idempotency_key_reused"),
            ..Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "the Idempotency-Key was already used for a different request",
            )
        }
    }

    /// A bounded read that ran past its time limit and was cancelled.
    pub(crate) fn search_timeout() -> Self {
        Self {
            code: Some("search_timeout"),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the query took too long; narrow it and try again",
            )
        }
    }

    /// An explorer query that ran past its time limit and was cancelled.
    pub(crate) fn query_timeout() -> Self {
        Self {
            code: Some("query_timeout"),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the query took too long; narrow the time range or the filters and try again",
            )
        }
    }

    /// The stream already has a queued or running sync run.
    pub(crate) fn sync_run_active() -> Self {
        Self {
            code: Some("sync_run_active"),
            ..Self::new(
                StatusCode::CONFLICT,
                "the stream already has a run that is queued or running",
            )
        }
    }

    /// A change of a sync mode, cursor field, primary key or source setting while a sync run is
    /// queued or running: the run would undo it.
    pub(crate) fn sync_run_blocks_change() -> Self {
        Self {
            code: Some("sync_run_active"),
            ..Self::new(
                StatusCode::CONFLICT,
                "a sync run is queued or running; wait for it to finish or cancel it, then change the sync mode, cursor field, primary key or source settings",
            )
        }
    }

    /// A consent was asked for before the operator set `server.public_url`: the provider could not be
    /// told where to send the browser back.
    pub(crate) fn public_url_not_configured() -> Self {
        Self {
            code: Some("public_url_not_configured"),
            ..Self::new(
                StatusCode::CONFLICT,
                "connecting an account needs server.public_url to be set",
            )
        }
    }

    /// A sample was asked for with a draft that changes where requests go or how they authenticate:
    /// the saved account and credential must not be used with an endpoint they were not set up for.
    pub(crate) fn manifest_draft_auth_changed() -> Self {
        Self {
            code: Some("manifest_draft_auth_changed"),
            ..Self::new(
                StatusCode::CONFLICT,
                "save the manifest to sample with a changed base URL or authentication",
            )
        }
    }

    /// A consent was asked for a source whose manifest does not use the authorization-code grant.
    pub(crate) fn oauth_not_configured() -> Self {
        Self {
            code: Some("oauth_not_configured"),
            ..Self::new(
                StatusCode::CONFLICT,
                "this source does not use an OAuth2 authorization-code manifest",
            )
        }
    }

    /// A run was asked for a stream that has no mapping, or for a source with none mapped.
    pub(crate) fn stream_not_mapped(detail: impl Into<String>) -> Self {
        Self {
            code: Some("stream_not_mapped"),
            ..Self::new(StatusCode::UNPROCESSABLE_ENTITY, detail)
        }
    }

    /// A run was asked for a stream that is not enabled.
    pub(crate) fn stream_not_enabled() -> Self {
        Self {
            code: Some("stream_not_enabled"),
            ..Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "the stream is not enabled; enable it before running it",
            )
        }
    }

    /// A reconciliation was asked for or scheduled on a stream that is not incremental. A full
    /// refresh already sweeps after every run.
    pub(crate) fn reconcile_needs_incremental() -> Self {
        Self {
            code: Some("reconcile_needs_incremental"),
            ..Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "only an incremental stream can be reconciled; a full refresh removes what the source dropped after every run",
            )
        }
    }

    /// The sweep limit of a stream is not a share above 0 and at most 1.
    pub(crate) fn invalid_sweep_limit() -> Self {
        Self {
            code: Some("invalid_sweep_limit"),
            ..Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "sweep_limit must be above 0 and at most 1",
            )
        }
    }

    /// A CSV import was given a mapping with a delete signal, which only a synced stream reads.
    pub(crate) fn delete_signal_unsupported() -> Self {
        Self {
            code: Some("delete_signal_unsupported"),
            ..Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "mapping: delete_when is for a synced stream; a CSV import has no deletes",
            )
        }
    }

    /// A caller without access to values asked to probe or break down a PII property.
    pub(crate) fn pii_property_not_explorable() -> Self {
        Self {
            code: Some("pii_property_not_explorable"),
            ..Self::new(
                StatusCode::FORBIDDEN,
                "only eq and exists apply to a PII or undeclared property without access to                  values, and it cannot be broken down",
            )
        }
    }
}

impl ApiError {
    /// A segment definition that does not validate: every issue with its pointer and code.
    pub(crate) fn segment_invalid(issues: Vec<SegmentIssueProblem>) -> Self {
        Self {
            code: Some("segment_invalid"),
            segment: Some(Box::new(SegmentExtra {
                issues: Some(issues),
                cycle: None,
                ..SegmentExtra::default()
            })),
            ..Self::unprocessable("the segment definition is not valid")
        }
    }

    /// A consent policy whose rules or conditions do not validate: every issue with its pointer
    /// into the request and a fixed code.
    pub(crate) fn consent_policy_invalid(issues: Vec<SegmentIssueProblem>) -> Self {
        Self {
            code: Some("consent_policy_invalid"),
            segment: Some(Box::new(SegmentExtra {
                issues: Some(issues),
                cycle: None,
                ..SegmentExtra::default()
            })),
            ..Self::unprocessable("the consent policy is not valid")
        }
    }

    /// A save that would make segments reference each other. The detail names the cycle as
    /// "A → B → A".
    pub(crate) fn segment_cycle(cycle: Vec<SegmentRefProblem>) -> Self {
        let path: Vec<&str> = cycle.iter().map(|segment| segment.name.as_str()).collect();
        Self {
            code: Some("segment_cycle"),
            ..Self::unprocessable(format!(
                "segments cannot reference each other: {}",
                path.join(" \u{2192} ")
            ))
            .with_cycle(cycle)
        }
    }

    fn with_cycle(mut self, cycle: Vec<SegmentRefProblem>) -> Self {
        self.segment = Some(Box::new(SegmentExtra {
            issues: None,
            cycle: Some(cycle),
            ..SegmentExtra::default()
        }));
        self
    }

    /// An archive of a segment that another live segment's current version still references.
    pub(crate) fn segment_referenced(by: &[SegmentRefProblem]) -> Self {
        let names: Vec<&str> = by.iter().map(|segment| segment.name.as_str()).collect();
        Self {
            code: Some("segment_referenced"),
            ..Self::new(
                StatusCode::CONFLICT,
                format!("referenced by {}", names.join(", ")),
            )
        }
    }

    /// A flow that does not pass the publish checks: every issue with its code and place.
    pub(crate) fn flow_invalid(issues: Vec<FlowIssueProblem>) -> Self {
        Self {
            code: Some("flow_invalid"),
            segment: Some(Box::new(SegmentExtra {
                flow_issues: Some(issues),
                ..SegmentExtra::default()
            })),
            ..Self::unprocessable("the flow is not valid")
        }
    }

    /// A graph or settings document that is not a flow document at all (wrong shape, unknown
    /// node type or key), which a draft cannot hold.
    pub(crate) fn flow_malformed(detail: impl Into<String>) -> Self {
        Self {
            code: Some("flow_malformed"),
            ..Self::unprocessable(detail)
        }
    }

    /// A lifecycle action the flow's state does not allow.
    pub(crate) fn flow_state(detail: impl Into<String>) -> Self {
        Self {
            code: Some("flow_state"),
            ..Self::new(StatusCode::CONFLICT, detail)
        }
    }

    /// A retry or skip of an enrollment that is not failed, or whose failure it cannot reach.
    pub(crate) fn enrollment_state(detail: impl Into<String>) -> Self {
        Self {
            code: Some("enrollment_state"),
            ..Self::new(StatusCode::CONFLICT, detail)
        }
    }

    /// A write to an archived flow.
    pub(crate) fn flow_archived() -> Self {
        Self {
            code: Some("flow_archived"),
            ..Self::new(StatusCode::CONFLICT, "the flow is archived")
        }
    }

    /// An operator control on a paused flow: a step would be held, so the flow is resumed first.
    pub(crate) fn flow_paused() -> Self {
        Self {
            code: Some("flow_paused"),
            ..Self::new(StatusCode::CONFLICT, "the flow is paused; resume it first")
        }
    }

    /// A write to an archived segment.
    pub(crate) fn segment_archived() -> Self {
        Self {
            code: Some("segment_archived"),
            ..Self::new(StatusCode::CONFLICT, "the segment is archived")
        }
    }

    /// An explain that ran past its time limit and was cancelled.
    pub(crate) fn explain_timeout() -> Self {
        Self {
            code: Some("explain_timeout"),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the explanation took too long; try again",
            )
        }
    }

    /// A preview that ran past its time limit and was cancelled.
    pub(crate) fn segment_preview_timeout() -> Self {
        Self {
            code: Some("segment_preview_timeout"),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the preview took too long; narrow the definition and try again",
            )
        }
    }

    /// A test-mode request that is refused for its shape or a bound: `code` is one of the stable
    /// `flow_test_*` codes.
    pub(crate) fn flow_test_refused(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            ..Self::unprocessable(detail)
        }
    }

    /// A test-mode read of a real person that ran past its time limit and was cancelled.
    pub(crate) fn flow_test_timeout() -> Self {
        Self {
            code: Some("flow_test_timeout"),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the test took too long reading the person; try again",
            )
        }
    }

    /// A test of a real person by a caller who may not see personal values.
    pub(crate) fn flow_test_values_forbidden() -> Self {
        Self {
            code: Some("flow_test_values_forbidden"),
            ..Self::forbidden(
                "testing a real person returns their values; ask a workspace admin, or post a snapshot",
            )
        }
    }
}

impl ApiError {
    /// The assistant is off for the workspace, or its model is no longer offered.
    pub(crate) fn ai_disabled() -> Self {
        Self {
            code: Some("ai_disabled"),
            ..Self::new(
                StatusCode::CONFLICT,
                "the AI assistant is not enabled for this workspace",
            )
        }
    }

    /// The workspace's monthly AI budget is spent; nothing was sent to the provider.
    pub(crate) fn ai_budget_exceeded() -> Self {
        Self {
            code: Some("ai_budget_exceeded"),
            ..Self::new(
                StatusCode::PAYMENT_REQUIRED,
                "the workspace's monthly AI budget is spent",
            )
        }
    }

    /// A user is over the per-minute AI call limit; nothing was sent to the provider.
    pub(crate) fn ai_rate_limited() -> Self {
        Self {
            code: Some("ai_rate_limited"),
            ..Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too many AI calls; try again in a minute",
            )
            .with_retry_after(60)
        }
    }

    /// A request carried personal data the workspace has not opted in to sending.
    pub(crate) fn ai_personal_data_not_allowed() -> Self {
        Self {
            code: Some("personal_data_not_allowed"),
            ..Self::unprocessable("personal data is not allowed in this workspace's AI requests")
        }
    }

    /// The provider could not be reached or answered unusably. The detail is fixed text, never
    /// the provider's own message.
    pub(crate) fn ai_provider_failed(detail: impl Into<String>) -> Self {
        Self {
            code: Some("ai_provider_error"),
            ..Self::new(StatusCode::BAD_GATEWAY, detail)
        }
    }

    /// An assistant draft is run by and for a signed-in person; an API key is refused.
    pub(crate) fn ai_user_required() -> Self {
        Self {
            code: Some("ai_user_required"),
            ..Self::forbidden("the assistant needs a signed-in user; an API key cannot use it")
        }
    }

    /// A draft that never validated has no content to accept.
    pub(crate) fn ai_draft_invalid() -> Self {
        Self {
            code: Some("ai_draft_invalid"),
            ..Self::new(
                StatusCode::CONFLICT,
                "this draft did not pass validation and cannot be accepted",
            )
        }
    }

    /// A segment draft that no longer passes the rules a save applies: every issue with its
    /// pointer and code, never a value.
    pub(crate) fn ai_draft_invalid_issues(issues: Vec<SegmentIssueProblem>) -> Self {
        Self {
            segment: Some(Box::new(SegmentExtra {
                issues: Some(issues),
                cycle: None,
                ..SegmentExtra::default()
            })),
            ..Self::ai_draft_invalid()
        }
    }

    /// An explanation that did not pass its checks within the repair attempts: the issue codes
    /// of the last attempt.
    pub(crate) fn ai_explain_failed(issues: Vec<SegmentIssueProblem>) -> Self {
        Self::ai_explain_failed_for("segment", issues)
    }

    /// [`Self::ai_explain_failed`] for what was explained: a segment, a flow or a trace.
    pub(crate) fn ai_explain_failed_for(what: &str, issues: Vec<SegmentIssueProblem>) -> Self {
        Self {
            code: Some("ai_explain_failed"),
            segment: Some(Box::new(SegmentExtra {
                issues: Some(issues),
                cycle: None,
                ..SegmentExtra::default()
            })),
            ..Self::unprocessable(format!(
                "the assistant could not explain this {what}; try again"
            ))
        }
    }

    /// A template document too large to send to the model.
    pub(crate) fn ai_template_too_large() -> Self {
        Self {
            code: Some("ai_template_too_large"),
            ..Self::unprocessable(
                "this template is too large to send to the assistant (more than 96 KiB); \
                 shorten it first",
            )
        }
    }

    /// A template or a task the assistant does not draft.
    pub(crate) fn ai_template_unsupported(detail: impl Into<String>) -> Self {
        Self {
            code: Some("ai_template_unsupported"),
            ..Self::unprocessable(detail)
        }
    }

    /// A flow too large to send to the model with its personal literals masked.
    pub(crate) fn ai_flow_too_large() -> Self {
        Self {
            code: Some("ai_flow_too_large"),
            ..Self::unprocessable(
                "this flow is too large to send to the assistant without personal data (more than \
                 200 nodes, or a condition too deep); simplify it first",
            )
        }
    }

    /// The saved mapping or manifest changed after the draft was made.
    pub(crate) fn ai_draft_outdated() -> Self {
        Self {
            code: Some("ai_draft_outdated"),
            ..Self::new(
                StatusCode::CONFLICT,
                "the saved version changed since this draft was made; draft again",
            )
        }
    }

    /// The chosen model lacks required capabilities; each is listed with its reason.
    pub(crate) fn ai_capability_missing(errors: Vec<ItemProblem>) -> Self {
        Self {
            code: Some("ai_capability_missing"),
            ..Self::unprocessable("the model lacks capabilities the assistant requires")
                .with_errors(errors)
        }
    }

    /// Stored secrets need the master key, which this deployment has not been given.
    pub(crate) fn secrets_unavailable() -> Self {
        Self {
            code: Some("secrets_unavailable"),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "stored secrets are not configured on this deployment",
            )
        }
    }
}

impl ApiError {
    /// A template draft that does not pass the publish checks: every blocking issue and warning
    /// with its code and place.
    pub(crate) fn template_invalid(issues: Vec<TemplateIssueProblem>) -> Self {
        Self {
            code: Some("template_invalid"),
            segment: Some(Box::new(SegmentExtra {
                template_issues: Some(issues),
                ..SegmentExtra::default()
            })),
            ..Self::unprocessable("the template is not valid")
        }
    }

    /// A content document that is not a template document at all (wrong shape or types), which a
    /// draft cannot hold.
    pub(crate) fn template_malformed(detail: impl Into<String>) -> Self {
        Self {
            code: Some("template_malformed"),
            ..Self::unprocessable(detail)
        }
    }

    /// A document whose `schema` is not the one this server reads.
    pub(crate) fn template_unsupported_schema() -> Self {
        Self {
            code: Some("template_unsupported_schema"),
            ..Self::unprocessable("the document schema is not supported by this server")
        }
    }

    /// A block document with a block `type` that is not part of the v1 block set.
    pub(crate) fn template_unknown_block() -> Self {
        Self {
            code: Some("template_unknown_block"),
            ..Self::unprocessable("a block has a type that is not part of the v1 block set")
        }
    }

    /// A document with a field the schema does not define.
    pub(crate) fn template_unknown_field() -> Self {
        Self {
            code: Some("template_unknown_field"),
            ..Self::unprocessable("an object has a field the document schema does not define")
        }
    }

    /// A document whose channel or category differs from the template's, which are fixed when the
    /// template is made.
    pub(crate) fn template_mismatch(detail: impl Into<String>) -> Self {
        Self {
            code: Some("template_mismatch"),
            ..Self::unprocessable(detail)
        }
    }

    /// A document of a channel and mode that no registered content type handles.
    pub(crate) fn template_content_unsupported() -> Self {
        Self {
            code: Some("template_content_unsupported"),
            ..Self::unprocessable("no registered content type handles this document")
        }
    }

    /// A publish whose draft was saved again while it was being validated.
    pub(crate) fn template_draft_changed() -> Self {
        Self {
            code: Some("template_draft_changed"),
            ..Self::new(
                StatusCode::CONFLICT,
                "the draft changed while it was being validated; publish again",
            )
        }
    }

    /// A draft save that names a revision other than the current one: the draft was saved by
    /// someone else since the caller read it.
    pub(crate) fn template_draft_revision_mismatch() -> Self {
        Self {
            code: Some("template_draft_revision_mismatch"),
            ..Self::new(
                StatusCode::CONFLICT,
                "the draft was saved by someone else; read it again",
            )
        }
    }

    /// Accepting an assistant draft of a template whose draft was saved, or already replaced by
    /// this very draft, since the assistant's draft was made.
    pub(crate) fn template_draft_changed_since_assistant_draft() -> Self {
        Self {
            code: Some("template_draft_changed"),
            ..Self::new(
                StatusCode::CONFLICT,
                "The template draft changed after this draft was made; ask the assistant again.",
            )
        }
    }

    /// A save or publish of the consent policy that names a draft revision other than the current
    /// one: the draft was saved by someone else since the caller read it.
    pub(crate) fn consent_policy_revision_mismatch() -> Self {
        Self {
            code: Some("consent_policy_revision_mismatch"),
            ..Self::new(
                StatusCode::CONFLICT,
                "the consent policy draft is not at the revision named; read it again",
            )
        }
    }

    /// A replace of the send settings that names a revision other than the current one: they were
    /// replaced by someone else since the caller read them.
    pub(crate) fn send_settings_revision_mismatch() -> Self {
        Self {
            code: Some("send_settings_revision_mismatch"),
            ..Self::new(
                StatusCode::CONFLICT,
                "the send settings are not at the revision named; read them again",
            )
        }
    }

    /// A publish (or, later, a send) whose sender identity may not send: it is unknown, archived,
    /// or on a domain whose DKIM, SPF and DMARC records are not all verified.
    pub(crate) fn sender_unverified(detail: impl Into<String>) -> Self {
        Self {
            code: Some("sender_unverified"),
            ..Self::unprocessable(detail)
        }
    }

    /// A sending domain that still has live sender identities, or identities that a template
    /// names.
    pub(crate) fn sending_domain_in_use() -> Self {
        Self {
            code: Some("sending_domain_in_use"),
            ..Self::new(
                StatusCode::CONFLICT,
                "the sending domain still has sender identities in use; archive them, and remove them from templates, first",
            )
        }
    }

    /// A write to an archived template.
    pub(crate) fn template_archived() -> Self {
        Self {
            code: Some("template_archived"),
            ..Self::new(StatusCode::CONFLICT, "the template is archived")
        }
    }

    /// A snippet that does not pass the publish checks: every blocking issue and warning with its
    /// code and place.
    pub(crate) fn snippet_invalid(issues: Vec<TemplateIssueProblem>) -> Self {
        Self {
            code: Some("snippet_invalid"),
            segment: Some(Box::new(SegmentExtra {
                template_issues: Some(issues),
                ..SegmentExtra::default()
            })),
            ..Self::unprocessable("the snippet is not valid")
        }
    }

    /// A snippet version that would stop live templates from sending.
    pub(crate) fn snippet_breaks_templates(broken: Vec<BrokenTemplateProblem>) -> Self {
        Self::breaks_templates("snippet_breaks_templates", "snippet", broken)
    }

    /// A theme version that would stop live templates from sending.
    pub(crate) fn theme_breaks_templates(broken: Vec<BrokenTemplateProblem>) -> Self {
        Self::breaks_templates("theme_breaks_templates", "theme", broken)
    }

    fn breaks_templates(
        code: &'static str,
        subject: &str,
        mut broken: Vec<BrokenTemplateProblem>,
    ) -> Self {
        let total = u32::try_from(broken.len()).unwrap_or(u32::MAX);
        broken.truncate(MAX_LISTED_TEMPLATES);
        Self {
            code: Some(code),
            segment: Some(Box::new(SegmentExtra {
                templates: Some((broken, total)),
                ..SegmentExtra::default()
            })),
            ..Self::unprocessable(format!(
                "This {subject} version would stop {total} live templates from sending; fix \
                 them or the version first."
            ))
        }
    }

    /// A snippet content that is not a block list at all, which a draft cannot hold.
    pub(crate) fn snippet_malformed(detail: impl Into<String>) -> Self {
        Self {
            code: Some("snippet_malformed"),
            ..Self::unprocessable(detail)
        }
    }

    /// A write to an archived snippet.
    pub(crate) fn snippet_archived() -> Self {
        Self {
            code: Some("snippet_archived"),
            ..Self::new(StatusCode::CONFLICT, "the snippet is archived")
        }
    }

    /// A publish whose draft was saved again while it was being validated.
    pub(crate) fn snippet_draft_changed() -> Self {
        Self {
            code: Some("snippet_draft_changed"),
            ..Self::new(
                StatusCode::CONFLICT,
                "the draft changed while it was being validated; publish again",
            )
        }
    }

    /// A draft save that names a revision other than the current one: the draft was saved by
    /// someone else since the caller read it.
    pub(crate) fn snippet_revision_mismatch() -> Self {
        Self {
            code: Some("snippet_revision_mismatch"),
            ..Self::new(
                StatusCode::CONFLICT,
                "the snippet draft is not at the revision named; read it again",
            )
        }
    }

    /// An archive of a snippet that template drafts or published theme footers still use, with
    /// those templates and themes.
    pub(crate) fn snippet_in_use(
        templates: Vec<SegmentRefProblem>,
        themes: Vec<SegmentRefProblem>,
    ) -> Self {
        Self {
            code: Some("snippet_in_use"),
            segment: Some(Box::new(SegmentExtra {
                snippet_templates: Some(templates),
                snippet_themes: Some(themes),
                ..SegmentExtra::default()
            })),
            ..Self::new(
                StatusCode::CONFLICT,
                "template drafts or theme footers still use the snippet; remove it from them first",
            )
        }
    }

    /// A theme that does not pass the publish checks: every blocking issue and warning with its
    /// code and place.
    pub(crate) fn theme_invalid(issues: Vec<TemplateIssueProblem>) -> Self {
        Self {
            code: Some("theme_invalid"),
            segment: Some(Box::new(SegmentExtra {
                template_issues: Some(issues),
                ..SegmentExtra::default()
            })),
            ..Self::unprocessable("the theme is not valid")
        }
    }

    /// Theme tokens that are not a set of tokens at all, which a draft cannot hold.
    pub(crate) fn theme_malformed(detail: impl Into<String>) -> Self {
        Self {
            code: Some("theme_malformed"),
            ..Self::unprocessable(detail)
        }
    }

    /// A write to an archived theme.
    pub(crate) fn theme_archived() -> Self {
        Self {
            code: Some("theme_archived"),
            ..Self::new(StatusCode::CONFLICT, "the theme is archived")
        }
    }

    /// A publish whose draft was saved again while it was being validated.
    pub(crate) fn theme_draft_changed() -> Self {
        Self {
            code: Some("theme_draft_changed"),
            ..Self::new(
                StatusCode::CONFLICT,
                "the draft changed while it was being validated; publish again",
            )
        }
    }

    /// A draft save that names a revision other than the current one.
    pub(crate) fn theme_revision_mismatch() -> Self {
        Self {
            code: Some("theme_revision_mismatch"),
            ..Self::new(
                StatusCode::CONFLICT,
                "the theme draft is not at the revision named; read it again",
            )
        }
    }

    /// An archive of the workspace default theme.
    pub(crate) fn theme_is_default() -> Self {
        Self {
            code: Some("theme_is_default"),
            ..Self::new(
                StatusCode::CONFLICT,
                "the theme is the workspace default; make another theme the default first",
            )
        }
    }

    /// A default that is not a published, live theme of the workspace.
    pub(crate) fn theme_not_publishable_default(detail: impl Into<String>) -> Self {
        Self {
            code: Some("theme_not_publishable_default"),
            ..Self::new(StatusCode::CONFLICT, detail)
        }
    }

    /// An archive of a template that active flows still send, with those flows.
    pub(crate) fn template_in_use(flows: Vec<FlowRefProblem>) -> Self {
        Self {
            code: Some("template_in_use"),
            segment: Some(Box::new(SegmentExtra {
                flows: Some(flows),
                ..SegmentExtra::default()
            })),
            ..Self::new(
                StatusCode::CONFLICT,
                "active flows still send the template; remove it from them first",
            )
        }
    }

    /// A template that already keeps the most saved preview people.
    pub(crate) fn preview_people_limit() -> Self {
        Self {
            code: Some("preview_people_limit"),
            ..Self::new(
                StatusCode::CONFLICT,
                format!(
                    "a template keeps at most {} preview people",
                    zirv_flow_store::MAX_PREVIEW_PEOPLE
                ),
            )
        }
    }

    /// A preview as a real person by a caller who may not see personal values.
    pub(crate) fn template_preview_values_forbidden() -> Self {
        Self {
            code: Some("template_preview_values_forbidden"),
            ..Self::forbidden(
                "previewing a real person shows their values; ask a workspace admin, or preview a sample person",
            )
        }
    }

    /// A preview that could not render. The renderer's own refusal and a render that blew its
    /// limits are the template's doing (422); a render host that died or spoke nonsense is ours
    /// (503, 502). None of them takes the API down: the pool replaces the child.
    pub(crate) fn render_failed(failure: &zirv_flow_render::RenderFailure) -> Self {
        use zirv_flow_render::RenderFailure;

        match failure {
            RenderFailure::Render(error) => {
                let detail = match error.field() {
                    Some(field) => format!(
                        "the template could not be rendered: {} in {field}",
                        error.code()
                    ),
                    None => format!("the template could not be rendered: {}", error.code()),
                };
                Self {
                    code: Some("template_render_failed"),
                    ..Self::unprocessable(detail)
                }
            }
            RenderFailure::UnsupportedDocument => Self::template_content_unsupported(),
            RenderFailure::MemoryLimit => Self {
                code: Some("render_memory_limit"),
                ..Self::unprocessable("the template used too much memory to render")
            },
            RenderFailure::Deadline => Self {
                code: Some("render_deadline"),
                ..Self::unprocessable("rendering the template took too long")
            },
            RenderFailure::RequestTooLarge => Self {
                code: Some("render_request_too_large"),
                ..Self::unprocessable("the template and its values are too large to render")
            },
            RenderFailure::ChildExited => Self {
                code: Some("render_child_exited"),
                ..Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the render host stopped; try again",
                )
            },
            RenderFailure::Protocol => Self {
                code: Some("render_protocol"),
                ..Self::new(
                    StatusCode::BAD_GATEWAY,
                    "the render host answered unusably; try again",
                )
            },
        }
    }

    /// No render pool was started in this process, so nothing can be previewed.
    pub(crate) fn render_unavailable() -> Self {
        Self {
            code: Some("render_unavailable"),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "rendering is not available on this deployment",
            )
        }
    }
}

impl From<zirv_flow_store::ExploreError> for ApiError {
    fn from(error: zirv_flow_store::ExploreError) -> Self {
        use zirv_flow_store::ExploreError;

        match error {
            ExploreError::Invalid { field, reason } => {
                Self::unprocessable(format!("{field}: {reason}"))
            }
            ExploreError::PiiPropertyNotExplorable => Self::pii_property_not_explorable(),
            ExploreError::Store(error) if error.is_timeout() => Self::query_timeout(),
            ExploreError::Store(_) => Self::internal(),
        }
    }
}

impl From<zirv_flow_core::ValidationError> for ApiError {
    fn from(error: zirv_flow_core::ValidationError) -> Self {
        Self::unprocessable(error.to_string())
    }
}

impl From<zirv_flow_store::StoreError> for ApiError {
    fn from(error: zirv_flow_store::StoreError) -> Self {
        if error.is_timeout() {
            Self::search_timeout()
        } else if error.is_run_active() {
            Self::sync_run_blocks_change()
        } else {
            Self::internal()
        }
    }
}

impl From<crate::admin::AdminError> for ApiError {
    fn from(error: crate::admin::AdminError) -> Self {
        use crate::admin::AdminError;

        match error {
            AdminError::Invalid(error) => error.into(),
            AdminError::Conflict(detail) => Self::new(StatusCode::CONFLICT, detail),
            AdminError::NotFound(_) => Self::not_found(),
            AdminError::Store(error) => error.into(),
            AdminError::Slot(error) => crate::people::slots::slot_problem(&error),
            AdminError::Secret(_) | AdminError::Ledger(_) | AdminError::Merge(_) => {
                Self::internal()
            }
            AdminError::Utm { code, detail } => Self {
                code: Some(code),
                ..Self::unprocessable(detail)
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(mut self) -> Response {
        let templates = self
            .segment
            .as_mut()
            .and_then(|extra| extra.templates.take());
        let problem = Problem {
            kind: "about:blank".into(),
            title: self.status.canonical_reason().unwrap_or("Error").into(),
            status: self.status.as_u16(),
            detail: self.detail,
            request_id: request_id::current(),
            trace_id: trace::current().map(|trace| trace.trace_id.to_string()),
            errors: self.errors,
            code: self.code.map(str::to_owned),
            issues: self.segment.as_mut().and_then(|extra| extra.issues.take()),
            cycle: self.segment.as_mut().and_then(|extra| extra.cycle.take()),
            flow_issues: self
                .segment
                .as_mut()
                .and_then(|extra| extra.flow_issues.take()),
            template_issues: self
                .segment
                .as_mut()
                .and_then(|extra| extra.template_issues.take()),
            snippet_templates: self
                .segment
                .as_mut()
                .and_then(|extra| extra.snippet_templates.take()),
            snippet_themes: self
                .segment
                .as_mut()
                .and_then(|extra| extra.snippet_themes.take()),
            total: templates.as_ref().map(|(_, total)| *total),
            templates: templates.map(|(listed, _)| listed),
            flows: self.segment.as_mut().and_then(|extra| extra.flows.take()),
        };
        let body = serde_json::to_vec(&problem).unwrap_or_default();
        let mut response = (
            self.status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            body,
        )
            .into_response();
        if let Some(seconds) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, seconds.into());
        }
        response
    }
}

pub(crate) async fn method_not_allowed() -> ApiError {
    ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_breaks_templates_problem_lists_twenty_and_counts_all() {
        let broken: Vec<BrokenTemplateProblem> = (0..25)
            .map(|n| BrokenTemplateProblem {
                id: uuid::Uuid::from_u128(n),
                name: format!("Template {n}"),
                codes: vec!["missing_unsubscribe".to_owned()],
            })
            .collect();

        let error = ApiError::snippet_breaks_templates(broken);

        assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.code, Some("snippet_breaks_templates"));
        assert!(error.detail.contains("25 live templates"));
        let (listed, total) = error.segment.and_then(|extra| extra.templates).unwrap();
        assert_eq!((listed.len(), total), (20, 25));
        assert_eq!(listed[19].name, "Template 19");
    }

    #[test]
    fn a_template_in_use_is_a_409_that_lists_the_flows() {
        let flow = uuid::Uuid::nil();
        let error = ApiError::template_in_use(vec![FlowRefProblem {
            flow_id: flow,
            flow_name: "Welcome".into(),
        }]);

        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.code(), Some("template_in_use"));
        let flows = error
            .segment
            .as_ref()
            .and_then(|extra| extra.flows.as_ref());
        assert_eq!(flows.map(Vec::len), Some(1));
    }

    #[test]
    fn the_standard_errors_carry_the_expected_status() {
        assert_eq!(ApiError::unauthenticated().status, StatusCode::UNAUTHORIZED);
        assert_eq!(ApiError::forbidden("no").status, StatusCode::FORBIDDEN);
        assert_eq!(ApiError::not_found().status, StatusCode::NOT_FOUND);
        assert_eq!(
            ApiError::unprocessable("x").status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            ApiError::internal().status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn the_consent_refusals_are_conflicts_with_a_code() {
        for (error, code) in [
            (
                ApiError::public_url_not_configured(),
                "public_url_not_configured",
            ),
            (ApiError::oauth_not_configured(), "oauth_not_configured"),
        ] {
            assert_eq!(error.status(), StatusCode::CONFLICT);
            assert_eq!(error.code, Some(code));
        }
    }

    #[test]
    fn an_unauthenticated_error_does_not_say_why() {
        assert_eq!(
            ApiError::unauthenticated().detail,
            "authentication required"
        );
    }

    #[test]
    fn a_validation_error_becomes_a_422_naming_the_field() {
        let error = ApiError::from(zirv_flow_core::password::validate("short").unwrap_err());

        assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(error.detail.starts_with("password:"));
        assert!(!error.detail.contains("short"));
    }

    #[test]
    fn admin_errors_map_to_their_statuses() {
        use crate::admin::AdminError;

        let status = |error| ApiError::from(error).status;
        assert_eq!(status(AdminError::Conflict("taken")), StatusCode::CONFLICT);
        assert_eq!(status(AdminError::NotFound("user")), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_ai_problems_carry_their_status_code_and_retry_hint() {
        let cases = [
            (ApiError::ai_disabled(), StatusCode::CONFLICT, "ai_disabled"),
            (
                ApiError::ai_user_required(),
                StatusCode::FORBIDDEN,
                "ai_user_required",
            ),
            (
                ApiError::ai_draft_invalid(),
                StatusCode::CONFLICT,
                "ai_draft_invalid",
            ),
            (
                ApiError::ai_explain_failed(Vec::new()),
                StatusCode::UNPROCESSABLE_ENTITY,
                "ai_explain_failed",
            ),
            (
                ApiError::ai_draft_invalid_issues(Vec::new()),
                StatusCode::CONFLICT,
                "ai_draft_invalid",
            ),
            (
                ApiError::ai_draft_outdated(),
                StatusCode::CONFLICT,
                "ai_draft_outdated",
            ),
            (
                ApiError::ai_budget_exceeded(),
                StatusCode::PAYMENT_REQUIRED,
                "ai_budget_exceeded",
            ),
            (
                ApiError::ai_rate_limited(),
                StatusCode::TOO_MANY_REQUESTS,
                "ai_rate_limited",
            ),
            (
                ApiError::ai_personal_data_not_allowed(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "personal_data_not_allowed",
            ),
            (
                ApiError::ai_provider_failed("the request timed out"),
                StatusCode::BAD_GATEWAY,
                "ai_provider_error",
            ),
            (
                ApiError::ai_capability_missing(vec![ItemProblem {
                    index: 0,
                    field: "usage".into(),
                    reason: "no usage".into(),
                }]),
                StatusCode::UNPROCESSABLE_ENTITY,
                "ai_capability_missing",
            ),
            (
                ApiError::secrets_unavailable(),
                StatusCode::SERVICE_UNAVAILABLE,
                "secrets_unavailable",
            ),
        ];
        for (error, status, code) in cases {
            let response = error.into_response();
            assert_eq!(response.status(), status, "{code}");
            let retry = response.headers().get(header::RETRY_AFTER).cloned();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["code"], code);
            assert_eq!(retry.is_some(), code == "ai_rate_limited", "{code}");
        }
    }

    #[tokio::test]
    async fn a_problem_inside_a_traced_request_carries_the_trace_id() {
        let trace = zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("req"));
        let expected = trace.trace_id.to_string();

        let response =
            trace::with_trace(trace, async { ApiError::not_found().into_response() }).await;

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["trace_id"], expected.as_str());
    }

    #[tokio::test]
    async fn an_api_error_renders_as_a_problem_document() {
        let response = ApiError::new(StatusCode::NOT_FOUND, "no such resource").into_response();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/problem+json"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            problem,
            serde_json::json!({
                "type": "about:blank",
                "title": "Not Found",
                "status": 404,
                "detail": "no such resource",
                "request_id": null,
                "trace_id": null
            })
        );
    }

    #[tokio::test]
    async fn errors_and_retry_after_are_rendered_and_absent_otherwise() {
        let response = ApiError::new(StatusCode::TOO_MANY_REQUESTS, "slow down")
            .with_retry_after(3)
            .with_errors(vec![ItemProblem {
                index: 2,
                field: "email".into(),
                reason: "invalid_email".into(),
            }])
            .into_response();

        assert_eq!(response.headers()[header::RETRY_AFTER], "3");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            problem["errors"],
            serde_json::json!([{"index": 2, "field": "email", "reason": "invalid_email"}])
        );
        let plain = ApiError::not_found().into_response();
        assert!(plain.headers().get(header::RETRY_AFTER).is_none());
    }

    #[tokio::test]
    async fn a_timeout_is_a_503_problem_with_the_search_timeout_code() {
        let response = ApiError::search_timeout().into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["code"], "search_timeout");
        assert_eq!(problem["status"], 503);
    }

    #[tokio::test]
    async fn an_explorer_timeout_is_a_503_with_the_query_timeout_code() {
        let response = ApiError::query_timeout().into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["code"], "query_timeout");
    }

    #[test]
    fn explorer_refusals_map_to_422_and_403() {
        use zirv_flow_store::ExploreError;

        let invalid = ApiError::from(ExploreError::Invalid {
            field: "to",
            reason: "must be after from",
        });
        let pii = ApiError::from(ExploreError::PiiPropertyNotExplorable);

        assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(invalid.detail(), "to: must be after from");
        assert_eq!(pii.status(), StatusCode::FORBIDDEN);
    }
}
