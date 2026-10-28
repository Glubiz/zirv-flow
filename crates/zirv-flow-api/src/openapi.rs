use utoipa::openapi::security::{ApiKey, ApiKeyValue, Http, HttpAuthScheme, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::assets;
use crate::assistant;
use crate::audit;
use crate::consent::{dimensions, person, policy, suppressions, topics, transfer};
use crate::contacts;
use crate::destinations;
use crate::events::{
    self, CreateEventSchemaRequest, EventPolicyBody, EventSchemaBody, EventSchemaDetailBody,
    IngestRejectBody, IngestRejectsBody, PropertyBody, ReasonBody, RedactionBody, SaveDraftRequest,
    SchemaVersionBody, UpdateEventSchemaRequest,
};
use crate::feedback;
use crate::flows::{self, FlowSchemas};
use crate::forwarding;
use crate::health::{self, StatusBody};
use crate::imports;
use crate::ingest::{
    self, AcceptedBody, BatchItemRequest, BatchRequest, IdentifyRequest, TrackRequest,
};
use crate::jobs_admin::{self, DeadJobsBody, DepthBody, JobBody, KindDepthBody};
use crate::keys::{self, ApiKeyBody, CreateApiKeyRequest, CreatedApiKeyBody};
use crate::lists;
use crate::members::{self, MemberBody, RoleBody, SetRoleRequest};
use crate::oidc;
use crate::people::{self, CreateTraitRequest, TraitBody, UpdateTraitRequest};
use crate::privacy::{self, erasures, exports};
use crate::problem::{
    FlowIssueProblem, ItemProblem, Problem, SegmentIssueProblem, SegmentRefProblem,
    TemplateIssueProblem,
};
use crate::retention;
use crate::segments::preview::{
    PreviewBody as SegmentPreviewBody, PreviewRequest as SegmentPreviewRequest,
    SampleBody as SegmentSampleBody,
};
use crate::segments::{self, SegmentSchemas};
use crate::sending;
use crate::session::{self, SessionBody, SignInRequest, UserBody};
use crate::setup::{self, SetupRequest, SetupStatus};
use crate::snippets;
use crate::sources;
use crate::templates;
use crate::themes;
use crate::traces::{self, CauseBody, DecisionBody, TraceBody};
use crate::users::{self, CreateUserRequest};
use crate::version::{self, VersionBody};
use crate::webhooks;
use crate::workspace_credentials;
use crate::workspaces::{
    self, CreateWorkspaceRequest, UpdateWorkspaceRequest, UtmSettingsBody, WorkspaceBody,
};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "zirv-flow API",
        // The API version, not the crate version, so a release that leaves
        // the API untouched does not make the committed document stale.
        version = "1",
        description = "HTTP API of the zirv-flow server."
    ),
    paths(
        health::healthz,
        health::readyz,
        version::version,
        session::sign_in,
        session::methods,
        session::current,
        session::sign_out,
        oidc::start,
        oidc::callback,
        users::list,
        users::create,
        workspaces::list,
        workspaces::create,
        workspaces::get,
        workspaces::update,
        members::list,
        members::set,
        members::remove,
        keys::list,
        keys::create,
        keys::revoke,
        assistant::settings::get_settings,
        assistant::settings::put_settings,
        assistant::settings::models,
        assistant::settings::usage,
        assistant::drafts::create,
        assistant::drafts::accept,
        assistant::explain::explain,
        assistant::flow_explain::explain_flow,
        assistant::flow_explain::explain_trace,
        people::registry::list,
        people::registry::create,
        people::registry::update,
        people::slots::index,
        people::slots::unindex,
        people::conflicts::list,
        people::conflicts::get,
        people::conflicts::resolve,
        people::search::search,
        people::profile::get,
        people::timeline::get,
        people::messages::list,
        events::schemas::list,
        events::schemas::create,
        events::schemas::get,
        events::schemas::put_draft,
        events::schemas::publish,
        events::schemas::update,
        events::policy::get,
        events::policy::put,
        events::rejects::list,
        events::replay::replay,
        events::debugger::stream,
        events::explorer::count,
        events::explorer::breakdown,
        events::explorer::list,
        events::explorer::export,
        contacts::definition::get,
        contacts::definition::put,
        contacts::crud::list,
        contacts::crud::create,
        contacts::crud::get,
        contacts::crud::replace,
        contacts::crud::patch,
        contacts::crud::remove,
        contacts::search::get,
        contacts::search::post,
        contacts::bulk::bulk,
        lists::crud::list,
        lists::crud::create,
        lists::crud::get,
        lists::crud::update,
        lists::crud::remove,
        lists::members::page,
        lists::members::add,
        lists::members::remove,
        lists::import::import,
        lists::person::lists,
        lists::person::history,
        flows::crud::list,
        flows::crud::create,
        flows::crud::get,
        flows::crud::update,
        flows::crud::validate,
        flows::crud::versions,
        flows::crud::version,
        flows::publish::publish,
        flows::publish::pause,
        flows::publish::resume,
        flows::publish::archive,
        flows::test_mode::test,
        flows::triggers::enroll_person,
        flows::triggers::start,
        flows::inspector::list,
        flows::inspector::get,
        flows::inspector::counts,
        flows::operator::retry,
        flows::operator::skip,
        flows::operator::exit_version,
        flows::signing::get,
        flows::signing::create,
        flows::signing::rotate,
        segments::crud::list,
        segments::crud::create,
        segments::crud::get,
        segments::crud::update,
        segments::crud::archive,
        segments::crud::versions,
        segments::crud::version,
        segments::members::members,
        segments::members::history,
        segments::members::refresh,
        segments::explain::explain,
        segments::preview::preview,
        snippets::crud::list,
        snippets::crud::create,
        snippets::crud::get,
        snippets::crud::update,
        snippets::crud::archive,
        snippets::crud::put_draft,
        snippets::preview::preview,
        snippets::publish::publish,
        snippets::publish::versions,
        snippets::publish::version,
        snippets::where_used::where_used,
        themes::crud::list,
        themes::crud::create,
        themes::crud::get,
        themes::crud::update,
        themes::crud::archive,
        themes::crud::put_draft,
        themes::default::put_default,
        themes::preview::preview,
        themes::publish::publish,
        themes::publish::versions,
        themes::publish::version,
        themes::where_used::where_used,
        templates::crud::list,
        templates::crud::create,
        templates::crud::get,
        templates::crud::update,
        templates::crud::archive,
        templates::crud::put_draft,
        templates::crud::convert_draft,
        templates::publish::publish,
        templates::publish::versions,
        templates::publish::version,
        templates::publish::version_stats,
        templates::where_used::where_used,
        templates::where_used::send_volume,
        templates::preview::preview,
        templates::preview_people::list,
        templates::preview_people::add,
        templates::preview_people::remove,
        crate::test_sends::send::test_send,
        crate::test_sends::send::list,
        crate::test_sends::send::quota,
        crate::test_sends::allowlist::allowlist_list,
        crate::test_sends::allowlist::allowlist_add,
        crate::test_sends::allowlist::allowlist_delete,
        crate::test_sends::allowlist::allowlist_options,
        destinations::crud::options,
        destinations::crud::list,
        destinations::crud::create,
        destinations::crud::get,
        destinations::crud::update,
        destinations::crud::remove,
        destinations::actions::check,
        destinations::domain_status::get,
        destinations::stats::get,
        destinations::kinds::list,
        forwarding::rules::list,
        forwarding::rules::create,
        forwarding::rules::get,
        forwarding::rules::update,
        forwarding::rules::remove,
        forwarding::queue::dead_letters,
        forwarding::queue::status,
        forwarding::queue::redrive,
        feedback::receive::receive,
        workspace_credentials::schema::list_kinds,
        workspace_credentials::crud::list,
        workspace_credentials::crud::create,
        workspace_credentials::crud::get,
        workspace_credentials::crud::update,
        workspace_credentials::crud::rotate,
        workspace_credentials::crud::remove,
        workspace_credentials::check::test,
        sending::domains::list,
        sending::domains::create,
        sending::domains::get,
        sending::domains::verify,
        sending::domains::remove,
        sending::identities::options,
        sending::identities::list,
        sending::identities::create,
        sending::identities::get,
        sending::identities::update,
        sending::identities::remove,
        sending::settings::get,
        sending::settings::put,
        sources::kinds::list,
        sources::crud::list,
        sources::crud::create,
        sources::crud::get,
        sources::crud::update,
        sources::crud::remove,
        sources::actions::test,
        sources::actions::discover,
        sources::streams::list,
        sources::streams::put,
        sources::preview::preview,
        sources::manifest::validate,
        sources::manifest::sample,
        sources::oauth::start,
        sources::oauth::callback,
        sources::runs::start,
        sources::runs::cancel,
        sources::runs::list,
        sources::runs::get,
        assets::upload::upload,
        assets::crud::list,
        assets::crud::get,
        assets::crud::remove,
        imports::upload::upload,
        imports::crud::list,
        imports::crud::get,
        imports::crud::put_mapping,
        imports::crud::dry_run,
        imports::crud::run,
        imports::crud::cancel,
        imports::rejects::list,
        imports::rejects::download,
        topics::list,
        topics::create,
        topics::update,
        dimensions::list,
        dimensions::create,
        dimensions::update,
        dimensions::add_value_handler,
        dimensions::update_value_handler,
        policy::get_draft,
        policy::put_draft,
        policy::publish,
        policy::versions,
        policy::evaluate,
        transfer::import_handler,
        transfer::export,
        person::get,
        person::set,
        person::history,
        suppressions::list,
        suppressions::check,
        suppressions::add,
        suppressions::remove_handler,
        suppressions::import_handler,
        suppressions::export,
        ingest::identify,
        ingest::track,
        ingest::batch,
        webhooks::receive::receive,
        webhooks::ops::list,
        webhooks::ops::create,
        webhooks::ops::get,
        webhooks::ops::update,
        webhooks::ops::rotate,
        webhooks::ops::remove,
        traces::get,
        setup::status,
        setup::complete,
        audit::list,
        audit::list_instance,
        audit::actions,
        audit::export::workspace,
        audit::export::instance,
        jobs_admin::depth,
        jobs_admin::dead,
        jobs_admin::retry,
        privacy::person::remove,
        erasures::create,
        erasures::list,
        exports::create,
        exports::list,
        exports::download_handler,
        privacy::settings::get,
        privacy::settings::put,
        retention::get,
        retention::put,
    ),
    components(schemas(
        Problem,
        ItemProblem,
        AcceptedBody,
        IdentifyRequest,
        TrackRequest,
        BatchRequest,
        BatchItemRequest,
        StatusBody,
        VersionBody,
        SignInRequest,
        SessionBody,
        session::MethodsBody,
        UserBody,
        CreateUserRequest,
        CreateWorkspaceRequest,
        UpdateWorkspaceRequest,
        WorkspaceBody,
        UtmSettingsBody,
        MemberBody,
        SetRoleRequest,
        RoleBody,
        ApiKeyBody,
        CreatedApiKeyBody,
        assistant::settings::AiSettingsBody,
        assistant::settings::PutAiSettingsRequest,
        assistant::settings::ProbeBody,
        assistant::settings::MissingBody,
        assistant::settings::AiModelsBody,
        assistant::settings::ProviderBody,
        assistant::settings::ModelBody,
        assistant::settings::AiUsageBody,
        assistant::settings::FeatureUsageBody,
        assistant::settings::UserUsageBody,
        assistant::settings::AiCallBody,
        assistant::drafts::DraftRequest,
        assistant::drafts::DraftKindBody,
        assistant::drafts::DraftBody,
        assistant::drafts::DraftIssue,
        assistant::drafts::DryRunBody,
        assistant::drafts::DryRunProblem,
        assistant::drafts::AcceptedDraftBody,
        assistant::drafts::RenderedEmailBody,
        assistant::drafts::TemplateRendersBody,
        assistant::tools::draft_template::TemplateTask,
        assistant::explain::AiExplainRequest,
        assistant::explain::AiExplainBody,
        assistant::explain::AiExplainedNode,
        assistant::flow_explain::AiFlowExplainRequest,
        assistant::flow_explain::AiFlowExplainBody,
        assistant::flow_explain::AiExplainedFlowNode,
        assistant::flow_explain::AiTraceExplainBody,
        assistant::flow_explain::AiExplainedRecord,
        assistant::diff::Change,
        assistant::diff::ChangeKind,
        CreateApiKeyRequest,
        TraitBody,
        people::slots::IndexBody,
        CreateTraitRequest,
        UpdateTraitRequest,
        people::conflicts::ConflictBody,
        people::conflicts::ConflictsBody,
        people::conflicts::ConflictDetailBody,
        people::conflicts::PersonIdentitiesBody,
        people::conflicts::IdentityBody,
        people::conflicts::ResolveRequest,
        people::search::SearchRequest,
        people::search::IdentifierQuery,
        people::search::TraitQuery,
        people::search::SearchBody,
        people::search::PersonHitBody,
        people::search::DisplayBody,
        people::profile::ProfileBody,
        people::profile::ProfileIdentityBody,
        people::profile::IdentityGroupBody,
        people::profile::ProfileTraitBody,
        people::profile::MergeBody,
        people::messages::MessagesBody,
        people::messages::MessageBody,
        people::messages::MessageTemplateBody,
        people::messages::MessageFlowBody,
        people::messages::MessageNodeBody,
        people::messages::MessageDestinationBody,
        people::messages::GateResultBody,
        people::timeline::TimelineBody,
        people::timeline::TimelineEntryBody,
        people::timeline::EventEntryBody,
        people::timeline::DecisionEntryBody,
        PropertyBody,
        EventSchemaBody,
        EventSchemaDetailBody,
        SchemaVersionBody,
        CreateEventSchemaRequest,
        SaveDraftRequest,
        UpdateEventSchemaRequest,
        EventPolicyBody,
        RedactionBody,
        IngestRejectBody,
        IngestRejectsBody,
        ReasonBody,
        events::replay::ReplayRequest,
        events::replay::ReplayFilter,
        events::replay::ReplayBody,
        events::replay::PredictionBody,
        events::replay::TotalsBody,
        events::debugger::DebuggerItemBody,
        events::debugger::ReasonItem,
        events::explorer::ExploreFilterRequest,
        events::explorer::ExploreScopeRequest,
        events::explorer::ExploreCountRequest,
        events::explorer::ExploreCountBody,
        events::explorer::BucketBody,
        events::explorer::ExploreBreakdownRequest,
        events::explorer::ExploreBreakdownBody,
        events::explorer::ValueCountBody,
        events::explorer::ExploreListRequest,
        events::explorer::ExploreListBody,
        events::explorer::ExploredEventBody,
        contacts::ContactBody,
        contacts::crud::CreateContactRequest,
        contacts::crud::ContactFieldsRequest,
        contacts::crud::ContactsBody,
        contacts::definition::ContactFieldBody,
        contacts::definition::ContactDefinitionBody,
        contacts::definition::PutContactDefinitionRequest,
        contacts::search::ContactIdentifierQuery,
        contacts::search::SearchContactsRequest,
        contacts::search::SearchContactsBody,
        contacts::bulk::BulkContactItem,
        contacts::bulk::BulkContactsRequest,
        contacts::bulk::BulkAcceptedBody,
        lists::crud::ListBody,
        lists::crud::CreateListRequest,
        lists::crud::UpdateListRequest,
        segments::explain::ExplainBody,
        segments::explain::ExplainNodeBody,
        segments::explain::ObservedBody,
        segments::explain::OutcomeBody,
        flows::crud::FlowBody,
        flows::crud::FlowDetailBody,
        flows::crud::FlowVersionBody,
        flows::crud::CreateFlowRequest,
        flows::crud::UpdateFlowRequest,
        flows::crud::ValidationBody,
        flows::publish::ImpactBody,
        flows::publish::PublishedBody,
        flows::triggers::EnrollPersonRequest,
        flows::triggers::PersonIdentifier,
        flows::triggers::QueuedBody,
        flows::inspector::EnrollmentItemBody,
        flows::inspector::EnrollmentsBody,
        flows::inspector::StepBody,
        flows::inspector::EnrollmentWaitBody,
        flows::inspector::EnrollmentPathBody,
        flows::inspector::NodeCountBody,
        flows::inspector::SkippedCountBody,
        flows::inspector::VersionCountsBody,
        traces::CauseKindBody,
        flows::operator::EnrollmentActionBody,
        flows::operator::ExitVersionBody,
        flows::triggers::StartFlowRequest,
        flows::publish::AudienceBody,
        flows::test_mode::body::FlowTestRequest,
        flows::test_mode::body::ScriptStepBody,
        flows::test_mode::body::InjectBody,
        flows::test_mode::body::SnapshotBody,
        flows::test_mode::body::SnapshotEventBody,
        flows::test_mode::body::SnapshotConsentBody,
        flows::test_mode::body::FlowTestBody,
        flows::test_mode::simulation::SimulationBody,
        flows::test_mode::simulation::SimulatedPersonBody,
        flows::test_mode::body::PathStepBody,
        flows::test_mode::body::ConditionBody,
        flows::test_mode::body::FlowObservedBody,
        flows::test_mode::body::EffectBody,
        flows::test_mode::body::WaitBody,
        flows::test_mode::body::WakeBody,
        flows::test_mode::body::ScriptResultBody,
        flows::test_mode::body::FinalStateBody,
        flows::test_mode::body::HaltBody,
        flows::test_mode::body::PersonAfterBody,
        flows::publish::UnhealthyInputBody,
        flows::signing::SigningSecretBody,
        flows::signing::IssuedSigningSecretBody,
        FlowIssueProblem,
        TemplateIssueProblem,
        segments::crud::SegmentBody,
        segments::crud::SegmentVersionBody,
        segments::crud::SegmentDetailBody,
        segments::crud::CreateSegmentRequest,
        segments::crud::UpdateSegmentRequest,
        segments::members::SegmentMembersBody,
        segments::members::SegmentMemberBody,
        segments::members::SegmentHistoryBody,
        segments::members::SegmentHistoryItemBody,
        segments::members::RefreshQueuedBody,
        SegmentPreviewRequest,
        SegmentPreviewBody,
        SegmentSampleBody,
        segments::preview::DataAgeBody,
        segments::preview::DataSourceBody,
        snippets::crud::SnippetBody,
        snippets::crud::SnippetListItemBody,
        snippets::crud::SnippetDraftBody,
        snippets::crud::SnippetDetailBody,
        snippets::crud::CreateSnippetRequest,
        snippets::crud::UpdateSnippetRequest,
        snippets::crud::SaveSnippetDraftRequest,
        snippets::publish::SnippetVersionBody,
        snippets::publish::SnippetVersionSummaryBody,
        snippets::publish::SnippetPublishedBody,
        snippets::where_used::SnippetUsedByBody,
        snippets::where_used::SnippetTemplateBody,
        snippets::where_used::SnippetThemeBody,
        snippets::where_used::SnippetFlowBody,
        snippets::preview::SnippetPreviewRequest,
        snippets::preview::SnippetPreviewBody,
        themes::crud::ThemeBody,
        themes::crud::ThemeListItemBody,
        themes::crud::ThemeDraftBody,
        themes::crud::ThemeDetailBody,
        themes::crud::CreateThemeRequest,
        themes::crud::UpdateThemeRequest,
        themes::crud::SaveThemeDraftRequest,
        themes::default::SetDefaultThemeRequest,
        themes::publish::ThemeVersionBody,
        themes::publish::ThemeVersionSummaryBody,
        themes::publish::ThemePublishedBody,
        themes::where_used::ThemeUsedByBody,
        themes::where_used::ThemeTemplateBody,
        themes::where_used::ThemeFlowBody,
        themes::preview::ThemePreviewRequest,
        themes::preview::ThemePreviewBody,
        templates::crud::TemplateBody,
        templates::crud::TemplateListItemBody,
        templates::crud::TemplateChecksBody,
        templates::crud::TemplateThemeBody,
        templates::crud::TemplateThemeChoice,
        templates::crud::TemplateNamedTheme,
        templates::crud::TemplateDraftBody,
        templates::crud::TemplateDetailBody,
        templates::crud::CreateTemplateRequest,
        templates::crud::UpdateTemplateRequest,
        templates::crud::TemplateUtmRequest,
        templates::crud::SaveTemplateDraftRequest,
        templates::crud::TemplateChannelRequest,
        templates::crud::TemplateCategoryRequest,
        templates::publish::TemplateVersionBody,
        templates::publish::TemplateVersionSummaryBody,
        templates::publish::TemplateVersionStatsBody,
        templates::publish::VersionRenderBody,
        templates::publish::TemplatePublishedBody,
        templates::where_used::TemplateUseBody,
        templates::where_used::TemplateSendVolumeBody,
        templates::publish::TemplatePublishRequest,
        templates::preview::TemplatePreviewRequest,
        templates::preview::TemplatePreviewBody,
        templates::preview_people::PreviewPersonBody,
        templates::preview_people::AddPreviewPersonRequest,
        crate::test_sends::TestSendBody,
        crate::test_sends::TestSendRequest,
        crate::test_sends::TestSendRecipient,
        crate::test_sends::TestSendsBody,
        crate::test_sends::QuotaBody,
        crate::test_sends::QuotaUsageBody,
        crate::test_sends::AllowlistBody,
        crate::test_sends::AllowlistOptionBody,
        crate::test_sends::AllowlistEntryBody,
        crate::test_sends::AllowlistAddRequest,
        templates::preview::TemplateHeaderBody,
        SegmentIssueProblem,
        SegmentRefProblem,
        lists::members::ListMembersBody,
        lists::members::ListMemberBody,
        lists::members::MemberIdentifierBody,
        lists::members::AddMembersRequest,
        lists::members::IdentifierInput,
        lists::members::AddMembersBody,
        lists::members::RemoveMembersRequest,
        lists::members::RemoveMembersBody,
        lists::members::NotFoundBody,
        lists::import::ImportBody,
        lists::import::UnmatchedRow,
        lists::person::PersonListBody,
        lists::person::MembershipHistoryBody,
        lists::person::MembershipChangeBody,
        forwarding::rules::RuleBody,
        forwarding::rules::CreateRuleRequest,
        forwarding::rules::UpdateRuleRequest,
        forwarding::queue::DeadLetterBody,
        forwarding::queue::DeadLettersBody,
        forwarding::queue::QueueStatusBody,
        forwarding::queue::ForwardingStatusBody,
        forwarding::queue::RedriveRequest,
        forwarding::queue::RedriveBody,
        destinations::crud::DestinationBody,
        destinations::crud::DestinationAbilitiesBody,
        destinations::crud::DestinationOptionBody,
        destinations::crud::CreateDestinationRequest,
        destinations::crud::UpdateDestinationRequest,
        destinations::actions::DestinationCheckBody,
        destinations::domain_status::DomainStatusBody,
        destinations::stats::DestinationStatsBody,
        destinations::stats::OutcomeCountsBody,
        destinations::stats::StatusClassCountsBody,
        destinations::kinds::DestinationKindBody,
        destinations::kinds::DestinationCapabilitiesBody,
        destinations::domain_status::DomainStatusRecordBody,
        destinations::domain_status::ProviderRecordBody,
        destinations::domain_status::ProviderOnlyRecordBody,
        feedback::receive::FeedbackAccepted,
        workspace_credentials::schema::KindBody,
        workspace_credentials::schema::SchemaBody,
        workspace_credentials::schema::SchemaFieldBody,
        workspace_credentials::schema::FieldTypeBody,
        workspace_credentials::crud::CredentialBody,
        workspace_credentials::crud::UsedByBody,
        workspace_credentials::crud::CreateCredentialRequest,
        workspace_credentials::crud::UpdateCredentialRequest,
        workspace_credentials::crud::RotateCredentialRequest,
        workspace_credentials::check::TestCredentialRequest,
        workspace_credentials::check::CredentialTestBody,
        sending::domains::SendingDomainBody,
        sending::domains::DomainRecordBody,
        sending::domains::CreateSendingDomainRequest,
        sending::identities::SenderIdentityBody,
        sending::identities::SenderIdentityOptionBody,
        sending::identities::CreateSenderIdentityRequest,
        sending::identities::UpdateSenderIdentityRequest,
        sending::settings::SendSettingsBody,
        sending::settings::FrequencyCapBody,
        sending::settings::QuietHoursBody,
        sources::kinds::SourceKindBody,
        sources::kinds::FieldSpecBody,
        sources::kinds::CapabilitiesBody,
        sources::crud::SourceBody,
        webhooks::ops::WebhookBody,
        webhooks::ops::CreatedWebhookBody,
        webhooks::ops::RotatedSecretBody,
        webhooks::ops::CreateWebhookRequest,
        webhooks::ops::UpdateWebhookRequest,
        webhooks::receive::WebhookAccepted,
        sources::crud::CreateSourceRequest,
        sources::crud::UpdateSourceRequest,
        sources::actions::TestBody,
        sources::streams::StreamBody,
        sources::streams::DescriptorBody,
        sources::streams::FieldBody,
        sources::streams::StreamSettingsRequest,
        sources::preview::PreviewRequest,
        sources::preview::PreviewBody,
        sources::preview::PreviewRecord,
        sources::preview::PreviewIdentifier,
        sources::preview::PreviewError,
        sources::preview::PreviewEvent,
        sources::manifest::ValidateRequest,
        sources::manifest::ValidateBody,
        sources::manifest::IssueBody,
        sources::manifest::ManifestStreamBody,
        sources::manifest::ManifestFieldBody,
        sources::manifest::SampleRequest,
        sources::manifest::SampleBody,
        sources::oauth::StartBody,
        sources::crud::OAuthBody,
        sources::runs::RunBody,
        sources::runs::RunsBody,
        sources::runs::RunDetailBody,
        sources::runs::RunRejectBody,
        sources::runs::RunRejectReasonBody,
        sources::runs::StartRunRequest,
        sources::runs::RunKindBody,
        assets::AssetBody,
        assets::AssetDetailBody,
        assets::AssetOwnerBody,
        assets::AssetsBody,
        imports::CsvImportBody,
        imports::CsvImportDetailBody,
        imports::crud::CsvImportsBody,
        imports::MappingBody,
        imports::EventRuleBody,
        imports::ValueRuleBody,
        imports::PropertyRuleBody,
        imports::ColumnRuleBody,
        imports::DeleteWhenBody,
        imports::DeleteRuleBody,
        imports::TargetBody,
        imports::TransformBody,
        imports::TraitTypeBody,
        imports::rejects::ImportRejectBody,
        imports::rejects::ImportRejectErrorBody,
        imports::rejects::ImportRejectsBody,
        topics::TopicBody,
        topics::CreateTopicRequest,
        topics::UpdateTopicRequest,
        dimensions::DimensionBody,
        dimensions::DimensionValueBody,
        dimensions::CreateDimensionRequest,
        dimensions::NewValueRequest,
        dimensions::UpdateDimensionRequest,
        policy::ConsentPolicyRuleBody,
        policy::ConsentPolicyRuleRequest,
        policy::SaveConsentPolicyDraftRequest,
        policy::ConsentPolicyDraftBody,
        policy::ConsentPolicyVersionBody,
        policy::EvaluateConsentPolicyRequest,
        policy::DecidingRuleBody,
        policy::ConsentPolicyEvaluationBody,
        transfer::ConsentImportBody,
        transfer::ConsentImportRowBody,
        person::ConsentBody,
        person::ReadBody,
        person::ConsentStateBody,
        person::SetConsentRequest,
        person::HistoryBody,
        person::HistoryEntryBody,
        suppressions::SuppressionBody,
        suppressions::SuppressionsBody,
        suppressions::IdentifierRequest,
        suppressions::AddSuppressionRequest,
        suppressions::AddedBody,
        suppressions::CheckBody,
        suppressions::SuppressionImportBody,
        suppressions::InvalidRowBody,
        TraceBody,
        DecisionBody,
        CauseBody,
        SetupStatus,
        SetupRequest,
        DepthBody,
        KindDepthBody,
        JobBody,
        DeadJobsBody,
        audit::AuditEntryBody,
        audit::AuditPageBody,
        audit::AuditAreaBody,
        audit::AuditActionsBody,
        privacy::PersonTarget,
        privacy::TargetIdentifier,
        erasures::ErasureBody,
        erasures::ErasureCallBody,
        erasures::ErasuresBody,
        exports::ExportBody,
        exports::ExportsBody,
        privacy::settings::PrivacySettingsBody,
        retention::RetentionBody,
        retention::WindowBody,
        retention::LastRunBody,
        retention::PutRetentionRequest,
    )),
    modifiers(&SecuritySchemes, &PreviewTag, &SegmentSchemas, &FlowSchemas)
)]
struct ApiDoc;

