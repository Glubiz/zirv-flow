use utoipa::openapi::security::{ApiKey, ApiKeyValue, Http, HttpAuthScheme, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::consent::{person, suppressions, topics};
use crate::events::{
    self, CreateEventSchemaRequest, EventPolicyBody, EventSchemaBody, EventSchemaDetailBody,
    IngestRejectBody, IngestRejectsBody, PropertyBody, ReasonBody, RedactionBody, SaveDraftRequest,
    SchemaVersionBody, UpdateEventSchemaRequest,
};
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
use crate::problem::{ItemProblem, Problem};
use crate::session::{self, SessionBody, SignInRequest, UserBody};
use crate::setup::{self, SetupRequest, SetupStatus};
use crate::traces::{self, CauseBody, DecisionBody, TraceBody};
use crate::users::{self, CreateUserRequest};
use crate::version::{self, VersionBody};
use crate::workspaces::{self, CreateWorkspaceRequest, WorkspaceBody};

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
        members::list,
        members::set,
        members::remove,
        keys::list,
        keys::create,
        keys::revoke,
        people::registry::list,
        people::registry::create,
        people::registry::update,
        people::conflicts::list,
        people::conflicts::get,
        people::conflicts::resolve,
        people::search::search,
        people::profile::get,
        people::timeline::get,
        events::schemas::list,
        events::schemas::create,
        events::schemas::get,
        events::schemas::put_draft,
        events::schemas::publish,
        events::schemas::update,
        events::policy::get,
        events::policy::put,
        events::rejects::list,
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
        traces::get,
        setup::status,
        setup::complete,
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
        WorkspaceBody,
        MemberBody,
        SetRoleRequest,
        RoleBody,
        ApiKeyBody,
        CreatedApiKeyBody,
        CreateApiKeyRequest,
        TraitBody,
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
        lists::crud::ListBody,
        lists::crud::CreateListRequest,
        lists::crud::UpdateListRequest,
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
        imports::CsvImportBody,
        imports::CsvImportDetailBody,
        imports::crud::CsvImportsBody,
        imports::MappingBody,
        imports::ColumnRuleBody,
        imports::TargetBody,
        imports::TransformBody,
        imports::TraitTypeBody,
        imports::rejects::ImportRejectBody,
        imports::rejects::ImportRejectErrorBody,
        imports::rejects::ImportRejectsBody,
        topics::TopicBody,
        topics::CreateTopicRequest,
        topics::UpdateTopicRequest,
        person::ConsentBody,
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
        privacy::PersonTarget,
        privacy::TargetIdentifier,
        erasures::ErasureBody,
        erasures::ErasuresBody,
        exports::ExportBody,
        exports::ExportsBody,
        privacy::settings::PrivacySettingsBody,
    )),
    modifiers(&SecuritySchemes)
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
