//! The route permission table: what every endpoint demands of its caller, in one place.
//!
//! Each handler still calls `authorize` (or `require_owner`, `require_user`,
//! `require_ingest_key`) itself; this table records the need that call makes, so a test can walk
//! every endpoint without a database. The needs follow the role matrix of architecture 16.2
//! (ruling 92d5483d): Edit for drafts, Publish for publishing and the operator actions, and Admin
//! for whatever the matrix does not place.
//!
//! One row per line, grouped by the feature module that owns the handler. A new endpoint fails
//! `every_openapi_operation_has_a_row_and_every_row_an_operation` until it has a row, and a new
//! route that is not in the OpenAPI document fails
//! `every_route_outside_openapi_is_a_public_row` until it is listed as `Public`.
//! `the_router_and_the_table_list_the_same_routes` also holds each row to a registered method.
//! Where a handler's need depends on the request, the row holds the strictest need and says why.
//!
//! The table is read by tests only: `authorize` takes its need from the call, and the 403 text
//! from the need, so lib.rs compiles this module for tests only.

use crate::auth::Need;

/// How a route authorises its caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Class {
    /// No principal and no scope: the route is open, or a signed token in the URL authorises it.
    Public,
    /// A workspace route: `authorize(.., Need)`.
    Need(Need),
    /// `require_owner`: the instance owner only.
    Owner,
    /// No workspace: a session (`require_user`) or a sign-in, setup, version or kinds route.
    Instance,
    /// `require_ingest_key`: an API key with the `ingest` scope and nobody else.
    Ingest,
    /// A workspace route whose handler makes no `authorize` call: a gap to close, never a class
    /// to keep. Any row with this class fails `no_route_is_left_unchecked`.
    Unchecked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    fn name(self) -> &'static str {
        match self {
            Method::Get => "get",
            Method::Post => "post",
            Method::Put => "put",
            Method::Patch => "patch",
            Method::Delete => "delete",
        }
    }
}

/// One endpoint: its method, its path template as the router registers it, its class and a note
/// (empty when there is nothing to add).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Route {
    pub(crate) method: Method,
    pub(crate) path: &'static str,
    pub(crate) class: Class,
    pub(crate) note: &'static str,
}

const PUBLIC: Class = Class::Public;
const INSTANCE: Class = Class::Instance;
const OWNER: Class = Class::Owner;
const INGEST: Class = Class::Ingest;
const READ: Class = Class::Need(Need::Read);
const READ_MEMBERS: Class = Class::Need(Need::ReadMembers);
const EDIT: Class = Class::Need(Need::Edit);
const PUBLISH: Class = Class::Need(Need::Publish);
const ADMIN: Class = Class::Need(Need::Admin);
#[cfg(test)]
const UNCHECKED: Class = Class::Unchecked;

macro_rules! table {
    ($($method:ident $path:literal => $class:ident $(, $note:literal)?;)*) => {
        &[$(Route {
            method: Method::$method,
            path: $path,
            class: $class,
            note: concat!("" $(, $note)?),
        }),*]
    };
}