/// Declares how callers authenticate.
struct SecuritySchemes;

impl Modify for SecuritySchemes {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        let session = ApiKey::Cookie(ApiKeyValue::with_description(
            "zf_session",
            "Session cookie set by POST /api/v1/session. Every POST, PUT, PATCH and DELETE              made with it must also send the CSRF token in the x-csrf-token header.",
        ));
        components.add_security_scheme("session", SecurityScheme::ApiKey(session));
        let mut bearer = Http::new(HttpAuthScheme::Bearer);
        bearer.description = Some(
            "Workspace API key (zfk_...). Valid only for its own workspace and within its scopes."
                .into(),
        );
        components.add_security_scheme("api_key", SecurityScheme::Http(bearer));
    }
}

/// The tag of every operation the policy leaves unfrozen (docs/api.md, "Unfrozen surface").
const PREVIEW_TAG: &str = "preview";

/// Whether the policy lists an operation at this path as preview: the job queue tooling, the
/// inbound webhook, the assistant and the event explorer.
fn is_preview(path: &str) -> bool {
    path.starts_with("/api/v1/admin/jobs")
        || path == "/hooks/{id}"
        || path.contains("/ai/")
        || path.contains("/events/explore/")
}

/// Adds the `preview` tag to the operations the policy lists as unfrozen, so a client can tell
/// them apart in the document.
struct PreviewTag;

