//! Instance-level user management. Owner only.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use utoipa::ToSchema;

use crate::AppState;
use crate::admin;
use crate::auth::Principal;
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::session::UserBody;

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateUserRequest {
    #[schema(example = "jane@example.com")]
    email: String,
    /// 12 to 1024 characters.
    password: String,
}

/// Lists every user. Owner only.
#[utoipa::path(
    get,
    path = "/api/v1/users",
    tag = "users",
    responses(
        (status = 200, description = "All users", body = Vec<UserBody>),
        (status = 401, description = "Not signed in", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not the owner, or an API key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Vec<UserBody>>, ApiError> {
    principal.require_owner()?;
    let users = zirv_flow_store::list_users(&state.pool).await?;
    Ok(Json(users.iter().map(UserBody::from).collect()))
}

/// Creates a user. Owner only. Needs the CSRF token.
#[utoipa::path(
    post,
    path = "/api/v1/users",
    tag = "users",
    request_body = CreateUserRequest,
    responses(
        (status = 201, description = "The new user", body = UserBody),
        (status = 401, description = "Not signed in", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not the owner, an API key, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The email is already in use", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid email or password", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    ApiJson(body): ApiJson<CreateUserRequest>,
) -> Result<(StatusCode, Json<UserBody>), ApiError> {
    principal.require_owner()?;
    let user = admin::create_user(
        &state.pool,
        &principal.context(),
        &body.email,
        &body.password,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(UserBody::from(&user))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_needs_both_fields() {
        assert!(serde_json::from_str::<CreateUserRequest>(r#"{"email":"a@b.c"}"#).is_err());
        assert!(
            serde_json::from_str::<CreateUserRequest>(r#"{"email":"a@b.c","password":"p"}"#)
                .is_ok()
        );
    }
}