/// Every endpoint of the router. Edit a row's class to change what the endpoint is expected to
/// demand; the handler's own `authorize` call is the other half.
pub(crate) const ROUTES: &[Route] = table! {
    // public routes outside the OpenAPI document
    Get    "/healthz" => PUBLIC;
    Get    "/readyz" => PUBLIC;
    Get    "/metrics" => PUBLIC;
    Post   "/hooks/{id}" => PUBLIC;
    Post   "/api/v1/destinations/{id}/feedback" => PUBLIC;
    Get    "/a/{file}" => PUBLIC;
    Get    "/t/o/{token}" => PUBLIC;
    Get    "/t/c/{token}" => PUBLIC;
    Get    "/u/{token}" => PUBLIC;
    Post   "/u/{token}" => PUBLIC;
    Get    "/u/{token}/preferences" => PUBLIC;
    Post   "/u/{token}/preferences" => PUBLIC;
    // version
    Get    "/api/v1/version" => INSTANCE, "no principal";
    // ingest
    Post   "/api/v1/ingest/identify" => INGEST;
    Post   "/api/v1/ingest/track" => INGEST;
    Post   "/api/v1/ingest/batch" => INGEST;
    // webhooks
    Get    "/api/v1/workspaces/{ws}/webhooks" => READ;
    Post   "/api/v1/workspaces/{ws}/webhooks" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/webhooks/{id}" => READ;
    Patch  "/api/v1/workspaces/{ws}/webhooks/{id}" => ADMIN;
    Delete "/api/v1/workspaces/{ws}/webhooks/{id}" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/webhooks/{id}/rotate-secret" => ADMIN;
    // setup
    Get    "/api/v1/setup" => INSTANCE, "no principal";
    Post   "/api/v1/setup" => INSTANCE, "no principal; the setup token authorises";
    // audit
    Get    "/api/v1/audit" => OWNER;
    Get    "/api/v1/audit/export" => OWNER;
    Get    "/api/v1/workspaces/{ws}/audit" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/audit/actions" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/audit/export" => ADMIN;
    // jobs_admin
    Get    "/api/v1/admin/jobs" => OWNER;
    Get    "/api/v1/admin/jobs/dead" => OWNER;
    Post   "/api/v1/admin/jobs/{id}/retry" => OWNER;
    // session
    Get    "/api/v1/session" => INSTANCE;
    Post   "/api/v1/session" => INSTANCE, "no principal; email and password";
    Delete "/api/v1/session" => INSTANCE;
    Get    "/api/v1/session/methods" => INSTANCE, "no principal";
    // oidc
    Get    "/api/v1/session/oidc/start" => INSTANCE, "no principal";
    Get    "/api/v1/session/oidc/callback" => INSTANCE, "no principal; the login state authorises";
    // users
    Get    "/api/v1/users" => OWNER;
    Post   "/api/v1/users" => OWNER;
    // workspaces
    Get    "/api/v1/workspaces" => INSTANCE, "lists the caller's workspaces";
    Post   "/api/v1/workspaces" => OWNER;
    Get    "/api/v1/workspaces/{ws}" => READ;
    Patch  "/api/v1/workspaces/{ws}" => ADMIN;
    // members
    Get    "/api/v1/workspaces/{ws}/members" => READ_MEMBERS;
    Put    "/api/v1/workspaces/{ws}/members/{user_id}" => ADMIN;
    Delete "/api/v1/workspaces/{ws}/members/{user_id}" => ADMIN;
    // keys
    Get    "/api/v1/workspaces/{ws}/api-keys" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/api-keys" => ADMIN;
    Delete "/api/v1/workspaces/{ws}/api-keys/{id}" => ADMIN;
    // assistant
    Get    "/api/v1/workspaces/{ws}/ai/settings" => ADMIN;
    Put    "/api/v1/workspaces/{ws}/ai/settings" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/ai/models" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/ai/usage" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/ai/drafts" => ADMIN, "Admin for a mapping or manifest draft (it reads a source), else Edit";
    Post   "/api/v1/workspaces/{ws}/ai/drafts/{id}/accept" => EDIT;
    Post   "/api/v1/workspaces/{ws}/segments/{id}/ai/explain" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/flows/{id}/ai/explain" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/traces/{trace_id}/ai/explain" => ADMIN;
    // people
    Get    "/api/v1/workspaces/{ws}/traits" => READ;
    Post   "/api/v1/workspaces/{ws}/traits" => ADMIN;
    Patch  "/api/v1/workspaces/{ws}/traits/{key}" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/traits/{key}/index" => ADMIN;
    Delete "/api/v1/workspaces/{ws}/traits/{key}/index" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/identity-conflicts" => READ_MEMBERS;
    Get    "/api/v1/workspaces/{ws}/identity-conflicts/{id}" => READ_MEMBERS, "values masked unless view_pii";
    Post   "/api/v1/workspaces/{ws}/identity-conflicts/{id}/resolve" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/people/search" => READ, "values masked unless view_pii";
    Get    "/api/v1/workspaces/{ws}/people/{id}" => READ, "values masked unless view_pii";
    Get    "/api/v1/workspaces/{ws}/people/{id}/timeline" => READ, "values masked unless view_pii";
    Get    "/api/v1/workspaces/{ws}/people/{id}/messages" => READ;
    // flows
    Get    "/api/v1/workspaces/{ws}/flows" => READ;
    Post   "/api/v1/workspaces/{ws}/flows" => EDIT;
    Get    "/api/v1/workspaces/{ws}/flows/{id}" => READ;
    Patch  "/api/v1/workspaces/{ws}/flows/{id}" => EDIT;
    Post   "/api/v1/workspaces/{ws}/flows/{id}/validate" => EDIT;
    Post   "/api/v1/workspaces/{ws}/flows/{id}/publish" => PUBLISH;
    Post   "/api/v1/workspaces/{ws}/flows/{id}/pause" => PUBLISH;
    Post   "/api/v1/workspaces/{ws}/flows/{id}/resume" => PUBLISH;
    Post   "/api/v1/workspaces/{ws}/flows/{id}/archive" => PUBLISH;
    Get    "/api/v1/workspaces/{ws}/flows/{id}/enrollments" => READ;
    Post   "/api/v1/workspaces/{ws}/flows/{id}/enrollments" => EDIT;
    Get    "/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}" => READ;
    Post   "/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/retry" => PUBLISH;
    Post   "/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/skip" => PUBLISH;
    Get    "/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/counts" => READ;
    Post   "/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/exit-enrollments" => PUBLISH;
    Post   "/api/v1/workspaces/{ws}/flows/{id}/start" => PUBLISH;
    Post   "/api/v1/workspaces/{ws}/flows/{id}/test" => ADMIN, "Edit; a request naming a real person (person_id) also needs view_pii";
    Get    "/api/v1/workspaces/{ws}/flows/{id}/versions" => READ;
    Get    "/api/v1/workspaces/{ws}/flows/{id}/versions/{n}" => READ;
    Get    "/api/v1/workspaces/{ws}/webhook-signing-secret" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/webhook-signing-secret" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/webhook-signing-secret/rotate" => ADMIN;
    // snippets
    Get    "/api/v1/workspaces/{ws}/snippets" => READ;
    Post   "/api/v1/workspaces/{ws}/snippets" => EDIT;
    Get    "/api/v1/workspaces/{ws}/snippets/{id}" => READ;
    Put    "/api/v1/workspaces/{ws}/snippets/{id}" => EDIT;
    Delete "/api/v1/workspaces/{ws}/snippets/{id}" => ADMIN;
    Put    "/api/v1/workspaces/{ws}/snippets/{id}/draft" => EDIT;
    Post   "/api/v1/workspaces/{ws}/snippets/{id}/preview" => EDIT;
    Post   "/api/v1/workspaces/{ws}/snippets/{id}/publish" => PUBLISH;
    Get    "/api/v1/workspaces/{ws}/snippets/{id}/versions" => READ;
    Get    "/api/v1/workspaces/{ws}/snippets/{id}/versions/{n}" => READ;
    Get    "/api/v1/workspaces/{ws}/snippets/{id}/where-used" => READ;
    // themes
    Get    "/api/v1/workspaces/{ws}/themes" => READ;
    Post   "/api/v1/workspaces/{ws}/themes" => EDIT;
    Put    "/api/v1/workspaces/{ws}/themes/default" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/themes/{id}" => READ;
    Put    "/api/v1/workspaces/{ws}/themes/{id}" => EDIT;
    Delete "/api/v1/workspaces/{ws}/themes/{id}" => ADMIN;
    Put    "/api/v1/workspaces/{ws}/themes/{id}/draft" => EDIT;
    Post   "/api/v1/workspaces/{ws}/themes/{id}/preview" => EDIT;
    Post   "/api/v1/workspaces/{ws}/themes/{id}/publish" => PUBLISH;
    Get    "/api/v1/workspaces/{ws}/themes/{id}/versions" => READ;
    Get    "/api/v1/workspaces/{ws}/themes/{id}/versions/{n}" => READ;
    Get    "/api/v1/workspaces/{ws}/themes/{id}/where-used" => READ;
    // templates
    Get    "/api/v1/workspaces/{ws}/templates" => READ;
    Post   "/api/v1/workspaces/{ws}/templates" => EDIT;
    Get    "/api/v1/workspaces/{ws}/templates/{id}" => READ;
    Put    "/api/v1/workspaces/{ws}/templates/{id}" => EDIT;
    Delete "/api/v1/workspaces/{ws}/templates/{id}" => ADMIN;
    Put    "/api/v1/workspaces/{ws}/templates/{id}/draft" => EDIT;
    Post   "/api/v1/workspaces/{ws}/templates/{id}/draft/convert-to-mjml" => EDIT;
    Post   "/api/v1/workspaces/{ws}/templates/{id}/publish" => PUBLISH;
    Get    "/api/v1/workspaces/{ws}/templates/{id}/versions" => READ;
    Get    "/api/v1/workspaces/{ws}/templates/{id}/versions/{n}" => READ;
    Get    "/api/v1/workspaces/{ws}/templates/{id}/versions/{n}/stats" => READ;
    Get    "/api/v1/workspaces/{ws}/templates/{id}/where-used" => READ;
    Get    "/api/v1/workspaces/{ws}/templates/{id}/send-volume" => READ;
    Post   "/api/v1/workspaces/{ws}/templates/{id}/preview" => ADMIN, "Edit; a request naming a real person (person_id) also needs view_pii";
    Get    "/api/v1/workspaces/{ws}/templates/{id}/preview-people" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/templates/{id}/preview-people" => ADMIN;
    Delete "/api/v1/workspaces/{ws}/templates/{id}/preview-people/{person_id}" => ADMIN;
    // test_sends
    Post   "/api/v1/workspaces/{ws}/templates/{id}/test-send" => ADMIN, "Edit; a request naming a real person (person_id) also needs view_pii";
    Get    "/api/v1/workspaces/{ws}/test-sends" => READ;
    Get    "/api/v1/workspaces/{ws}/test-sends/quota" => READ;
    Get    "/api/v1/workspaces/{ws}/test-send-allowlist" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/test-send-allowlist" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/test-send-allowlist/options" => EDIT;
    Delete "/api/v1/workspaces/{ws}/test-send-allowlist/{id}" => ADMIN;
    // segments
    Get    "/api/v1/workspaces/{ws}/segments" => READ;
    Post   "/api/v1/workspaces/{ws}/segments" => EDIT;
    Post   "/api/v1/workspaces/{ws}/segments/preview" => READ, "values masked unless view_pii";
    Get    "/api/v1/workspaces/{ws}/segments/{id}" => READ;
    Patch  "/api/v1/workspaces/{ws}/segments/{id}" => EDIT;
    Post   "/api/v1/workspaces/{ws}/segments/{id}/archive" => EDIT;
    Get    "/api/v1/workspaces/{ws}/segments/{id}/versions" => READ;
    Get    "/api/v1/workspaces/{ws}/segments/{id}/versions/{n}" => READ;
    Get    "/api/v1/workspaces/{ws}/segments/{id}/members" => READ, "values masked unless view_pii";
    Get    "/api/v1/workspaces/{ws}/segments/{id}/history" => READ, "values masked unless view_pii";
    Post   "/api/v1/workspaces/{ws}/segments/{id}/refresh" => EDIT;
    Get    "/api/v1/workspaces/{ws}/people/{id}/explain" => READ, "values masked unless view_pii";
    // events
    Get    "/api/v1/workspaces/{ws}/event-schemas" => READ;
    Post   "/api/v1/workspaces/{ws}/event-schemas" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/event-schemas/{name}" => READ;
    Patch  "/api/v1/workspaces/{ws}/event-schemas/{name}" => ADMIN;
    Put    "/api/v1/workspaces/{ws}/event-schemas/{name}/draft" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/event-schemas/{name}/versions" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/event-policy" => READ;
    Put    "/api/v1/workspaces/{ws}/event-policy" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/ingest-rejects" => READ;
    Post   "/api/v1/workspaces/{ws}/ingest-rejects/replay" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/debugger/stream" => READ, "values masked unless view_pii";
    Post   "/api/v1/workspaces/{ws}/events/explore/count" => READ, "values masked unless view_pii";
    Post   "/api/v1/workspaces/{ws}/events/explore/breakdown" => READ, "values masked unless view_pii";
    Post   "/api/v1/workspaces/{ws}/events/explore/list" => READ, "values masked unless view_pii";
    Post   "/api/v1/workspaces/{ws}/events/explore/export" => READ;
    // consent
    Get    "/api/v1/workspaces/{ws}/topics" => READ;
    Post   "/api/v1/workspaces/{ws}/topics" => ADMIN;
    Patch  "/api/v1/workspaces/{ws}/topics/{id}" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/consent/dimensions" => READ;
    Post   "/api/v1/workspaces/{ws}/consent/dimensions" => ADMIN;
    Patch  "/api/v1/workspaces/{ws}/consent/dimensions/{key}" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/consent/dimensions/{key}/values" => ADMIN;
    Patch  "/api/v1/workspaces/{ws}/consent/dimensions/{key}/values/{value}" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/consent/policy/draft" => READ;
    Put    "/api/v1/workspaces/{ws}/consent/policy/draft" => EDIT;
    Post   "/api/v1/workspaces/{ws}/consent/policy/publish" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/consent/policy/versions" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/consent/policy/evaluate" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/consent/import" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/consent/export" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/people/{id}/consent" => READ;
    Put    "/api/v1/workspaces/{ws}/people/{id}/consent" => EDIT;
    Get    "/api/v1/workspaces/{ws}/people/{id}/consent/history" => READ, "values masked unless view_pii";
    Get    "/api/v1/workspaces/{ws}/suppressions" => READ;
    Post   "/api/v1/workspaces/{ws}/suppressions" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/suppressions/check" => READ;
    Post   "/api/v1/workspaces/{ws}/suppressions/remove" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/suppressions/import" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/suppressions/export" => ADMIN;
    // contacts
    Get    "/api/v1/workspaces/{ws}/contact-definition" => READ;
    Put    "/api/v1/workspaces/{ws}/contact-definition" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/contacts" => READ;
    Post   "/api/v1/workspaces/{ws}/contacts" => EDIT;
    Post   "/api/v1/workspaces/{ws}/contacts/bulk" => EDIT;
    Get    "/api/v1/workspaces/{ws}/contacts/search" => READ;
    Post   "/api/v1/workspaces/{ws}/contacts/search" => READ;
    Get    "/api/v1/workspaces/{ws}/contacts/{key}" => READ;
    Put    "/api/v1/workspaces/{ws}/contacts/{key}" => EDIT;
    Patch  "/api/v1/workspaces/{ws}/contacts/{key}" => EDIT;
    Delete "/api/v1/workspaces/{ws}/contacts/{key}" => EDIT;
    // destinations
    Get    "/api/v1/workspaces/{ws}/destinations/options" => EDIT;
    Get    "/api/v1/workspaces/{ws}/destinations" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/destinations" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/destinations/{id}" => ADMIN;
    Patch  "/api/v1/workspaces/{ws}/destinations/{id}" => ADMIN;
    Delete "/api/v1/workspaces/{ws}/destinations/{id}" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/destinations/{id}/check" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/destinations/{id}/domain-status" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/destinations/{id}/stats" => ADMIN;
    Get    "/api/v1/destination-kinds" => INSTANCE;
    // forwarding
    Get    "/api/v1/workspaces/{ws}/forwarding/rules" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/forwarding/rules" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/forwarding/rules/{id}" => ADMIN;
    Patch  "/api/v1/workspaces/{ws}/forwarding/rules/{id}" => ADMIN;
    Delete "/api/v1/workspaces/{ws}/forwarding/rules/{id}" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/forwarding/dead-letters" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/forwarding/status" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/forwarding/re-drive" => ADMIN;
    // workspace_credentials
    Get    "/api/v1/workspaces/{ws}/credential-kinds" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/credentials" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/credentials" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/credentials/{id}" => ADMIN;
    Patch  "/api/v1/workspaces/{ws}/credentials/{id}" => ADMIN;
    Delete "/api/v1/workspaces/{ws}/credentials/{id}" => ADMIN;
    Put    "/api/v1/workspaces/{ws}/credentials/{id}/secret" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/credentials/{id}/test" => ADMIN;
    // sending
    Get    "/api/v1/workspaces/{ws}/sending-domains" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/sending-domains" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/sending-domains/{id}" => ADMIN;
    Delete "/api/v1/workspaces/{ws}/sending-domains/{id}" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/sending-domains/{id}/verify" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/sender-identities/options" => EDIT;
    Get    "/api/v1/workspaces/{ws}/sender-identities" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/sender-identities" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/sender-identities/{id}" => ADMIN;
    Patch  "/api/v1/workspaces/{ws}/sender-identities/{id}" => ADMIN;
    Delete "/api/v1/workspaces/{ws}/sender-identities/{id}" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/sending/settings" => READ;
    Put    "/api/v1/workspaces/{ws}/sending/settings" => ADMIN;
    // sources
    Get    "/api/v1/source-kinds" => INSTANCE;
    Get    "/api/v1/workspaces/{ws}/sources" => READ;
    Post   "/api/v1/workspaces/{ws}/sources" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/sources/{id}" => READ;
    Patch  "/api/v1/workspaces/{ws}/sources/{id}" => ADMIN;
    Delete "/api/v1/workspaces/{ws}/sources/{id}" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/sources/{id}/test" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/sources/{id}/discover" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/sources/{id}/oauth/start" => ADMIN;
    Get    "/api/v1/oauth/callback" => ADMIN, "outside /workspaces/{ws}: needs the signed-in session of the user who started the consent, Admin on the consent's workspace, and the state token";
    Post   "/api/v1/workspaces/{ws}/source-manifests/validate" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/sources/{id}/manifest/sample" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/sources/{id}/streams" => READ, "values masked unless view_pii";
    Put    "/api/v1/workspaces/{ws}/sources/{id}/streams/{name}" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/sources/{id}/streams/{name}/preview" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/sources/{id}/runs" => READ;
    Post   "/api/v1/workspaces/{ws}/sources/{id}/runs" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/sources/{id}/runs/{run_id}" => READ;
    Post   "/api/v1/workspaces/{ws}/sources/{id}/runs/{run_id}/cancel" => ADMIN;
    // lists
    Get    "/api/v1/workspaces/{ws}/lists" => READ;
    Post   "/api/v1/workspaces/{ws}/lists" => EDIT;
    Get    "/api/v1/workspaces/{ws}/lists/{id}" => READ;
    Patch  "/api/v1/workspaces/{ws}/lists/{id}" => EDIT;
    Delete "/api/v1/workspaces/{ws}/lists/{id}" => EDIT;
    Get    "/api/v1/workspaces/{ws}/lists/{id}/members" => READ, "values masked unless view_pii";
    Post   "/api/v1/workspaces/{ws}/lists/{id}/members" => EDIT;
    Post   "/api/v1/workspaces/{ws}/lists/{id}/members/remove" => EDIT;
    Post   "/api/v1/workspaces/{ws}/lists/{id}/members/import" => EDIT;
    Get    "/api/v1/workspaces/{ws}/people/{id}/lists" => READ;
    Get    "/api/v1/workspaces/{ws}/people/{id}/lists/history" => READ;
    // privacy
    Delete "/api/v1/workspaces/{ws}/people/{id}" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/privacy/erasures" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/privacy/erasures" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/privacy/exports" => ADMIN;
    Post   "/api/v1/workspaces/{ws}/privacy/exports" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/privacy/exports/{id}/download" => ADMIN;
    Get    "/api/v1/workspaces/{ws}/privacy/settings" => READ;
    Put    "/api/v1/workspaces/{ws}/privacy/settings" => ADMIN;
    // assets
    Get    "/api/v1/workspaces/{ws}/assets" => READ;
    Post   "/api/v1/workspaces/{ws}/assets" => EDIT;
    Get    "/api/v1/workspaces/{ws}/assets/{id}" => READ;
    Delete "/api/v1/workspaces/{ws}/assets/{id}" => ADMIN;
    // imports
    Get    "/api/v1/workspaces/{ws}/imports" => READ;
    Post   "/api/v1/workspaces/{ws}/imports" => EDIT, "values masked unless view_pii";
    Get    "/api/v1/workspaces/{ws}/imports/{id}" => READ, "values masked unless view_pii";
    Put    "/api/v1/workspaces/{ws}/imports/{id}/mapping" => EDIT;
    Post   "/api/v1/workspaces/{ws}/imports/{id}/dry-run" => EDIT;
    Post   "/api/v1/workspaces/{ws}/imports/{id}/run" => PUBLISH;
    Post   "/api/v1/workspaces/{ws}/imports/{id}/cancel" => EDIT;
    Get    "/api/v1/workspaces/{ws}/imports/{id}/rejects" => READ;
    Get    "/api/v1/workspaces/{ws}/imports/{id}/rejects.csv" => READ, "values masked unless view_pii";
    // traces
    Get    "/api/v1/workspaces/{ws}/traces/{trace_id}" => READ;
};

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const LIB: &str = include_str!("lib.rs");
    /// Router mounts that add routes without a textual `.route(`: none today. An entry is
    /// `(source text, reason)` and the reason must say why its routes need no row.
    const EXPLICIT_MOUNTS: &[(&str, &str)] = &[];
    const MOUNTS: [&str; 6] = [
        ".nest(",
        ".nest_service(",
        ".route_service(",
        ".merge(",
        ".fallback_service(",
        ".route_layer_service(",
    ];
    const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

    fn key(method: &str, path: &str) -> String {
        format!("{method} {path}")
    }

    fn row_key(row: &Route) -> String {
        key(row.method.name(), row.path)
    }

    /// Every `(method, path)` the OpenAPI document describes.
    fn documented() -> BTreeSet<String> {
        let document = serde_json::to_value(crate::openapi()).unwrap();
        let mut keys = BTreeSet::new();
        for (path, item) in document["paths"].as_object().unwrap() {
            for method in item.as_object().unwrap().keys() {
                if METHODS.contains(&method.as_str()) {
                    keys.insert(key(method, path));
                }
            }
        }
        keys
    }

    /// Every `(method, path)` the router registers, read from the source of `routes()`.
    fn registered() -> BTreeSet<String> {
        let start = LIB.find("fn routes()").expect("routes()");
        let end = LIB.find(".fallback(web::fallback)").expect("the fallback");
        let code: String = LIB[start..end]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut keys = BTreeSet::new();
        for chunk in code.split(".route(").skip(1) {
            let path = chunk.split('"').nth(1).expect("a route path");
            let handlers = &chunk[chunk.find(',').expect("a route handler")..];
            for method in METHODS {
                let call = format!("{method}(");
                for (at, _) in handlers.match_indices(&call) {
                    let before = handlers[..at].chars().next_back();
                    if !before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                        keys.insert(key(method, path));
                    }
                }
            }
        }
        keys
    }

    /// The lines of `source` that mount routes other than through `.route(` and are not listed
    /// in `EXPLICIT_MOUNTS`.
    fn unlisted_mounts(source: &str) -> Vec<String> {
        source
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//"))
            .filter(|line| MOUNTS.iter().any(|mount| line.contains(mount)))
            .filter(|line| !EXPLICIT_MOUNTS.iter().any(|(text, _)| line.contains(text)))
            .map(str::to_owned)
            .collect()
    }

    /// The operations that lack a row, and the non-public rows no operation backs. A public row
    /// may or may not be documented.
    fn coverage_gaps(documented: &BTreeSet<String>, rows: &[Route]) -> Vec<String> {
        let listed: BTreeSet<String> = rows.iter().map(row_key).collect();
        let needing_an_operation: BTreeSet<String> = rows
            .iter()
            .filter(|row| row.class != Class::Public)
            .map(row_key)
            .collect();
        let mut gaps = Vec::new();
        for missing in documented.difference(&listed) {
            gaps.push(format!("missing row: {missing}"));
        }
        for stale in needing_an_operation.difference(documented) {
            gaps.push(format!("stale row: {stale}"));
        }
        gaps
    }

    fn unchecked(rows: &[Route]) -> Vec<String> {
        rows.iter()
            .filter(|row| row.class == Class::Unchecked)
            .map(row_key)
            .collect()
    }

    #[test]
    fn every_openapi_operation_has_a_row_and_every_row_an_operation() {
        let gaps = coverage_gaps(&documented(), ROUTES);
        assert!(gaps.is_empty(), "{}", gaps.join("\n"));
    }

    #[test]
    fn a_table_missing_a_row_names_it() {
        let truncated: Vec<Route> = ROUTES
            .iter()
            .filter(|row| row_key(row) != "post /api/v1/workspaces/{ws}/flows")
            .copied()
            .collect();
        assert_eq!(truncated.len(), ROUTES.len() - 1);
        assert_eq!(
            coverage_gaps(&documented(), &truncated),
            vec!["missing row: post /api/v1/workspaces/{ws}/flows".to_owned()]
        );
    }

    #[test]
    fn a_row_without_an_operation_is_reported_as_stale() {
        let mut extended = ROUTES.to_vec();
        extended.push(Route {
            method: Method::Delete,
            path: "/api/v1/workspaces/{ws}/nothing",
            class: READ,
            note: "",
        });
        assert_eq!(
            coverage_gaps(&documented(), &extended),
            vec!["stale row: delete /api/v1/workspaces/{ws}/nothing".to_owned()]
        );
    }

    #[test]
    fn the_router_and_the_table_list_the_same_routes() {
        let rows: BTreeSet<String> = ROUTES.iter().map(row_key).collect();
        let routed = registered();
        let unlisted: Vec<_> = routed.difference(&rows).collect();
        let unrouted: Vec<_> = rows.difference(&routed).collect();
        assert!(
            unlisted.is_empty() && unrouted.is_empty(),
            "routed without a row: {unlisted:?}\nrow without a route: {unrouted:?}"
        );
    }

    #[test]
    fn every_route_outside_openapi_is_a_public_row() {
        let public: BTreeSet<String> = ROUTES
            .iter()
            .filter(|row| row.class == Class::Public)
            .map(row_key)
            .collect();
        let undocumented: BTreeSet<String> =
            registered().difference(&documented()).cloned().collect();
        let not_public: Vec<_> = undocumented.difference(&public).collect();
        assert!(
            not_public.is_empty(),
            "routed, not in OpenAPI and not a Public row: {not_public:?}"
        );
        assert!(!undocumented.is_empty());
    }

    #[test]
    fn the_table_has_no_duplicate_key() {
        let mut seen = BTreeSet::new();
        for row in ROUTES {
            assert!(seen.insert(row_key(row)), "duplicate row: {}", row_key(row));
        }
    }

    #[test]
    fn a_workspace_path_is_always_a_need_row() {
        for row in ROUTES {
            if row.path.contains("/workspaces/{ws}") {
                assert!(
                    matches!(row.class, Class::Need(_)),
                    "{} is under a workspace but is {:?}",
                    row_key(row),
                    row.class
                );
            }
        }
    }

    #[test]
    fn no_route_is_left_unchecked() {
        assert_eq!(unchecked(ROUTES), Vec::<String>::new());
    }

    #[test]
    fn an_unchecked_row_is_reported() {
        let rows = [Route {
            method: Method::Get,
            path: "/api/v1/workspaces/{ws}/forgotten",
            class: UNCHECKED,
            note: "",
        }];
        assert_eq!(
            unchecked(&rows),
            vec!["get /api/v1/workspaces/{ws}/forgotten".to_owned()]
        );
    }

    #[test]
    fn a_request_dependent_need_is_recorded_as_the_strictest_with_a_note() {
        for path in [
            "/api/v1/workspaces/{ws}/templates/{id}/preview",
            "/api/v1/workspaces/{ws}/templates/{id}/test-send",
            "/api/v1/workspaces/{ws}/flows/{id}/test",
        ] {
            let row = ROUTES
                .iter()
                .find(|row| row.method == Method::Post && row.path == path)
                .unwrap_or_else(|| panic!("no row for {path}"));
            assert_eq!(row.class, ADMIN, "{path}");
            assert!(row.note.contains("Edit"), "{path} has no note");
        }
        let drafts = ROUTES
            .iter()
            .find(|row| row.path == "/api/v1/workspaces/{ws}/ai/drafts")
            .unwrap();
        assert_eq!(drafts.class, ADMIN);
        assert!(drafts.note.contains("Edit"));
    }

    /// The publish rows, as the matrix places them.
    const PUBLISH_ROWS: [(&str, &str); 12] = [
        ("post", "/api/v1/workspaces/{ws}/flows/{id}/publish"),
        ("post", "/api/v1/workspaces/{ws}/flows/{id}/pause"),
        ("post", "/api/v1/workspaces/{ws}/flows/{id}/resume"),
        ("post", "/api/v1/workspaces/{ws}/flows/{id}/archive"),
        (
            "post",
            "/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/retry",
        ),
        (
            "post",
            "/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/skip",
        ),
        (
            "post",
            "/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/exit-enrollments",
        ),
        ("post", "/api/v1/workspaces/{ws}/flows/{id}/start"),
        ("post", "/api/v1/workspaces/{ws}/snippets/{id}/publish"),
        ("post", "/api/v1/workspaces/{ws}/themes/{id}/publish"),
        ("post", "/api/v1/workspaces/{ws}/templates/{id}/publish"),
        ("post", "/api/v1/workspaces/{ws}/imports/{id}/run"),
    ];

    #[test]
    fn exactly_the_ruled_routes_need_publish() {
        let found: BTreeSet<String> = ROUTES
            .iter()
            .filter(|row| row.class == PUBLISH)
            .map(row_key)
            .collect();
        let ruled: BTreeSet<String> = PUBLISH_ROWS
            .iter()
            .map(|(method, path)| key(method, path))
            .collect();
        assert_eq!(found, ruled);
    }

    #[test]
    fn a_viewer_edits_nothing_an_editor_publishes_nothing_and_a_publisher_administers_nothing() {
        use zirv_flow_core::Role;

        use crate::auth::role_allows;

        let refused = |role: Role| -> BTreeSet<String> {
            ROUTES
                .iter()
                .filter_map(|row| match row.class {
                    Class::Need(need) if !role_allows(role, need) => Some(format!("{need:?}")),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            refused(Role::Viewer),
            BTreeSet::from(["Edit", "Publish", "Admin"].map(String::from))
        );
        assert_eq!(
            refused(Role::Editor),
            BTreeSet::from(["Publish", "Admin"].map(String::from))
        );
        assert_eq!(
            refused(Role::Publisher),
            BTreeSet::from(["Admin".to_owned()])
        );
        assert!(refused(Role::Admin).is_empty());
    }

    #[test]
    fn the_router_mounts_no_routes_the_inventory_cannot_see() {
        // A checkout may end its lines with CRLF.
        let lib = LIB.replace("\r\n", "\n");
        let end = lib
            .find("#[cfg(test)]\nmod tests")
            .expect("the test module");
        assert_eq!(unlisted_mounts(&lib[..end]), Vec::<String>::new());
        assert!(EXPLICIT_MOUNTS.iter().all(|(_, reason)| !reason.is_empty()));
    }

    #[test]
    fn a_nested_or_merged_mount_is_caught() {
        for source in [
            "Router::new().nest(\"/x\", other())",
            "    .nest_service(\"/x\", files)",
            "    .route_service(\"/x\", svc)",
            "    .merge(other_routes())",
        ] {
            assert_eq!(unlisted_mounts(source), vec![source.trim().to_owned()]);
        }
        assert!(unlisted_mounts("    // .merge(other_routes())").is_empty());
    }
}