impl Modify for PreviewTag {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        for (path, item) in openapi.paths.paths.iter_mut() {
            if !is_preview(path) {
                continue;
            }
            for operation in [
                &mut item.get,
                &mut item.put,
                &mut item.post,
                &mut item.delete,
                &mut item.patch,
            ]
            .into_iter()
            .flatten()
            {
                let tags = operation.tags.get_or_insert_with(Vec::new);
                if !tags.iter().any(|tag| tag == PREVIEW_TAG) {
                    tags.push(PREVIEW_TAG.to_owned());
                }
            }
        }
        let tags = openapi.tags.get_or_insert_with(Vec::new);
        tags.push(
            utoipa::openapi::tag::TagBuilder::new()
                .name(PREVIEW_TAG)
                .description(Some(
                    "Not frozen: may change in any release, 1.x included (the API contract policy lists them).",
                ))
                .build(),
        );
    }
}

/// The OpenAPI 3.1 description of the HTTP API.
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMITTED: &str = include_str!("../openapi.json");

    #[test]
    fn the_committed_openapi_document_matches_the_code() {
        let mut generated = openapi().to_pretty_json().unwrap();
        generated.push('\n');

        if std::env::var_os("ZIRV_FLOW_BLESS").is_some() {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/openapi.json");
            std::fs::write(path, &generated).unwrap();
            return;
        }

        assert!(
            COMMITTED.replace("\r\n", "\n") == generated,
            "openapi.json is stale; regenerate with ZIRV_FLOW_BLESS=1 cargo nextest run -p zirv-flow-api openapi"
        );
    }

    fn check_version_and_paths(document: &serde_json::Value) -> Result<(), String> {
        if document["info"]["version"].as_str() != Some("1") {
            return Err("OpenAPI info.version must stay 1".into());
        }
        const UNVERSIONED: &[&str] = &[
            "/hooks/{id}", // Public inbound webhook: HMAC contract and WebhookPanel types.
            "/healthz",    // Unversioned liveness probe.
            "/readyz",     // Unversioned readiness probe.
        ];
        let paths = document["paths"].as_object().unwrap();
        for path in UNVERSIONED {
            if !paths.contains_key(*path) {
                return Err(format!(
                    "OpenAPI allowlist path {path} is no longer documented"
                ));
            }
        }
        for path in paths.keys() {
            if !path.starts_with("/api/v1/") && !UNVERSIONED.contains(&path.as_str()) {
                return Err(format!("OpenAPI path {path} must start with /api/v1/"));
            }
        }
        Ok(())
    }

    #[test]
    fn openapi_version_and_paths_stay_on_v1() {
        let document = serde_json::to_value(openapi()).unwrap();
        check_version_and_paths(&document).unwrap();
        // /t and /u are public tracking/unsubscribe, /a serves assets, and
        // /__test is test-only: these are not part of the generated API client.
        for prefix in ["/t", "/u", "/a", "/__test"] {
            for path in document["paths"].as_object().unwrap().keys() {
                assert!(
                    path != prefix && !path.starts_with(&format!("{prefix}/")),
                    "OpenAPI must not document {path}"
                );
            }
        }
    }

    #[test]
    fn openapi_version_and_paths_reject_an_incompatible_document() {
        let document = serde_json::to_value(openapi()).unwrap();
        let mut wrong_version = document.clone();
        wrong_version["info"]["version"] = serde_json::json!("2");
        assert_eq!(
            check_version_and_paths(&wrong_version),
            Err("OpenAPI info.version must stay 1".into())
        );
        let mut stale_allowlist = document.clone();
        stale_allowlist["paths"]
            .as_object_mut()
            .unwrap()
            .remove("/readyz");
        assert_eq!(
            check_version_and_paths(&stale_allowlist),
            Err("OpenAPI allowlist path /readyz is no longer documented".into())
        );
        for path in [
            "/v2/people",
            "/t/open",
            "/u/optout",
            "/a/file",
            "/__test/reset",
        ] {
            let mut wrong_path = document.clone();
            wrong_path["paths"][path] = serde_json::json!({});
            assert_eq!(
                check_version_and_paths(&wrong_path),
                Err(format!("OpenAPI path {path} must start with /api/v1/"))
            );
        }
    }

    #[test]
    fn no_two_registered_schemas_share_a_name() {
        let source = include_str!("openapi.rs");
        let list = source
            .split("schemas(")
            .nth(1)
            .and_then(|rest| rest.split("\n    ),").next())
            .expect("the schemas list");
        let mut seen = std::collections::BTreeMap::<&str, &str>::new();
        for entry in list.lines().map(str::trim) {
            let Some(path) = entry.strip_suffix(',') else {
                continue;
            };
            if path.is_empty()
                || !path
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
            {
                continue;
            }
            let name = path.rsplit("::").next().unwrap();
            if let Some(first) = seen.insert(name, path) {
                panic!("schema {name} is registered by {first} and {path}");
            }
        }
        assert!(seen.len() > 100, "the list was not read");
    }

    #[test]
    fn preview_schemas_match_their_registered_types() {
        fn check<T: utoipa::ToSchema>(document: &serde_json::Value) {
            let name = T::name();
            assert_eq!(
                document["components"]["schemas"][name.as_ref()],
                serde_json::to_value(T::schema()).unwrap(),
                "schema {name} was overwritten",
            );
        }

        let document = serde_json::to_value(openapi()).unwrap();
        check::<segments::preview::PreviewRequest>(&document);
        check::<segments::preview::PreviewBody>(&document);
        check::<segments::preview::SampleBody>(&document);
        check::<sources::preview::PreviewRequest>(&document);
        check::<sources::preview::PreviewBody>(&document);
        check::<sources::manifest::SampleBody>(&document);
    }

    const POLICY: &str = include_str!("../../../docs/api.md");

    fn policy() -> String {
        POLICY.replace("\r\n", "\n")
    }

    /// The text of a `##` section of the policy, without its heading.
    fn section(policy: &str, heading: &str) -> String {
        let start = policy
            .find(&format!("\n## {heading}\n"))
            .unwrap_or_else(|| panic!("the policy has no section {heading}"));
        let rest = &policy[start + 1..];
        let body = &rest[rest.find('\n').unwrap()..];
        body.split("\n## ").next().unwrap().to_owned()
    }

    fn operations(document: &serde_json::Value) -> Vec<(String, String, &serde_json::Value)> {
        let mut found = Vec::new();
        for (path, item) in document["paths"].as_object().unwrap() {
            for (method, operation) in item.as_object().unwrap() {
                found.push((method.clone(), path.clone(), operation));
            }
        }
        found
    }

    #[test]
    fn every_operation_has_a_security_entry() {
        let document = serde_json::to_value(openapi()).unwrap();
        let all = operations(&document);
        assert!(all.len() > 250, "the document was not read");
        for (method, path, operation) in all {
            assert!(
                operation["security"].is_array(),
                "{method} {path} has no security entry; a public operation says security(())"
            );
        }
    }

    /// The list operations whose body is a bare array, by design (docs/api.md, "Lists and
    /// pagination").
    const BOUNDED_COLLECTIONS: &[&str] = &[
        "list_destination_kinds",
        "list_source_kinds",
        "list_users",
        "list_workspaces",
        "list_api_keys",
        "list_consent_dimensions",
        "list_consent_policy_versions",
        "list_credential_kinds",
        "list_credentials",
        "list_destinations",
        "list_destination_options",
        "list_event_schemas",
        "list_flows",
        "list_flow_versions",
        "list_forwarding_rules",
        "list_lists",
        "list_members",
        "list_person_lists",
        "list_segments",
        "list_segment_versions",
        "list_sender_identities",
        "list_sender_identity_options",
        "list_sending_domains",
        "list_snippets",
        "list_snippet_versions",
        "list_sources",
        "list_source_streams",
        "list_templates",
        "list_template_preview_people",
        "list_template_versions",
        "list_test_send_allowlist_options",
        "list_themes",
        "list_theme_versions",
        "list_topics",
        "list_traits",
        "list_webhooks",
    ];
    /// The list operations that answer reference data, not rows.
    const EXEMPT_SHAPES: &[&str] = &[
        "list_ai_models",
        "list_audit_actions",
        "list_test_send_allowlist",
    ];

    fn resolve<'a>(
        document: &'a serde_json::Value,
        schema: &'a serde_json::Value,
    ) -> &'a serde_json::Value {
        match schema["$ref"].as_str() {
            Some(reference) => {
                &document["components"]["schemas"][reference.rsplit('/').next().unwrap()]
            }
            None => schema,
        }
    }

    #[test]
    fn every_list_operation_is_paged_the_shared_way_or_documented_as_not() {
        let document = serde_json::to_value(openapi()).unwrap();
        let policy = section(&policy(), "Lists and pagination");
        let mut paged = 0;
        let mut seen = Vec::new();
        for (method, path, operation) in operations(&document) {
            let Some(id) = operation["operationId"].as_str() else {
                continue;
            };
            if method != "get" || !id.starts_with("list_") {
                continue;
            }
            seen.push(id.to_owned());
            let schema = &operation["responses"]["200"]["content"]["application/json"]["schema"];
            let body = resolve(&document, schema);
            if body["type"] == "array" {
                assert!(
                    BOUNDED_COLLECTIONS.contains(&id),
                    "{id} ({path}) is an unpaged array but not a documented bounded collection"
                );
            } else if body["properties"]["next_cursor"].is_object() {
                let query: Vec<&str> = operation["parameters"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|parameter| parameter["in"] == "query")
                    .filter_map(|parameter| parameter["name"].as_str())
                    .collect();
                assert!(
                    query.contains(&"limit") && query.contains(&"cursor"),
                    "{id} must declare limit and cursor"
                );
                paged += 1;
            } else {
                assert!(
                    EXEMPT_SHAPES.contains(&id),
                    "{id} ({path}) is neither paged nor a documented exempt shape"
                );
            }
        }
        assert!(paged >= 20, "the paged lists were not found");
        for id in BOUNDED_COLLECTIONS.iter().chain(EXEMPT_SHAPES) {
            assert!(
                seen.iter().any(|seen| seen == id),
                "{id} is listed but is not a list operation"
            );
            assert!(
                policy.contains(&format!("`{id}`")),
                "{id} is missing from the policy"
            );
        }
    }

    #[test]
    fn no_operation_still_pages_with_before_or_after() {
        let document = serde_json::to_value(openapi()).unwrap();
        let text = document.to_string();
        for old in ["next_before", "next_after"] {
            assert!(!text.contains(old), "{old} is still in the document");
        }
        for (method, path, operation) in operations(&document) {
            for parameter in operation["parameters"].as_array().into_iter().flatten() {
                let name = parameter["name"].as_str().unwrap_or_default();
                assert!(
                    name != "before" && name != "after",
                    "{method} {path} takes {name}"
                );
            }
        }
    }

    /// The places in `text` that page with a removed name: `before` or `after` as a query key
    /// (`?before`, `&after=`) and the old response fields. `names` is false for a text that
    /// tells the history of the rename and so mentions the old response fields.
    fn stale_paging_names(text: &str, names: bool) -> Vec<String> {
        let mut found = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let bytes = line.as_bytes();
            for word in ["before", "after"] {
                for (at, _) in line.match_indices(word) {
                    let keyed = at > 0 && matches!(bytes[at - 1], b'?' | b'&');
                    let end = at + word.len();
                    let whole = bytes
                        .get(end)
                        .is_none_or(|next| !next.is_ascii_alphanumeric() && *next != b'_');
                    if keyed && whole {
                        found.push(format!("line {}: {line}", number + 1));
                    }
                }
            }
            if names && (line.contains("next_before") || line.contains("next_after")) {
                found.push(format!("line {}: {line}", number + 1));
            }
        }
        found
    }

    #[test]
    fn the_docs_do_not_page_with_the_removed_names() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|extension| extension == "md") {
                    out.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = vec![root.join("README.md")];
        walk(&root.join("docs"), &mut files);
        assert!(files.len() > 5, "the docs were not found");
        for file in files {
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&file).unwrap();
            let stale = stale_paging_names(&text, name != "api.md");
            assert!(
                stale.is_empty(),
                "{name} pages with a removed name: {stale:?}"
            );
        }
    }

    #[test]
    fn the_docs_scan_flags_a_removed_paging_name_and_not_prose() {
        for stale in [
            "`GET .../suppressions?limit&before&reason` lists",
            "curl \"$BASE/contacts?limit=50&after=<next>\"",
            "`?before=x`",
            "the field next_before is set",
        ] {
            assert!(!stale_paging_names(stale, true).is_empty(), "{stale}");
        }
        for prose in [
            "the draft is saved before the segment is counted",
            "a pause after the first step",
            "rows after the cursor, `?limit&cursor&kinds`",
            "an `after_id` field and `&before_x`",
        ] {
            assert!(stale_paging_names(prose, true).is_empty(), "{prose}");
        }
        assert!(stale_paging_names("they were next_before", false).is_empty());
    }

    /// The calls that carry a problem code as a string literal: the text of the call and the
    /// position of the literal among its arguments. Besides `.with_code(..)` they are the local
    /// helpers that build a problem from a code: `conflict(code, detail)` in `flows/signing.rs`,
    /// `breaks_templates(code, ..)` in `problem.rs`, `refused(field, code)` in
    /// `consent/suppressions.rs` and the `AdminError::Utm` variant.
    const CODE_CALLS: [(&str, usize); 8] = [
        ("with_code(", 0),
        ("code: Some(", 0),
        ("source_fields_invalid(", 0),
        ("flow_test_refused(", 0),
        ("breaks_templates(", 0),
        ("conflict(", 0),
        ("refused(", 1),
        ("AdminError::Utm { code: ", 0),
    ];

    /// The snake_case string literals in the code-carrying position of a call in `source`.
    fn literals_in(source: &str) -> std::collections::BTreeSet<String> {
        let source = source.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut codes = std::collections::BTreeSet::new();
        for (marker, position) in CODE_CALLS {
            for (at, _) in source.match_indices(marker) {
                if source[..at].ends_with("fn ") {
                    continue;
                }
                let mut rest = source[at + marker.len()..].trim_start();
                if position == 1 {
                    let Some((first, after)) = rest.split_once(',') else {
                        continue;
                    };
                    if first.contains(['(', ')']) {
                        continue;
                    }
                    rest = after.trim_start();
                }
                let Some(rest) = rest.strip_prefix('"') else {
                    continue;
                };
                let code = rest.split('"').next().unwrap();
                if !code.is_empty()
                    && code
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                {
                    codes.insert(code.to_owned());
                }
            }
        }
        codes
    }

    /// Every problem code literal in the non-test source of the crate.
    fn coded_literals() -> std::collections::BTreeSet<String> {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let mut codes = std::collections::BTreeSet::new();
        for file in files {
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            if name.contains("acceptance") || name == "test_support.rs" {
                continue;
            }
            let source = std::fs::read_to_string(&file)
                .unwrap()
                .replace("\r\n", "\n");
            let source = source.split("\n#[cfg(test)]\nmod tests").next().unwrap();
            codes.extend(literals_in(source));
        }
        codes
    }

    #[test]
    fn a_code_passed_through_a_local_wrapper_is_found_and_missing_from_the_policy_fails() {
        let source = r#"
            fn conflict(code: &'static str, detail: &'static str) -> ApiError {
                ApiError::new(StatusCode::CONFLICT, detail).with_code(code)
            }
            Err(conflict(
                "zz_wrapper_code_not_listed",
                "detail",
            ))
            Err(refused("note", "zz_second_argument_code"))
            AdminError::Utm {
                code: "zz_utm_code",
                detail,
            }
        "#;
        let found = literals_in(source);
        for code in [
            "zz_wrapper_code_not_listed",
            "zz_second_argument_code",
            "zz_utm_code",
        ] {
            assert!(found.contains(code), "{code} was not found");
        }
        assert_eq!(found.len(), 3, "{found:?}");
        let listed = listed_codes(&policy());
        assert!(found.iter().all(|code| !listed.contains(code)));
        // The real sources hold the wrapper codes, and the policy lists them.
        let real = coded_literals();
        for code in [
            "signing_secret_missing",
            "signing_secret_exists",
            "snippet_breaks_templates",
            "utm_pattern_invalid",
        ] {
            assert!(real.contains(code), "{code} was not found in the sources");
            assert!(
                listed.iter().any(|listed| listed == code),
                "{code} is not listed"
            );
        }
    }

    /// The codes whose literal is not next to a marker: set from a value of another crate, or
    /// by the generic fallback of a status.
    fn computed_codes() -> Vec<&'static str> {
        use zirv_flow_core::csv_file::CsvError;
        use zirv_flow_core::mapping::MappingError;
        let column = || "c".to_owned();
        let mut codes = vec![
            CsvError::NotUtf8.code(),
            CsvError::RecordTooLarge.code(),
            CsvError::TooManyFields.code(),
            CsvError::UnterminatedQuote.code(),
            MappingError::NoIdentifier.code(),
            MappingError::TooManyRules.code(),
            MappingError::UnknownColumn(column()).code(),
            MappingError::InvalidIdentifierKind(column()).code(),
            MappingError::InvalidTraitKey(column()).code(),
            MappingError::UnknownTrait(column()).code(),
            MappingError::ArchivedTrait(column()).code(),
            MappingError::DuplicateTrait(column()).code(),
            MappingError::InvalidTransform(column()).code(),
            MappingError::InvalidEventName.code(),
            MappingError::ReservedEventName.code(),
            MappingError::InvalidEventProperty(column()).code(),
            MappingError::DuplicateEventProperty(column()).code(),
            MappingError::TooManyEventProperties.code(),
        ];
        codes.extend([
            "utm_pattern_invalid",
            "utm_domain_invalid",
            "utm_domains_required",
        ]);
        codes.extend(crate::problem::FALLBACK_CODES.iter().map(|(_, code)| *code));
        codes
    }

    fn listed_codes(policy: &str) -> Vec<String> {
        section(policy, "Problem codes")
            .lines()
            .filter_map(|line| line.strip_prefix("| `"))
            .filter_map(|rest| rest.split('`').next())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn every_problem_code_in_the_sources_is_in_the_policy_list() {
        let listed = listed_codes(&policy());
        assert!(listed.len() > 100, "the code list was not read");
        let mut missing: Vec<String> = coded_literals()
            .into_iter()
            .filter(|code| !listed.contains(code))
            .collect();
        missing.extend(
            computed_codes()
                .into_iter()
                .filter(|code| !listed.iter().any(|listed| listed == code))
                .map(str::to_owned),
        );
        assert!(
            missing.is_empty(),
            "codes missing from docs/api.md: {missing:?}"
        );
    }

    #[test]
    fn every_policy_code_is_snake_case_and_listed_once() {
        let mut seen = std::collections::BTreeSet::new();
        for code in listed_codes(&policy()) {
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{code}"
            );
            assert!(seen.insert(code.clone()), "{code} is listed twice");
        }
    }

    #[test]
    fn the_preview_tag_is_on_exactly_the_operations_the_policy_leaves_unfrozen() {
        let document = serde_json::to_value(openapi()).unwrap();
        let unfrozen = section(&policy(), "Unfrozen surface");
        for needle in [
            "/hooks/{id}",
            "/api/v1/admin/jobs",
            "/ai/",
            "/events/explore/",
            "preview",
        ] {
            assert!(
                unfrozen.contains(needle),
                "{needle} is missing from the unfrozen list"
            );
        }
        const RULED: [(&str, &str); 17] = [
            ("get", "/api/v1/admin/jobs"),
            ("get", "/api/v1/admin/jobs/dead"),
            ("post", "/api/v1/admin/jobs/{id}/retry"),
            ("post", "/api/v1/workspaces/{ws}/ai/drafts"),
            ("post", "/api/v1/workspaces/{ws}/ai/drafts/{id}/accept"),
            ("get", "/api/v1/workspaces/{ws}/ai/models"),
            ("get", "/api/v1/workspaces/{ws}/ai/settings"),
            ("put", "/api/v1/workspaces/{ws}/ai/settings"),
            ("get", "/api/v1/workspaces/{ws}/ai/usage"),
            ("post", "/api/v1/workspaces/{ws}/events/explore/breakdown"),
            ("post", "/api/v1/workspaces/{ws}/events/explore/count"),
            ("post", "/api/v1/workspaces/{ws}/events/explore/export"),
            ("post", "/api/v1/workspaces/{ws}/events/explore/list"),
            ("post", "/api/v1/workspaces/{ws}/flows/{id}/ai/explain"),
            ("post", "/api/v1/workspaces/{ws}/segments/{id}/ai/explain"),
            (
                "post",
                "/api/v1/workspaces/{ws}/traces/{trace_id}/ai/explain",
            ),
            ("post", "/hooks/{id}"),
        ];
        let mut tagged: Vec<(String, String)> = operations(&document)
            .into_iter()
            .filter(|(_, _, operation)| {
                operation["tags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tag| tag == "preview")
            })
            .map(|(method, path, _)| (method, path))
            .collect();
        tagged.sort();
        let mut ruled: Vec<(String, String)> = RULED
            .iter()
            .map(|(method, path)| ((*method).to_owned(), (*path).to_owned()))
            .collect();
        ruled.sort();
        assert_eq!(tagged, ruled);
        assert!(
            document["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tag| tag["name"] == "preview")
        );
    }

    #[test]
    fn timestamps_the_policy_promises_are_date_time_strings() {
        let document = serde_json::to_value(openapi()).unwrap();
        let schemas = &document["components"]["schemas"];
        assert_eq!(
            schemas["GateResultBody"]["properties"]["run_at"]["format"],
            "date-time"
        );
        assert_eq!(
            schemas["PreviewEvent"]["properties"]["occurred_at"]["format"],
            "date-time"
        );
    }

    #[test]
    fn the_probes_and_the_version_are_public_operations_with_an_empty_security_entry() {
        let document = serde_json::to_value(openapi()).unwrap();
        for path in ["/api/v1/version", "/healthz", "/readyz"] {
            assert_eq!(
                document["paths"][path]["get"]["security"],
                serde_json::json!([{}]),
                "{path}"
            );
        }
    }

    #[test]
    fn every_operation_id_is_unique() {
        let document = serde_json::to_value(openapi()).unwrap();
        let mut seen = std::collections::BTreeMap::new();
        for (path, item) in document["paths"].as_object().unwrap() {
            for (method, operation) in item.as_object().unwrap() {
                let id = operation["operationId"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{method} {path} has no operationId"));
                if let Some(first) = seen.insert(id.to_owned(), format!("{method} {path}")) {
                    panic!("operationId {id} is used by {first} and {method} {path}");
                }
            }
        }
    }
}
