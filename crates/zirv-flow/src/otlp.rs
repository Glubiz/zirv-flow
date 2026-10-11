//! Optional OTLP trace export. Off unless `[log.otlp]` is configured, and then the only
//! thing it adds to the subscriber is a layer that turns the `tracing` spans into OTel
//! spans, keyed by the zirv trace id.

use std::cell::Cell;
use std::time::Duration;

use opentelemetry::trace::{SpanBuilder, SpanId, TraceContextExt, TraceId, Tracer, TracerProvider};
use opentelemetry::{Context, KeyValue, Value};
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{
    IdGenerator, RandomIdGenerator, SdkTracer, SdkTracerProvider, Span as SdkSpan,
    TracerProviderBuilder,
};
use tracing::warn;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::Registry;

/// The span field both the `http.request` and the `job` span record the zirv trace id in.
const TRACE_ID_FIELD: &str = "trace_id";

/// The collector's path for traces, below the configured base URL.
const TRACES_PATH: &str = "v1/traces";

/// The running exporter. Dropping it does not flush; call [`Otlp::shutdown`].
pub(crate) struct Otlp {
    provider: SdkTracerProvider,
}

impl Otlp {
    /// The exporter for `endpoint`, or none when export is not configured. Nothing is
    /// created, spawned or sent without an endpoint.
    pub(crate) fn build(endpoint: Option<&str>) -> Result<Option<Self>, String> {
        let Some(endpoint) = endpoint else {
            return Ok(None);
        };
        // reqwest takes the process-wide rustls provider; another install is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let url = traces_url(endpoint);
        let exporter = SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(url)
            .build()
            .map_err(|error| error.to_string())?;
        let provider = provider().with_batch_exporter(exporter).build();
        Ok(Some(Self { provider }))
    }

    /// The layer to put under the log layer.
    pub(crate) fn layer(&self) -> OpenTelemetryLayer<Registry, ZirvTracer> {
        tracing_opentelemetry::layer().with_tracer(ZirvTracer(self.provider.tracer("zirv-flow")))
    }

    /// Flushes what is queued and stops the exporter, giving up after `timeout`.
    pub(crate) async fn shutdown(self, timeout: Duration) {
        let provider = self.provider;
        let flush = tokio::task::spawn_blocking(move || provider.shutdown_with_timeout(timeout));
        match tokio::time::timeout(timeout, flush).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => warn!("trace export did not shut down cleanly: {error}"),
            Ok(Err(_)) | Err(_) => warn!(
                "trace export did not flush within {}s; pending spans are dropped",
                timeout.as_secs()
            ),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_provider(provider: SdkTracerProvider) -> Self {
        Self { provider }
    }
}

/// The collector's trace path below the base URL, which may itself have a path.
fn traces_url(endpoint: &str) -> String {
    format!("{}/{TRACES_PATH}", endpoint.trim_end_matches('/'))
}

/// A provider builder with the zirv resource and id generator, before any exporter.
pub(crate) fn provider() -> TracerProviderBuilder {
    let resource = Resource::builder()
        .with_service_name("zirv-flow")
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .build();
    SdkTracerProvider::builder()
        .with_resource(resource)
        .with_id_generator(PinnedTraceIds::default())
}

thread_local! {
    static NEXT_TRACE_ID: Cell<Option<TraceId>> = const { Cell::new(None) };
}

/// Hands out the trace id [`ZirvTracer`] pinned for the span being built, and random ids
/// otherwise.
#[derive(Debug, Default)]
struct PinnedTraceIds(RandomIdGenerator);

impl IdGenerator for PinnedTraceIds {
    fn new_trace_id(&self) -> TraceId {
        NEXT_TRACE_ID
            .take()
            .unwrap_or_else(|| self.0.new_trace_id())
    }

    fn new_span_id(&self) -> SpanId {
        self.0.new_span_id()
    }
}

/// A tracer that gives a root span the trace id from its `trace_id` field, so the exported
/// trace id is the zirv one without inventing a parent span. Spans with a parent keep the
/// parent's trace.
#[derive(Clone, Debug)]
pub(crate) struct ZirvTracer(SdkTracer);

impl Tracer for ZirvTracer {
    type Span = SdkSpan;

    fn build_with_context(&self, builder: SpanBuilder, parent: &Context) -> SdkSpan {
        if !parent.span().span_context().is_valid() {
            NEXT_TRACE_ID.set(zirv_trace_id(&builder));
        }
        let span = self.0.build_with_context(builder, parent);
        NEXT_TRACE_ID.set(None);
        span
    }
}

fn zirv_trace_id(builder: &SpanBuilder) -> Option<TraceId> {
    let field = builder
        .attributes
        .as_ref()?
        .iter()
        .find(|attribute| attribute.key.as_str() == TRACE_ID_FIELD)?;
    let Value::String(text) = &field.value else {
        return None;
    };
    let id = TraceId::from_hex(text.as_str()).ok()?;
    (id != TraceId::INVALID).then_some(id)
}

#[cfg(test)]
mod tests {
    use opentelemetry_sdk::trace::InMemorySpanExporter;
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    fn exporting() -> (Otlp, InMemorySpanExporter) {
        let exporter = InMemorySpanExporter::default();
        let provider = provider().with_simple_exporter(exporter.clone()).build();
        (Otlp::with_provider(provider), exporter)
    }

