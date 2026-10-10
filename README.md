# Zirv Flow

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

Zirv Flow is a self-hosted marketing automation tool written in Rust. Marketers
connect internal and external data sources, segment the people in their business
from that data, and run automation flows for everyone or for a segment.

## Focus areas

- **Segmentation** -- define and maintain segments of people from the data you
  connect.
- **Data sources** -- bring internal and external data into one place.
- **Integrations** -- reach people and systems through the channels and tools
  you already use.

## Status

Pre-alpha. The project is at the architecture and roadmap stage; configuration,
database bootstrap and the HTTP server skeleton are implemented, but nothing is ready to use yet.

- [Architecture](docs/architecture.md)
- [Roadmap](docs/roadmap.md)

## Configuration

Pass an optional TOML file with `zirv-flow --config <path>`, or set
`ZIRV_FLOW_CONFIG`. Environment overrides use the case-insensitive form
`ZIRV_FLOW_<SECTION>__<KEY>` and take precedence over the file, for example
`ZIRV_FLOW_DATABASE__MAX_CONNECTIONS=20`.

| Key | Type | Default | Rule |
|-----|------|---------|------|
| `server.bind` | socket address | `127.0.0.1:8080` | must parse |
| `database.url` | URL, secret | required | scheme `postgres` or `postgresql` |
| `database.max_connections` | u32 | 10 | at least 1 |
| `database.migrate_on_start` | bool | false | none |
| `worker.concurrency` | u32 | 4 | at least 1 |

Run `zirv-flow config check` to validate the effective configuration and print
it as TOML with the database password redacted.

## Requirements and database migrations

Zirv Flow requires PostgreSQL 16 or newer. Apply pending migrations before
starting a role:

```sh
zirv-flow migrate
```

Alternatively, set `database.migrate_on_start = true`. Role startup refuses a
database with pending migrations when this setting is false, a partially
applied migration, or a schema newer than the running binary.

## HTTP server

`zirv-flow serve` (and `all`, until the worker and scheduler roles exist) runs
the startup database check, binds `server.bind` and serves until Ctrl-C or
SIGTERM, draining in-flight requests.

| Endpoint | Behaviour |
|----------|-----------|
| `GET /healthz` | Liveness; always 200 and never touches the database. |
| `GET /readyz` | 200 when the database is reachable and its schema is up to date, otherwise 503. |
| `GET /metrics` | Prometheus text, including `zirv_flow_http_requests_total` and `zirv_flow_http_request_duration_seconds`. |
| `GET /api/v1/version` | The running version. |

Errors are `application/problem+json` (RFC 9457). Every response carries an
`x-request-id`; a valid incoming one (1 to 128 characters of `A-Za-z0-9._-`) is
kept, otherwise one is generated.

The OpenAPI 3.1 document is committed at `crates/zirv-flow-api/openapi.json`,
and a test fails when it drifts from the code. After changing an API
annotation, regenerate it with:

```sh
ZIRV_FLOW_BLESS=1 cargo nextest run -p zirv-flow-api openapi
```

## Development

Install the stable Rust toolchain and [cargo-nextest](https://nexte.st), then:

```sh
cargo build
```

Before opening a PR, run the four gates:

```sh
cargo build
cargo nextest run --no-fail-fast
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

PostgreSQL-backed tests are ignored by default. To run them against the two CI
versions locally:

```sh
docker run --name zf-pg16 -e POSTGRES_PASSWORD=postgres -p 55416:5432 -d postgres:16
DATABASE_URL=postgres://postgres:postgres@localhost:55416/postgres cargo nextest run --run-ignored only --no-fail-fast

docker run --name zf-pg18 -e POSTGRES_PASSWORD=postgres -p 55418:5432 -d postgres:18
DATABASE_URL=postgres://postgres:postgres@localhost:55418/postgres cargo nextest run --run-ignored only --no-fail-fast
```

## Contributing, security, and license

- [Contributing](CONTRIBUTING.md) and the [Code of Conduct](CODE_OF_CONDUCT.md)
- [Security policy](SECURITY.md) -- report vulnerabilities privately
- [Disclaimer](DISCLAIMER.md) -- operator responsibilities for a self-hosted tool
- [MIT License](LICENSE)
