use utoipa::openapi::security::{ApiKey, ApiKeyValue, Http, HttpAuthScheme, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::health::{self, StatusBody};
use crate::keys::{self, ApiKeyBody, CreateApiKeyRequest, CreatedApiKeyBody};
use crate::members::{self, MemberBody, RoleBody, SetRoleRequest};
use crate::oidc;
use crate::problem::Problem;
use crate::session::{self, SessionBody, SignInRequest, UserBody};
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
    ),
    components(schemas(
        Problem,
        StatusBody,
        VersionBody,
        SignInRequest,
        SessionBody,
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
}
