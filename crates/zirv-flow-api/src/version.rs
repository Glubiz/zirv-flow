use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub(crate) struct VersionBody {
    /// The running zirv-flow version.
    version: &'static str,
}

/// The running zirv-flow version.
#[utoipa::path(
    get,
    path = "/api/v1/version",
    tag = "meta",
    responses((status = 200, description = "The running version", body = VersionBody))
)]
pub(crate) async fn version() -> Json<VersionBody> {
    Json(VersionBody {
        version: env!("CARGO_PKG_VERSION"),
    })
}
