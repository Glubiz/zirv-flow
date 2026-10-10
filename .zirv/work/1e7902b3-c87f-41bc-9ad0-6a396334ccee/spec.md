# Spec: HTTP server, health and OpenAPI skeleton (issue #5)

## API crate (`zirv-flow-api`)

### Dependencies

- axum 0.8
- tokio
- tower-http, with the `request-id` and `util` features
- utoipa 5
- serde and serde_json
- metrics
- `zirv-flow-store`, which is already an allowed edge

### Public surface

- `router(state: AppState) -> axum::Router`.
- `AppState` holds:
  - the `PgPool`;
  - a metrics render callback, `Arc<dyn Fn() -> String + Send + Sync>`, so the api crate does not depend on the exporter.
- `serve(listener: tokio::net::TcpListener, state, shutdown: impl Future) -> io::Result<()>`.
- `openapi() -> utoipa::openapi::OpenApi`.

### Endpoints

| Path | Behaviour |
|------|-----------|
| `GET /healthz` | Returns 200 `{"status":"ok"}` and never touches the database. |
| `GET /readyz` | Runs `zirv_flow_store::schema_state` with a 2 s timeout. `UpToDate` gives 200 `{"status":"ready"}`. Anything else, an error or a timeout gives 503 problem+json, with a `detail` such as `database unreachable` or `database schema is not up to date (pending: 2)`. |
| `GET /metrics` | Returns 200 with `text/plain; version=0.0.4` and the render callback's output. |
| `GET /api/v1/version` | Returns 200 `{"version": "<CARGO_PKG_VERSION>"}`. This is the first `/api/v1` resource. |
| Unknown `/api/v1/*` | 404 problem+json. |

The OpenAPI document covers `/healthz`, `/readyz` and `/api/v1/version`, plus the `Problem` schema.

### Errors

- `ApiError` produces RFC 9457 `application/problem+json` with these fields: `type` (`about:blank` for now), `title`, `status`, `detail`, and the extension `request_id`.
- No error body echoes request input or secrets.

### Request id

- Every request gets `x-request-id`:
  - an incoming valid value is kept: 1 to 128 chars of `[A-Za-z0-9._-]`;
  - otherwise a new UUID v4 is generated.
- The id is echoed on the response and put into problem bodies.

### Metrics

- `zirv_flow_http_requests_total{method,route,status}`, a counter.
- `zirv_flow_http_request_duration_seconds{method,route}`, a histogram.
- `route` is the matched route template. Unmatched requests use the literal `unmatched`, so label cardinality stays bounded.

### OpenAPI drift check

- The committed document is `crates/zirv-flow-api/openapi.json`, pretty-printed with a trailing newline.
- A test compares `openapi()` with it.
- On mismatch the test fails with: `openapi.json is stale; regenerate with ZIRV_FLOW_BLESS=1 cargo nextest run -p zirv-flow-api openapi`.
- With `ZIRV_FLOW_BLESS=1` the test rewrites the file instead of failing.

## Binary

- Installs the `metrics-exporter-prometheus` recorder once, via `PrometheusBuilder::install_recorder`, and passes `handle.render` into `AppState`.
- `serve` and `all` behave the same until #7. Each one:
  1. runs the existing startup gate;
  2. binds `server.bind`;
  3. logs the bound address to stderr;
  4. serves until Ctrl-C, or SIGTERM on Unix;
  5. exits 0.
- A bind failure is `error: server: could not bind <addr>: <io error>`, exit 1.
- `worker` and `scheduler` stay not-implemented.

## Tests

### Router tests

These use `tower::ServiceExt::oneshot` and do not need Postgres:
- healthz;
- readyz against an unreachable lazy pool (`connect_lazy` to `127.0.0.1:1`) gives 503 problem+json;
- unknown `/api/v1/x` gives a 404 problem whose `request_id` equals the response header;
- an incoming valid `x-request-id` is kept, and an invalid one is replaced;
- `/metrics` shows the request counter after one request, using a test recorder handle;
- the OpenAPI drift test.

### Postgres tests

Marked `#[ignore]` and run with `#[sqlx::test]`:
- readyz on a migrated database gives 200;
- readyz on a database with pending migrations gives 503. Use a fixture migrator; this needs a store hook or an `AppState` that takes the schema check. Pick the simplest seam.

### Binary test

- `startup` gating stays covered.

## Docs

- README: the endpoints and how to regenerate the OpenAPI document.
- `.zirv/context/common.md` module map, then regenerate CLAUDE.md and AGENTS.md.

## Version

0.5.0.
