use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;
use utoipa::ToSchema;
use zirv_flow_store::SchemaState;

use crate::AppState;
use crate::problem::{ApiError, Problem};
use crate::state::SchemaCheck;

const READINESS_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Serialize, ToSchema)]
pub(crate) struct StatusBody {
    #[schema(example = "ok")]
    status: &'static str,
}

/// Liveness: the process is serving requests. Never touches the database.
#[utoipa::path(
    get,
    operation_id = "get_healthz",
    path = "/healthz",
    tag = "health",
    responses((status = 200, description = "The process is alive", body = StatusBody)),
    security(())
)]
pub(crate) async fn healthz() -> Json<StatusBody> {
    Json(StatusBody { status: "ok" })
}

/// Readiness: the database is reachable and its schema is up to date.
#[utoipa::path(
    get,
    operation_id = "get_readyz",
    path = "/readyz",
    tag = "health",
    responses(
        (status = 200, description = "Ready to serve", body = StatusBody),
        (
            status = 503,
            description = "The database is unreachable or its schema is not up to date, \
                or a role loop has stopped or shutdown has begun",
            body = Problem,
            content_type = "application/problem+json"
        )
    ),
    security(())
)]
pub(crate) async fn readyz(State(state): State<AppState>) -> Result<Json<StatusBody>, ApiError> {
    if let Some(reason) = state.role_readiness.as_ref().and_then(|ready| ready()) {
        return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, reason));
    }
    check_database(state.pool.clone(), &state.schema_check).await?;
    Ok(ready())
}

pub(crate) fn ready() -> Json<StatusBody> {
    Json(StatusBody { status: "ready" })
}

/// Whether the database answers and its schema is up to date; the error is the 503 to return.
pub(crate) async fn check_database(
    pool: zirv_flow_store::PgPool,
    schema_check: &SchemaCheck,
) -> Result<(), ApiError> {
    let unavailable = |detail: &str| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, detail);
    match tokio::time::timeout(READINESS_TIMEOUT, schema_check(pool)).await {
        Err(_) => Err(unavailable("database check timed out")),
        Ok(Err(_)) => Err(unavailable("database unreachable")),
        Ok(Ok(SchemaState::UpToDate)) => Ok(()),
        Ok(Ok(SchemaState::Pending { count })) => Err(unavailable(&format!(
            "database schema is not up to date (pending: {count})"
        ))),
        Ok(Ok(_)) => Err(unavailable(
            "database schema is not compatible with this version",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use zirv_flow_store::PgPool;

    use crate::{AppState, router};

    fn unreachable_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(500))
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap()
    }

    async fn readyz_body(state: AppState) -> (StatusCode, String) {
        use http_body_util::BodyExt;
        let response = router(state)
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn readyz_names_a_role_reason_before_it_checks_the_database() {
        let state = AppState::new(unreachable_pool(), Arc::new(String::new))
            .with_role_readiness(Arc::new(|| Some("the scheduler loop has stopped")));

        let (status, body) = readyz_body(state).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("the scheduler loop has stopped"), "{body}");
    }

    #[tokio::test]
    async fn readyz_falls_through_to_the_database_when_the_roles_are_ready() {
        let state = AppState::new(unreachable_pool(), Arc::new(String::new))
            .with_role_readiness(Arc::new(|| None));

        let (status, body) = readyz_body(state).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("database"), "{body}");
    }

    async fn status(pool: PgPool) -> StatusCode {
        let state = AppState::new(pool, Arc::new(String::new));
        router(state)
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn readyz_is_ok_on_a_migrated_database(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();

        assert_eq!(status(pool).await, StatusCode::OK);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn readyz_is_unavailable_when_the_database_schema_is_newer(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO _sqlx_migrations \
             (version, description, success, checksum, execution_time) \
             VALUES (999, 'future', true, '\\x00', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(status(pool).await, StatusCode::SERVICE_UNAVAILABLE);
    }
}