    #[test]
    fn the_trace_path_goes_below_the_base_url_path() {
        assert_eq!(traces_url("http://h:4318/"), "http://h:4318/v1/traces");
        assert_eq!(
            traces_url("http://h:4318/otlp"),
            "http://h:4318/otlp/v1/traces"
        );
        assert_eq!(
            traces_url("http://h:4318/otlp/"),
            "http://h:4318/otlp/v1/traces"
        );
    }

    #[test]
    fn no_endpoint_builds_nothing() {
        assert!(Otlp::build(None).unwrap().is_none());
    }

    #[test]
    fn a_root_span_exports_under_the_trace_id_in_its_field_and_children_follow() {
        let (otlp, exporter) = exporting();
        let subscriber = tracing_subscriber::registry().with(otlp.layer());
        let id = "0123456789abcdef0123456789abcdef";

        tracing::subscriber::with_default(subscriber, || {
            let root = tracing::info_span!("root", trace_id = id);
            let _entered = root.enter();
            tracing::info_span!("child").in_scope(|| {});
        });

        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 2);
        for span in &spans {
            assert_eq!(span.span_context.trace_id().to_string(), id);
        }
        let root = spans.iter().find(|span| span.name == "root").unwrap();
        assert!(!root.parent_span_is_remote);
        assert_eq!(root.parent_span_id, SpanId::INVALID);
    }

    #[test]
    fn a_span_without_a_usable_trace_id_field_gets_a_random_trace() {
        let (otlp, exporter) = exporting();
        let subscriber = tracing_subscriber::registry().with(otlp.layer());

        tracing::subscriber::with_default(subscriber, || {
            tracing::info_span!("bare").in_scope(|| {});
            tracing::info_span!("bad", trace_id = "not-hex").in_scope(|| {});
        });

        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 2);
        assert!(
            spans
                .iter()
                .all(|span| span.span_context.trace_id() != TraceId::INVALID)
        );
        assert_ne!(
            spans[0].span_context.trace_id(),
            spans[1].span_context.trace_id()
        );
    }

    async fn get(app: axum::Router, traceparent: Option<&str>) -> String {
        use tower::ServiceExt;

        let mut request = axum::http::Request::get("/healthz");
        if let Some(value) = traceparent {
            request = request.header("traceparent", value);
        }
        let response = app
            .oneshot(request.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        response.headers()["x-trace-id"]
            .to_str()
            .unwrap()
            .to_owned()
    }

    fn api(pool: zirv_flow_store::PgPool) -> axum::Router {
        zirv_flow_api::router(zirv_flow_api::AppState::new(
            pool,
            std::sync::Arc::new(String::new),
        ))
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_request_and_the_job_it_enqueued_export_under_the_trace_id_the_response_names(
        pool: zirv_flow_store::PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let (otlp, exporter) = exporting();
        let _logging =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(otlp.layer()));

        let trace_id = get(api(pool.clone()), None).await;
        sqlx::query(
            "INSERT INTO jobs (id, kind, trace_id, cause_kind, cause_id)              VALUES (gen_random_uuid(), 'maintenance.noop', $1::uuid, 'api', 'req-1')",
        )
        .bind(&trace_id)
        .execute(&pool)
        .await
        .unwrap();
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let worker = zirv_flow_jobs::Worker::new(
            pool.clone(),
            zirv_flow_jobs::Registry::built_in(),
            zirv_flow_jobs::WorkerConfig::new(1),
        );
        let running = tokio::spawn(worker.run(stopped));
        for _ in 0..600 {
            let left: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
                .fetch_one(&pool)
                .await
                .unwrap();
            if left == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        stop.send(true).unwrap();
        running.await.unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let exported = |name: &str| {
            spans
                .iter()
                .find(|span| span.name == name)
                .unwrap_or_else(|| panic!("no {name} span in {spans:?}"))
                .span_context
                .trace_id()
                .to_string()
        };
        assert_eq!(exported("http.request"), trace_id);
        assert_eq!(exported("job"), trace_id);
    }

    #[tokio::test]
    async fn a_request_with_a_traceparent_exports_under_the_zirv_trace_with_one_link_to_the_caller()
    {
        let (otlp, exporter) = exporting();
        let _logging =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(otlp.layer()));
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/unused").unwrap();
        let caller_trace = "4bf92f3577b34da6a3ce929d0e0e4736";
        let caller_span = "00f067aa0ba902b7";

        let linked = get(
            api(pool.clone()),
            Some(&format!("00-{caller_trace}-{caller_span}-01")),
        )
        .await;
        let unlinked = get(api(pool), Some("00-nonsense")).await;

        let spans = exporter.get_finished_spans().unwrap();
        let request = |trace: &str| {
            spans
                .iter()
                .find(|span| span.span_context.trace_id().to_string() == trace)
                .unwrap_or_else(|| panic!("no span for {trace} in {spans:?}"))
        };
        let linked = request(&linked);
        assert_eq!(linked.name, "http.request");
        assert_eq!(linked.parent_span_id, SpanId::INVALID);
        assert_eq!(linked.links.links.len(), 1);
        let link = &linked.links.links[0].span_context;
        assert_eq!(link.trace_id().to_string(), caller_trace);
        assert_eq!(link.span_id().to_string(), caller_span);
        assert!(request(&unlinked).links.links.is_empty());
    }

    #[tokio::test]
    async fn an_exporter_for_an_endpoint_builds_inside_the_runtime_and_shuts_down_with_nothing_queued()
     {
        let otlp = Otlp::build(Some("https://127.0.0.1:9/")).unwrap().unwrap();

        otlp.shutdown(Duration::from_secs(5)).await;
    }
}
