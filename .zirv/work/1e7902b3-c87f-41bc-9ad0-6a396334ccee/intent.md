# Intent

## Problem

Issue #5. The `serve` role does nothing. zirv-flow has no HTTP server, no
health or readiness endpoints, no metrics endpoint, no `/api/v1` surface and no
OpenAPI document.

## Desired outcome

- An axum server in `zirv-flow-api`, started by `serve`. `all` runs it too, until
  the worker and scheduler arrive in #7.
- Endpoints:
  - `/healthz`: liveness.
  - `/readyz`: 200 only when the database is reachable and the schema is up to
    date, otherwise 503.
  - `/metrics`: Prometheus text.
- An `/api/v1` skeleton described by a committed OpenAPI 3.1 document (utoipa).
  A test fails when the committed document drifts from the code.
- Errors are `application/problem+json` (RFC 9457).
- Every response carries an `x-request-id`.
- SIGTERM or Ctrl-C drains in-flight requests, then exits.

## Constraints

- Metrics go through the `metrics` facade. The binary installs one
  `metrics-exporter-prometheus` recorder (my decision; the supervisor was
  unavailable). Metric names use the `zirv_flow_` prefix.
- HTTP code stays in the api crate. The binary only composes.
- Version 0.5.0. No competing product is named anywhere.

## Open questions

None.

## Acceptance criteria

- [ ] `/readyz` returns 200 against an up-to-date database. It returns 503 with a
      problem+json body when the database is unreachable or the schema is not up
      to date.
- [ ] Changing an API annotation without regenerating the document fails
      `cargo nextest run`, and so fails CI.
- [ ] An unknown `/api/v1` path returns a 404 problem+json body that carries the
      request id.
- [ ] `/metrics` serves Prometheus text that includes
      `zirv_flow_http_requests_total`.
- [ ] `zirv-flow serve` binds `server.bind` and shuts down cleanly on Ctrl-C.
- [ ] build, nextest, fmt, clippy, crate-deps and test-presence pass. CI is
      green, including the PostgreSQL jobs.
