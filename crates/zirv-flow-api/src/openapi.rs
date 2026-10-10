use utoipa::OpenApi;

use crate::health::{self, StatusBody};
use crate::problem::Problem;
use crate::version::{self, VersionBody};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "zirv-flow API",
        // The API version, not the crate version, so a release that leaves
        // the API untouched does not make the committed document stale.
        version = "1",
        description = "HTTP API of the zirv-flow server."
    ),
    paths(health::healthz, health::readyz, version::version),
    components(schemas(Problem, StatusBody, VersionBody))
)]
struct ApiDoc;

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
