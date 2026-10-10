//! HTTP API for zirv-flow.

pub mod admin;
mod auth;
mod cookies;
mod credentials;
mod health;
mod json;
mod keys;
mod members;
mod metrics;
mod openapi;
mod passwords;
mod problem;
mod request_id;
mod session;
mod state;
mod users;
mod version;
mod workspaces;

#[cfg(test)]
mod acceptance;

use std::future::Future;
use std::io;

use axum::Router;
use axum::middleware::from_fn;
use axum::routing::{get, put};
use tokio::net::TcpListener;

pub use openapi::openapi;
pub use problem::Problem;
pub use state::AppState;

/// Builds the router for every HTTP surface of the server.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .route("/metrics", get(metrics::render))
        .route("/api/v1/version", get(version::version))
        .route(
            "/api/v1/session",
            get(session::current)
                .post(session::sign_in)
                .delete(session::sign_out),
        )
        .route("/api/v1/users", get(users::list).post(users::create))
        .route(
            "/api/v1/workspaces",
            get(workspaces::list).post(workspaces::create),
        )
        .route("/api/v1/workspaces/{ws}", get(workspaces::get))
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
        .fallback(problem::not_found)
        .method_not_allowed_fallback(problem::method_not_allowed)
        .layer(from_fn(metrics::record))
        .layer(from_fn(request_id::assign))
        .with_state(state)
}

/// Serves the router on `listener` until `shutdown` completes, draining in-flight requests.
pub async fn serve(
    listener: TcpListener,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    use axum::body::Body;
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
        get(state.clone(), "/no/such/path/1234").await;
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
        assert!(!body.contains("1234"));
        assert!(body.contains("zirv_flow_http_request_duration_seconds"));
    }
}
