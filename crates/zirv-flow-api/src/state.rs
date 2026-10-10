use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zirv_flow_store::{PgPool, SchemaState, StoreError};

pub(crate) type SchemaCheck = Arc<
    dyn Fn(PgPool) -> Pin<Box<dyn Future<Output = Result<SchemaState, StoreError>> + Send>>
        + Send
        + Sync,
>;

/// Shared state for every handler.
#[derive(Clone)]
pub struct AppState {
    pub(crate) pool: PgPool,
    pub(crate) render_metrics: Arc<dyn Fn() -> String + Send + Sync>,
    pub(crate) schema_check: SchemaCheck,
}

impl AppState {
    /// `render_metrics` returns the Prometheus text exposition, so this crate stays
    /// independent of the exporter.
    pub fn new(pool: PgPool, render_metrics: Arc<dyn Fn() -> String + Send + Sync>) -> Self {
        Self {
            pool,
            render_metrics,
            schema_check: Arc::new(|pool| {
                Box::pin(async move { zirv_flow_store::schema_state(&pool).await })
            }),
        }
    }

    /// Replaces the readiness schema check; lets tests report states the embedded migrator cannot.
    #[cfg(test)]
    pub(crate) fn with_schema_check(mut self, check: SchemaCheck) -> Self {
        self.schema_check = check;
        self
    }
}
