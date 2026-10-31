//! HTTP API for zirv-flow.

pub mod admin;
mod assets;
mod assistant;
mod audit;
mod auth;
mod consent;
mod contacts;
mod cookies;
mod credentials;
pub mod deprecation;
mod destinations;
mod events;
mod feedback;
mod flows;
mod forwarding;
mod health;
mod idempotency;
mod imports;
mod ingest;
mod jobs_admin;
mod json;
mod keys;
mod lists;
mod members;
mod metrics;
mod oidc;
mod openapi;
mod ops;
mod passwords;
mod people;
mod privacy;
mod problem;
#[cfg(test)]
mod rbac;
mod request_id;
mod retention;
mod segments;
mod sending;
mod session;
mod setup;
mod snippets;
mod sources;
mod state;
mod summary;
mod templates;
mod test_sends;
mod themes;
mod trace;
mod traces;
mod tracking;
mod users;
mod version;
mod web;
mod webhooks;
mod workspace_credentials;
mod workspaces;

#[cfg(test)]
mod acceptance;

#[cfg(test)]
mod test_support;

use std::future::Future;
use std::io;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware::from_fn;
use axum::routing::{delete, get, patch, post, put};
use tokio::net::TcpListener;

pub use assets::AssetLimits;
pub use assistant::{
    AiConfig, AiConfigError, AiModelConfig, AiProviderConfig, CaseReport, DispatchError,
    Dispatcher, EvaluationError, dispatch, evaluate_assistant,
};
pub use events::ExplorerLimits;
pub use imports::ImportLimits;
pub use ingest::IngestLimits;
pub use oidc::redirect_url as oidc_redirect_url;
pub use openapi::openapi;
pub use ops::{OpsState, ops_router, serve_ops};
pub use problem::Problem;
pub use state::AppState;
pub use zirv_flow_core::egress::{Decision as EgressDecision, EgressConfigError, EgressPolicy};
pub use zirv_flow_identity::{IdentityError, OidcSettings, SecretString};

/// Builds the router for every HTTP surface of the server.
pub fn router(state: AppState) -> Router {
    with_layers(routes(), state)
}

