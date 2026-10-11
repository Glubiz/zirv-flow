//! The metrics and health listener of processes that do not run the `serve` role.

use std::future::Future;
use std::io;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware::from_fn;
use axum::response::Response;
use axum::routing::get;
use tokio::net::TcpListener;
use zirv_flow_store::PgPool;

use crate::health::{self, StatusBody};
use crate::problem::{self, ApiError};
use crate::request_id;
use crate::state::SchemaCheck;

/// Why the roles of the process are not ready, or `None` when they are.
type RoleReadiness = Arc<dyn Fn() -> Option<&'static str> + Send + Sync>;

/// State of the ops listener.
#[derive(Clone)]
pub struct OpsState {
    pool: PgPool,
    render_metrics: Arc<dyn Fn() -> String + Send + Sync>,
    role_readiness: RoleReadiness,
    schema_check: SchemaCheck,
}

impl OpsState {
    /// `render_metrics` returns the Prometheus text exposition. `role_readiness` returns the
    /// reason the process is not ready, such as a stopped role loop, or `None` when it is.
    pub fn new(
        pool: PgPool,
        render_metrics: Arc<dyn Fn() -> String + Send + Sync>,
        role_readiness: Arc<dyn Fn() -> Option<&'static str> + Send + Sync>,
    ) -> Self {
        Self {
            pool,
            render_metrics,
            role_readiness,
            schema_check: Arc::new(|pool| {
                Box::pin(async move { zirv_flow_store::schema_state(&pool).await })
            }),
        }
    }
}

/// `GET /healthz`, `/readyz` and `/metrics`, answering as they do on the API server.
pub fn ops_router(state: OpsState) -> Router {
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .fallback(problem::not_found)
        .method_not_allowed_fallback(problem::method_not_allowed)
        .layer(from_fn(request_id::assign))
        .with_state(state)
}

/// Serves the ops router on `listener` until `shutdown` completes.
pub async fn serve_ops(
    listener: TcpListener,
    state: OpsState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    axum::serve(listener, ops_router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

async fn readyz(State(state): State<OpsState>) -> Result<Json<StatusBody>, ApiError> {
    if let Some(reason) = (state.role_readiness)() {
        return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, reason));
    }
    health::check_database(state.pool.clone(), &state.schema_check).await?;
    Ok(health::ready())
}

async fn metrics(State(state): State<OpsState>) -> Response {
    crate::metrics::respond(&state.pool, &state.render_metrics)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;

    fn unreachable_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(500))
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap()
    }

    fn state(pool: PgPool, reason: Option<&'static str>) -> OpsState {
        OpsState::new(
            pool,
            Arc::new(|| "zirv_flow_example 1\n".to_owned()),
            Arc::new(move || reason),
        )
    }

    async fn get(state: OpsState, uri: &str) -> Response<Body> {
        ops_router(state)
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn body(response: Response<Body>) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn readyz_names_the_role_reason_before_asking_the_database() {
        let response = get(
            state(unreachable_pool(), Some("the worker loop has stopped")),
            "/readyz",
        )
        .await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers()["content-type"],
            "application/problem+json"
        );
        let problem: Value = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(problem["detail"], "the worker loop has stopped");
    }

    #[tokio::test]
    async fn readyz_reports_the_database_when_the_roles_are_running() {
        let response = get(state(unreachable_pool(), None), "/readyz").await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let problem: Value = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(problem["detail"], "database unreachable");
    }

    #[tokio::test]
    async fn metrics_serves_the_rendered_text() {
        let response = get(state(unreachable_pool(), None), "/metrics").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"],
            "text/plain; version=0.0.4"
        );
        assert!(body(response).await.contains("zirv_flow_example 1"));
    }

    #[tokio::test]
    async fn healthz_is_ok_even_when_a_role_has_stopped() {
        let response = get(state(unreachable_pool(), Some("shutting down")), "/healthz").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body(response).await, r#"{"status":"ok"}"#);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn readyz_is_ok_when_roles_run_and_the_schema_is_current(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();

        let response = get(state(pool, None), "/readyz").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body(response).await, r#"{"status":"ready"}"#);
    }
}
