use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;
use utoipa::ToSchema;
use zirv_flow_store::SchemaState;

use crate::AppState;
use crate::problem::{ApiError, Problem};

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
    responses((status = 200, description = "The process is alive", body = StatusBody))
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
            description = "The database is unreachable or its schema is not up to date",
            body = Problem,
            content_type = "application/problem+json"
        )
    )
)]
pub(crate) async fn readyz(State(state): State<AppState>) -> Result<Json<StatusBody>, ApiError> {
    let check = (state.schema_check)(state.pool.clone());
    let unavailable = |detail: &str| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, detail);
    match tokio::time::timeout(READINESS_TIMEOUT, check).await {
        Err(_) => Err(unavailable("database check timed out")),
        Ok(Err(_)) => Err(unavailable("database unreachable")),
        Ok(Ok(SchemaState::UpToDate)) => Ok(Json(StatusBody { status: "ready" })),
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