/// Every route and the fallbacks, before the layers every request passes through.
fn routes() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .route("/metrics", get(metrics::render))
        .route("/api/v1/version", get(version::version))
        .route("/api/v1/ingest/identify", post(ingest::identify))
        .route("/api/v1/ingest/track", post(ingest::track))
        .route("/api/v1/ingest/batch", post(ingest::batch))
        .route("/hooks/{id}", post(webhooks::receive::receive))
        // The public feedback webhook of an email provider destination: no `Principal` and no
        // CSRF, the provider's signature authorises the one event (see `feedback`).
        .route(
            "/api/v1/destinations/{id}/feedback",
            post(feedback::receive::receive),
        )
        // The public routes of a sent message: no `Principal` and no CSRF, the signed token
        // authorises the one message (see `tracking`).
        // The public file of an uploaded image: no `Principal`, the hash of the bytes is the
        // address (see `assets::serve`).
        .route("/a/{file}", get(assets::serve::serve))
        .route("/t/o/{token}", get(tracking::open))
        .route("/t/c/{token}", get(tracking::click))
        .route(
            "/u/{token}",
            get(tracking::unsubscribe_page)
                .post(tracking::unsubscribe)
                .layer(DefaultBodyLimit::max(tracking::UNSUBSCRIBE_BODY_LIMIT)),
        )
        .route(
            "/u/{token}/preferences",
            get(tracking::preferences_page)
                .post(tracking::preferences_save)
                .layer(DefaultBodyLimit::max(tracking::PREFERENCES_BODY_LIMIT)),
        )
        .route(
            "/api/v1/workspaces/{ws}/webhooks",
            get(webhooks::ops::list).post(webhooks::ops::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/webhooks/{id}",
            get(webhooks::ops::get)
                .patch(webhooks::ops::update)
                .delete(webhooks::ops::remove),
        )
        .route(
            "/api/v1/workspaces/{ws}/webhooks/{id}/rotate-secret",
            post(webhooks::ops::rotate),
        )
        .route("/api/v1/setup", get(setup::status).post(setup::complete))
        .route("/api/v1/audit", get(audit::list_instance))
        .route("/api/v1/audit/export", get(audit::export::instance))
        .route("/api/v1/workspaces/{ws}/audit", get(audit::list))
        .route("/api/v1/workspaces/{ws}/audit/actions", get(audit::actions))
        .route(
            "/api/v1/workspaces/{ws}/audit/export",
            get(audit::export::workspace),
        )
        .route("/api/v1/admin/jobs", get(jobs_admin::depth))
        .route("/api/v1/admin/jobs/dead", get(jobs_admin::dead))
        .route(
            "/api/v1/admin/jobs/{id}/retry",
            axum::routing::post(jobs_admin::retry),
        )
        .route(
            "/api/v1/session",
            get(session::current)
                .post(session::sign_in)
                .delete(session::sign_out),
        )
        .route("/api/v1/session/methods", get(session::methods))
        .route("/api/v1/session/oidc/start", get(oidc::start))
        .route("/api/v1/session/oidc/callback", get(oidc::callback))
        .route("/api/v1/users", get(users::list).post(users::create))
        .route(
            "/api/v1/workspaces",
            get(workspaces::list).post(workspaces::create),
        )
        .route("/api/v1/workspaces/{ws}/summary", get(summary::get))
        .route(
            "/api/v1/workspaces/{ws}",
            get(workspaces::get).patch(workspaces::update),
        )
        .route("/api/v1/workspaces/{ws}/members", get(members::list))
        .route(
            "/api/v1/workspaces/{ws}/members/{user_id}",
            put(members::set).delete(members::remove),
        )
        .route(
            "/api/v1/workspaces/{ws}/api-keys",
            get(keys::list).post(keys::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/api-keys/{id}",
            axum::routing::delete(keys::revoke),
        )
        .route(
            "/api/v1/workspaces/{ws}/ai/settings",
            get(assistant::settings::get_settings).put(assistant::settings::put_settings),
        )
        .route(
            "/api/v1/workspaces/{ws}/ai/models",
            get(assistant::settings::models),
        )
        .route(
            "/api/v1/workspaces/{ws}/ai/usage",
            get(assistant::settings::usage),
        )
        .route(
            "/api/v1/workspaces/{ws}/ai/drafts",
            post(assistant::drafts::create).layer(DefaultBodyLimit::max(
                assistant::drafts::MAX_DRAFT_REQUEST_BYTES,
            )),
        )
        .route(
            "/api/v1/workspaces/{ws}/ai/drafts/{id}/accept",
            post(assistant::drafts::accept),
        )
        .route(
            "/api/v1/workspaces/{ws}/traits",
            get(people::list).post(people::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/traits/{key}",
            patch(people::update),
        )
        .route(
            "/api/v1/workspaces/{ws}/traits/{key}/index",
            post(people::slots::index).delete(people::slots::unindex),
        )
        .route(
            "/api/v1/workspaces/{ws}/identity-conflicts",
            get(people::conflicts::list),
        )
        .route(
            "/api/v1/workspaces/{ws}/identity-conflicts/{id}",
            get(people::conflicts::get),
        )
        .route(
            "/api/v1/workspaces/{ws}/identity-conflicts/{id}/resolve",
            axum::routing::post(people::conflicts::resolve),
        )
        .route(
            "/api/v1/workspaces/{ws}/people/search",
            post(people::search::search),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows",
            get(flows::crud::list).post(flows::crud::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}",
            get(flows::crud::get).patch(flows::crud::update),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/validate",
            post(flows::crud::validate),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/publish",
            post(flows::publish::publish),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/pause",
            post(flows::publish::pause),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/resume",
            post(flows::publish::resume),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/archive",
            post(flows::publish::archive),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/enrollments",
            get(flows::inspector::list).post(flows::triggers::enroll_person),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}",
            get(flows::inspector::get),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/retry",
            post(flows::operator::retry),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/skip",
            post(flows::operator::skip),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/counts",
            get(flows::inspector::counts),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/exit-enrollments",
            post(flows::operator::exit_version),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/start",
            post(flows::triggers::start),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/test",
            post(flows::test_mode::test),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/versions",
            get(flows::crud::versions),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/versions/{n}",
            get(flows::crud::version),
        )
        .route(
            "/api/v1/workspaces/{ws}/webhook-signing-secret",
            get(flows::signing::get).post(flows::signing::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/webhook-signing-secret/rotate",
            post(flows::signing::rotate),
        )
        .route(
            "/api/v1/workspaces/{ws}/snippets",
            get(snippets::crud::list).post(snippets::crud::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/snippets/{id}",
            get(snippets::crud::get)
                .put(snippets::crud::update)
                .delete(snippets::crud::archive),
        )
        .route(
            "/api/v1/workspaces/{ws}/snippets/{id}/draft",
            put(snippets::crud::put_draft),
        )
        .route(
            "/api/v1/workspaces/{ws}/snippets/{id}/preview",
            post(snippets::preview::preview),
        )
        .route(
            "/api/v1/workspaces/{ws}/snippets/{id}/publish",
            post(snippets::publish::publish),
        )
        .route(
            "/api/v1/workspaces/{ws}/snippets/{id}/versions",
            get(snippets::publish::versions),
        )
        .route(
            "/api/v1/workspaces/{ws}/snippets/{id}/versions/{n}",
            get(snippets::publish::version),
        )
        .route(
            "/api/v1/workspaces/{ws}/snippets/{id}/where-used",
            get(snippets::where_used::where_used),
        )
        .route(
            "/api/v1/workspaces/{ws}/themes",
            get(themes::crud::list).post(themes::crud::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/themes/default",
            put(themes::default::put_default),
        )
        .route(
            "/api/v1/workspaces/{ws}/themes/{id}",
            get(themes::crud::get)
                .put(themes::crud::update)
                .delete(themes::crud::archive),
        )
        .route(
            "/api/v1/workspaces/{ws}/themes/{id}/draft",
            put(themes::crud::put_draft),
        )
        .route(
            "/api/v1/workspaces/{ws}/themes/{id}/preview",
            post(themes::preview::preview),
        )
        .route(
            "/api/v1/workspaces/{ws}/themes/{id}/publish",
            post(themes::publish::publish),
        )
        .route(
            "/api/v1/workspaces/{ws}/themes/{id}/versions",
            get(themes::publish::versions),
        )
        .route(
            "/api/v1/workspaces/{ws}/themes/{id}/versions/{n}",
            get(themes::publish::version),
        )
        .route(
            "/api/v1/workspaces/{ws}/themes/{id}/where-used",
            get(themes::where_used::where_used),
        )
        .route(
            "/api/v1/workspaces/{ws}/templates",
            get(templates::crud::list).post(templates::crud::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/templates/{id}",
            get(templates::crud::get)
                .put(templates::crud::update)
                .delete(templates::crud::archive),
        )
        .route(
            "/api/v1/workspaces/{ws}/templates/{id}/draft",
            put(templates::crud::put_draft),
        )
        .route(
            "/api/v1/workspaces/{ws}/templates/{id}/draft/convert-to-mjml",
            post(templates::crud::convert_draft),
        )
        .route(
            "/api/v1/workspaces/{ws}/templates/{id}/publish",
            post(templates::publish::publish),
        )
        .route(
            "/api/v1/workspaces/{ws}/templates/{id}/versions",
            get(templates::publish::versions),
        )
        .route(
            "/api/v1/workspaces/{ws}/templates/{id}/versions/{n}",
            get(templates::publish::version),
        )
        .route(
            "/api/v1/workspaces/{ws}/templates/{id}/versions/{n}/stats",
            get(templates::publish::version_stats),
        )
        .route(
            "/api/v1/workspaces/{ws}/templates/{id}/where-used",
            get(templates::where_used::where_used),
        )
        .route(
            "/api/v1/workspaces/{ws}/templates/{id}/send-volume",
            get(templates::where_used::send_volume),
        )
        .route(
            "/api/v1/workspaces/{ws}/templates/{id}/preview",
            post(templates::preview::preview)
                .layer(axum::middleware::map_response(templates::preview::headers)),
        )
        .route(
            "/api/v1/workspaces/{ws}/templates/{id}/preview-people",
            get(templates::preview_people::list).post(templates::preview_people::add),
        )
        .route(
            "/api/v1/workspaces/{ws}/templates/{id}/preview-people/{person_id}",
            delete(templates::preview_people::remove),
        )
        .route(
            "/api/v1/workspaces/{ws}/templates/{id}/test-send",
            post(test_sends::test_send),
        )
        .route("/api/v1/workspaces/{ws}/test-sends", get(test_sends::list))
        .route(
            "/api/v1/workspaces/{ws}/test-sends/quota",
            get(test_sends::quota),
        )
        .route(
            "/api/v1/workspaces/{ws}/test-send-allowlist",
            get(test_sends::allowlist_list).post(test_sends::allowlist_add),
        )
        .route(
            "/api/v1/workspaces/{ws}/test-send-allowlist/options",
            get(test_sends::allowlist_options),
        )
        .route(
            "/api/v1/workspaces/{ws}/test-send-allowlist/{id}",
            delete(test_sends::allowlist_delete),
        )
        .route(
            "/api/v1/workspaces/{ws}/segments",
            get(segments::crud::list).post(segments::crud::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/segments/preview",
            post(segments::preview::preview),
        )
        .route(
            "/api/v1/workspaces/{ws}/segments/{id}",
            get(segments::crud::get).patch(segments::crud::update),
        )
        .route(
            "/api/v1/workspaces/{ws}/segments/{id}/archive",
            post(segments::crud::archive),
        )
        .route(
            "/api/v1/workspaces/{ws}/segments/{id}/versions",
            get(segments::crud::versions),
        )
        .route(
            "/api/v1/workspaces/{ws}/segments/{id}/versions/{n}",
            get(segments::crud::version),
        )
        .route(
            "/api/v1/workspaces/{ws}/segments/{id}/members",
            get(segments::members::members),
        )
        .route(
            "/api/v1/workspaces/{ws}/segments/{id}/history",
            get(segments::members::history),
        )
        .route(
            "/api/v1/workspaces/{ws}/segments/{id}/refresh",
            post(segments::members::refresh),
        )
        .route(
            "/api/v1/workspaces/{ws}/segments/{id}/ai/explain",
            post(assistant::explain::explain),
        )
        .route(
            "/api/v1/workspaces/{ws}/flows/{id}/ai/explain",
            post(assistant::flow_explain::explain_flow),
        )
        .route(
            "/api/v1/workspaces/{ws}/traces/{trace_id}/ai/explain",
            post(assistant::flow_explain::explain_trace),
        )
        .route(
            "/api/v1/workspaces/{ws}/people/{id}",
            get(people::profile::get),
        )
        .route(
            "/api/v1/workspaces/{ws}/people/{id}/explain",
            get(segments::explain::explain),
        )
        .route(
            "/api/v1/workspaces/{ws}/people/{id}/timeline",
            get(people::timeline::get),
        )
        .route(
            "/api/v1/workspaces/{ws}/people/{id}/messages",
            get(people::messages::list),
        )
        .route(
            "/api/v1/workspaces/{ws}/event-schemas",
            get(events::schemas::list).post(events::schemas::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/event-schemas/{name}",
            get(events::schemas::get).patch(events::schemas::update),
        )
        .route(
            "/api/v1/workspaces/{ws}/event-schemas/{name}/draft",
            put(events::schemas::put_draft),
        )
        .route(
            "/api/v1/workspaces/{ws}/event-schemas/{name}/versions",
            axum::routing::post(events::schemas::publish),
        )
        .route(
            "/api/v1/workspaces/{ws}/event-policy",
            get(events::policy::get).put(events::policy::put),
        )
        .route(
            "/api/v1/workspaces/{ws}/ingest-rejects",
            get(events::rejects::list),
        )
        .route(
            "/api/v1/workspaces/{ws}/ingest-rejects/replay",
            post(events::replay::replay),
        )
        .route(
            "/api/v1/workspaces/{ws}/debugger/stream",
            get(events::debugger::stream),
        )
        .route(
            "/api/v1/workspaces/{ws}/events/explore/count",
            post(events::explorer::count),
        )
        .route(
            "/api/v1/workspaces/{ws}/events/explore/breakdown",
            post(events::explorer::breakdown),
        )
        .route(
            "/api/v1/workspaces/{ws}/events/explore/list",
            post(events::explorer::list),
        )
        .route(
            "/api/v1/workspaces/{ws}/events/explore/export",
            post(events::explorer::export),
        )
        .route(
            "/api/v1/workspaces/{ws}/topics",
            get(consent::topics::list).post(consent::topics::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/topics/{id}",
            patch(consent::topics::update),
        )
        .route(
            "/api/v1/workspaces/{ws}/consent/dimensions",
            get(consent::dimensions::list).post(consent::dimensions::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/consent/dimensions/{key}",
            patch(consent::dimensions::update),
        )
        .route(
            "/api/v1/workspaces/{ws}/consent/dimensions/{key}/values",
            post(consent::dimensions::add_value_handler),
        )
        .route(
            "/api/v1/workspaces/{ws}/consent/dimensions/{key}/values/{value}",
            patch(consent::dimensions::update_value_handler),
        )
        .route(
            "/api/v1/workspaces/{ws}/consent/policy/draft",
            get(consent::policy::get_draft).put(consent::policy::put_draft),
        )
        .route(
            "/api/v1/workspaces/{ws}/consent/policy/publish",
            post(consent::policy::publish),
        )
        .route(
            "/api/v1/workspaces/{ws}/consent/policy/versions",
            get(consent::policy::versions),
        )
        .route(
            "/api/v1/workspaces/{ws}/consent/policy/evaluate",
            post(consent::policy::evaluate),
        )
        .route(
            "/api/v1/workspaces/{ws}/consent/import",
            post(consent::transfer::import_handler),
        )
        .route(
            "/api/v1/workspaces/{ws}/consent/export",
            get(consent::transfer::export),
        )
        .route(
            "/api/v1/workspaces/{ws}/people/{id}/consent",
            get(consent::person::get).put(consent::person::set),
        )
        .route(
            "/api/v1/workspaces/{ws}/people/{id}/consent/history",
            get(consent::person::history),
        )
        .route(
            "/api/v1/workspaces/{ws}/suppressions",
            get(consent::suppressions::list).post(consent::suppressions::add),
        )
        .route(
            "/api/v1/workspaces/{ws}/suppressions/check",
            post(consent::suppressions::check),
        )
        .route(
            "/api/v1/workspaces/{ws}/suppressions/remove",
            post(consent::suppressions::remove_handler),
        )
        .route(
            "/api/v1/workspaces/{ws}/suppressions/import",
            post(consent::suppressions::import_handler),
        )
        .route(
            "/api/v1/workspaces/{ws}/suppressions/export",
            get(consent::suppressions::export),
        )
        .route(
            "/api/v1/workspaces/{ws}/contact-definition",
            get(contacts::definition::get).put(contacts::definition::put),
        )
        .route(
            "/api/v1/workspaces/{ws}/contacts",
            get(contacts::crud::list).post(contacts::crud::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/contacts/bulk",
            post(contacts::bulk::bulk),
        )
        .route(
            "/api/v1/workspaces/{ws}/contacts/search",
            get(contacts::search::get).post(contacts::search::post),
        )
        .route(
            "/api/v1/workspaces/{ws}/contacts/{key}",
            get(contacts::crud::get)
                .put(contacts::crud::replace)
                .patch(contacts::crud::patch)
                .delete(contacts::crud::remove),
        )
        .route(
            "/api/v1/workspaces/{ws}/destinations/options",
            get(destinations::crud::options),
        )
        .route(
            "/api/v1/workspaces/{ws}/destinations",
            get(destinations::crud::list).post(destinations::crud::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/destinations/{id}",
            get(destinations::crud::get)
                .patch(destinations::crud::update)
                .delete(destinations::crud::remove),
        )
        .route(
            "/api/v1/workspaces/{ws}/destinations/{id}/check",
            post(destinations::actions::check),
        )
        .route(
            "/api/v1/workspaces/{ws}/destinations/{id}/domain-status",
            get(destinations::domain_status::get),
        )
        .route(
            "/api/v1/workspaces/{ws}/destinations/{id}/stats",
            get(destinations::stats::get),
        )
        .route(
            "/api/v1/workspaces/{ws}/forwarding/rules",
            get(forwarding::rules::list).post(forwarding::rules::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/forwarding/rules/{id}",
            get(forwarding::rules::get)
                .patch(forwarding::rules::update)
                .delete(forwarding::rules::remove),
        )
        .route(
            "/api/v1/workspaces/{ws}/forwarding/dead-letters",
            get(forwarding::queue::dead_letters),
        )
        .route(
            "/api/v1/workspaces/{ws}/forwarding/status",
            get(forwarding::queue::status),
        )
        .route(
            "/api/v1/workspaces/{ws}/forwarding/re-drive",
            post(forwarding::queue::redrive),
        )
        .route(
            "/api/v1/workspaces/{ws}/credential-kinds",
            get(workspace_credentials::schema::list_kinds),
        )
        .route(
            "/api/v1/workspaces/{ws}/credentials",
            get(workspace_credentials::crud::list).post(workspace_credentials::crud::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/credentials/{id}",
            get(workspace_credentials::crud::get)
                .patch(workspace_credentials::crud::update)
                .delete(workspace_credentials::crud::remove),
        )
        .route(
            "/api/v1/workspaces/{ws}/credentials/{id}/secret",
            put(workspace_credentials::crud::rotate),
        )
        .route(
            "/api/v1/workspaces/{ws}/credentials/{id}/test",
            post(workspace_credentials::check::test),
        )
        .route(
            "/api/v1/workspaces/{ws}/sending-domains",
            get(sending::domains::list).post(sending::domains::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/sending-domains/{id}",
            get(sending::domains::get).delete(sending::domains::remove),
        )
        .route(
            "/api/v1/workspaces/{ws}/sending-domains/{id}/verify",
            post(sending::domains::verify),
        )
        .route(
            "/api/v1/workspaces/{ws}/sender-identities/options",
            get(sending::identities::options),
        )
        .route(
            "/api/v1/workspaces/{ws}/sender-identities",
            get(sending::identities::list).post(sending::identities::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/sender-identities/{id}",
            get(sending::identities::get)
                .patch(sending::identities::update)
                .delete(sending::identities::remove),
        )
        .route(
            "/api/v1/workspaces/{ws}/sending/settings",
            get(sending::settings::get).put(sending::settings::put),
        )
        .route("/api/v1/source-kinds", get(sources::kinds::list))
        .route("/api/v1/destination-kinds", get(destinations::kinds::list))
        .route(
            "/api/v1/workspaces/{ws}/sources",
            get(sources::crud::list).post(sources::crud::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/sources/{id}",
            get(sources::crud::get)
                .patch(sources::crud::update)
                .delete(sources::crud::remove),
        )
        .route(
            "/api/v1/workspaces/{ws}/sources/{id}/test",
            post(sources::actions::test),
        )
        .route(
            "/api/v1/workspaces/{ws}/sources/{id}/discover",
            post(sources::actions::discover),
        )
        .route(
            "/api/v1/workspaces/{ws}/sources/{id}/oauth/start",
            post(sources::oauth::start),
        )
        .route("/api/v1/oauth/callback", get(sources::oauth::callback))
        .route(
            "/api/v1/workspaces/{ws}/source-manifests/validate",
            post(sources::manifest::validate).layer(DefaultBodyLimit::max(
                zirv_flow_connectors::manifest::MAX_MANIFEST_BYTES
                    + sources::manifest::ENVELOPE_BYTES,
            )),
        )
        .route(
            "/api/v1/workspaces/{ws}/sources/{id}/manifest/sample",
            post(sources::manifest::sample).layer(DefaultBodyLimit::max(
                zirv_flow_connectors::manifest::MAX_MANIFEST_BYTES
                    + sources::manifest::ENVELOPE_BYTES,
            )),
        )
        .route(
            "/api/v1/workspaces/{ws}/sources/{id}/streams",
            get(sources::streams::list),
        )
        .route(
            "/api/v1/workspaces/{ws}/sources/{id}/streams/{name}",
            put(sources::streams::put),
        )
        .route(
            "/api/v1/workspaces/{ws}/sources/{id}/streams/{name}/preview",
            post(sources::preview::preview),
        )
        .route(
            "/api/v1/workspaces/{ws}/sources/{id}/runs",
            get(sources::runs::list).post(sources::runs::start),
        )
        .route(
            "/api/v1/workspaces/{ws}/sources/{id}/runs/{run_id}",
            get(sources::runs::get),
        )
        .route(
            "/api/v1/workspaces/{ws}/sources/{id}/runs/{run_id}/cancel",
            post(sources::runs::cancel),
        )
        .route(
            "/api/v1/workspaces/{ws}/lists",
            get(lists::crud::list).post(lists::crud::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/lists/{id}",
            get(lists::crud::get)
                .patch(lists::crud::update)
                .delete(lists::crud::remove),
        )
        .route(
            "/api/v1/workspaces/{ws}/lists/{id}/members",
            get(lists::members::page).post(lists::members::add),
        )
        .route(
            "/api/v1/workspaces/{ws}/lists/{id}/members/remove",
            post(lists::members::remove),
        )
        .route(
            "/api/v1/workspaces/{ws}/lists/{id}/members/import",
            post(lists::import::import).layer(DefaultBodyLimit::max(lists::import::MAX_BODY_BYTES)),
        )
        .route(
            "/api/v1/workspaces/{ws}/people/{id}/lists",
            get(lists::person::lists),
        )
        .route(
            "/api/v1/workspaces/{ws}/people/{id}/lists/history",
            get(lists::person::history),
        )
        .route(
            "/api/v1/workspaces/{ws}/people/{id}",
            delete(privacy::person::remove),
        )
        .route(
            "/api/v1/workspaces/{ws}/privacy/erasures",
            get(privacy::erasures::list).post(privacy::erasures::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/privacy/exports",
            get(privacy::exports::list).post(privacy::exports::create),
        )
        .route(
            "/api/v1/workspaces/{ws}/privacy/exports/{id}/download",
            get(privacy::exports::download_handler),
        )
        .route(
            "/api/v1/workspaces/{ws}/privacy/settings",
            get(privacy::settings::get).put(privacy::settings::put),
        )
        .route(
            "/api/v1/workspaces/{ws}/retention",
            get(retention::get).put(retention::put),
        )
        .route(
            "/api/v1/workspaces/{ws}/assets",
            get(assets::crud::list)
                .post(assets::upload::upload)
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/v1/workspaces/{ws}/assets/{id}",
            get(assets::crud::get).delete(assets::crud::remove),
        )
        .route(
            "/api/v1/workspaces/{ws}/imports",
            get(imports::crud::list)
                .post(imports::upload::upload)
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/v1/workspaces/{ws}/imports/{id}",
            get(imports::crud::get),
        )
        .route(
            "/api/v1/workspaces/{ws}/imports/{id}/mapping",
            put(imports::crud::put_mapping),
        )
        .route(
            "/api/v1/workspaces/{ws}/imports/{id}/dry-run",
            post(imports::crud::dry_run),
        )
        .route(
            "/api/v1/workspaces/{ws}/imports/{id}/run",
            post(imports::crud::run),
        )
        .route(
            "/api/v1/workspaces/{ws}/imports/{id}/cancel",
            post(imports::crud::cancel),
        )
        .route(
            "/api/v1/workspaces/{ws}/imports/{id}/rejects",
            get(imports::rejects::list),
        )
        .route(
            "/api/v1/workspaces/{ws}/imports/{id}/rejects.csv",
            get(imports::rejects::download),
        )
        .route(
            "/api/v1/workspaces/{ws}/traces/{trace_id}",
            get(traces::get),
        )
        .fallback(web::fallback)
        .method_not_allowed_fallback(problem::method_not_allowed)
}

/// Outermost first: the request id, then the trace, then the metrics.
fn with_layers(routes: Router<AppState>, state: AppState) -> Router {
    routes
        .layer(from_fn(metrics::record))
        .layer(from_fn(trace::assign))
        .layer(from_fn(request_id::assign))
        .with_state(state)
}

/// Serves the router on `listener` until `shutdown` completes, draining in-flight requests.
pub async fn serve(
    listener: TcpListener,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    // A debugger stream never ends by itself, so it must be told, or the drain would wait on it.
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let state = state.with_stop(stopped);
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            shutdown.await;
            let _ = stop.send(true);
        })
        .await
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{Request, Response, StatusCode};
    use http_body_util::BodyExt;
    use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
    use serde_json::Value;
    use tower::ServiceExt;
    use zirv_flow_store::{PgPool, SchemaState};

    use super::*;

    fn unreachable_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(500))
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap()
    }

    fn state() -> AppState {
        AppState::new(unreachable_pool(), Arc::new(String::new))
    }

    async fn get(state: AppState, uri: &str) -> Response<Body> {
        router(state)
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn get_with_id(id: &str) -> Response<Body> {
        router(state())
            .oneshot(
                Request::get("/healthz")
                    .header("x-request-id", id)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn text(response: Response<Body>) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn json(response: Response<Body>) -> Value {
        serde_json::from_str(&text(response).await).unwrap()
    }

    fn content_type(response: &Response<Body>) -> &str {
        response.headers()["content-type"].to_str().unwrap()
    }

    #[tokio::test]
    async fn healthz_is_ok_without_touching_the_database() {
        let response = get(state(), "/healthz").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json(response).await, serde_json::json!({"status": "ok"}));
    }

    #[tokio::test]
    async fn a_wrong_method_is_a_405_problem_with_an_allow_header() {
        let response = router(state())
            .oneshot(Request::post("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(content_type(&response), "application/problem+json");
        assert!(
            response.headers()["allow"]
                .to_str()
                .unwrap()
                .contains("GET")
        );
    }

    #[tokio::test]
    async fn readyz_is_unavailable_when_the_database_is_unreachable() {
        let response = get(state(), "/readyz").await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(content_type(&response), "application/problem+json");
        let body = json(response).await;
        assert_eq!(body["status"], 503);
        assert_eq!(body["detail"], "database unreachable");
    }

    #[tokio::test]
    async fn readyz_reports_pending_migrations() {
        let state = state().with_schema_check(Arc::new(|_| {
            Box::pin(async { Ok(SchemaState::Pending { count: 2 }) })
        }));

        let response = get(state, "/readyz").await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            json(response).await["detail"],
            "database schema is not up to date (pending: 2)"
        );
    }

    #[tokio::test]
    async fn readyz_is_ready_when_the_schema_is_up_to_date() {
        let state =
            state().with_schema_check(Arc::new(|_| Box::pin(async { Ok(SchemaState::UpToDate) })));

        let response = get(state, "/readyz").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json(response).await, serde_json::json!({"status": "ready"}));
    }

    #[tokio::test]
    async fn version_reports_the_package_version() {
        let response = get(state(), "/api/v1/version").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            json(response).await,
            serde_json::json!({"version": env!("CARGO_PKG_VERSION")})
        );
    }

    #[tokio::test]
    async fn an_unknown_api_path_is_a_problem_carrying_the_request_id() {
        let response = get(state(), "/api/v1/x").await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(content_type(&response), "application/problem+json");
        let header = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let body = json(response).await;
        assert_eq!(body["type"], "about:blank");
        assert_eq!(body["status"], 404);
        assert_eq!(body["request_id"], header);
    }

    #[tokio::test]
    async fn a_valid_incoming_request_id_is_kept() {
        let response = get_with_id("abc.DEF_123-x").await;

        assert_eq!(response.headers()["x-request-id"], "abc.DEF_123-x");
    }

    #[tokio::test]
    async fn an_invalid_incoming_request_id_is_replaced() {
        for bad in ["has space", &"a".repeat(129)] {
            let response = get_with_id(bad).await;

            let id = response.headers()["x-request-id"].to_str().unwrap();
            assert_ne!(id, bad);
            assert_eq!(id.len(), 36, "expected a generated UUID, got {id}");
        }
    }

    fn recorder() -> &'static PrometheusHandle {
        static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
        HANDLE.get_or_init(|| PrometheusBuilder::new().install_recorder().unwrap())
    }

    #[tokio::test]
    async fn metrics_show_the_request_counter_with_bounded_route_labels() {
        recorder();
        let state = AppState::new(unreachable_pool(), Arc::new(|| recorder().render()));
        get(state.clone(), "/healthz").await;
        get(state.clone(), "/api/no/such/path/1234").await;
        router(state.clone())
            .oneshot(
                Request::builder()
                    .method("FOO1234")
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let response = get(state, "/metrics").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(content_type(&response), "text/plain; version=0.0.4");
        let body = text(response).await;
        assert!(body.contains(
            r#"zirv_flow_http_requests_total{method="GET",route="/healthz",status="200"}"#
        ));
        assert!(body.contains(r#"route="unmatched",status="404""#));
        assert!(body.contains(r#"method="OTHER""#));
        // Neither the raw path nor the raw method is a label. A bare "1234" would also match a
        // float sample value by chance.
        assert!(!body.contains("no/such/path"));
        assert!(!body.contains("FOO"));
        assert!(body.contains("zirv_flow_http_request_duration_seconds"));
    }

    #[tokio::test]
    async fn rendering_metrics_publishes_the_main_pool_gauges() {
        recorder();
        let state = AppState::new(unreachable_pool(), Arc::new(|| recorder().render()));

        let body = text(get(state, "/metrics").await).await;

        assert!(
            body.contains(r#"zirv_flow_db_pool_connections{pool="main",state="idle"}"#),
            "{body}"
        );
        assert!(
            body.contains(r#"zirv_flow_db_pool_connections{pool="main",state="in_use"}"#),
            "{body}"
        );
        assert!(
            body.contains(r#"zirv_flow_db_pool_max_connections{pool="main"}"#),
            "{body}"
        );
    }

    #[tokio::test]
    async fn every_response_names_its_trace_and_a_problem_body_repeats_it() {
        let response = get(state(), "/api/v1/x").await;

        let header = response.headers()["x-trace-id"]
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(header.len(), 32);
        assert_eq!(json(response).await["trace_id"], header);
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    async fn enqueue_follow_up(State(state): State<AppState>) -> StatusCode {
        let trace = trace::current().expect("the trace middleware wraps every route");
        zirv_flow_store::enqueue(
            &state.pool,
            &zirv_flow_store::NewJob::new("maintenance.noop", trace),
        )
        .await
        .unwrap();
        StatusCode::ACCEPTED
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_enqueued_by_a_request_logs_the_same_trace_id_as_the_request(pool: sqlx::PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let captured = Captured::default();
        let _logging = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .json()
                .with_writer(captured.clone())
                .finish(),
        );
        let app = with_layers(
            routes().route("/test/enqueue", axum::routing::post(enqueue_follow_up)),
            AppState::new(pool.clone(), Arc::new(String::new)),
        );

        let response = app
            .oneshot(
                Request::post("/test/enqueue")
                    .header("x-request-id", "req-42")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let trace_id = response.headers()["x-trace-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let job = sqlx::query_as::<_, (uuid::Uuid, String, String)>(
            "SELECT trace_id, cause_kind, cause_id FROM jobs",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(job.0.simple().to_string(), trace_id);
        assert_eq!((job.1.as_str(), job.2.as_str()), ("api", "req-42"));
        let output = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        let line: Value = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|line| line["fields"]["message"] == "request completed")
            .unwrap_or_else(|| panic!("no request line in {output}"));
        assert_eq!(line["span"]["trace_id"], trace_id.as_str());
        assert_eq!(line["span"]["request_id"], "req-42");
        assert_eq!(line["span"]["route"], "/test/enqueue");
    }
}
