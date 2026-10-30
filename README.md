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

Beta: M0–M7 are implemented and M8 beta hardening is in progress. The project is
preparing v1.0.0; the final release and the milestone's validation criteria are
still pending. See the [roadmap](docs/roadmap.md#m8--beta-hardening-and-v10) for
release requirements and [architecture](docs/architecture.md) for the design.

## Documentation

- [Getting started](docs/guide/getting-started.md): setup, a first newsletter and its trace.
- <a id="sources"></a>[Sources](docs/guide/sources.md): connect data, map it, run syncs and check freshness.
- <a id="segments"></a>[Segments](docs/guide/segments.md): build audiences, preview, explain and maintain membership.
- [Flows](docs/guide/flows.md): triggers, steps, test mode and running enrollments.
- [Templates](docs/guide/templates.md): edit, test, review and publish content.
- [Channels](docs/guide/channels.md): domains, destinations, delivery and consent.
- [Compliance](docs/guide/compliance.md): product controls and operator responsibilities.
- [API policy](docs/api.md), [contract audit](docs/api-audit.md) and [OpenAPI](crates/zirv-flow-api/openapi.json).
- Operations: [backup and restore](docs/operations/backup-restore.md), [upgrades](docs/operations/upgrade.md), [secrets](docs/operations/secrets.md), [runbooks](docs/operations/runbooks.md).

## Quickstart with Docker Compose

`docker-compose.yml` runs `zirv-flow all` and PostgreSQL 16 with a named data
volume and health checks. A one-shot `zirv-flow-key` service creates the master
key on first start in the `zirv-flow-secrets` volume and keeps it on later
starts; `docker compose down -v` deletes it with the database. From a clean checkout:

```sh
docker compose up -d --build --wait
curl -fsS localhost:8080/readyz
```

Create the first owner. The password is read from one line of standard input,
so it never appears on a command line:

```sh
docker compose exec -T zirv-flow zirv-flow admin create-owner --email you@example.com
```

Run it from an interactive terminal and type the password, or pipe it in from a
file or a secret store. The compose file's database credentials are local-use
defaults (`POSTGRES_USER`, `POSTGRES_PASSWORD` and `POSTGRES_DB` override them;
the password goes into a URL, so keep `@ : / # ?` out of it); change them
before exposing the stack anywhere else. The stack serves plain HTTP on
`127.0.0.1:8080` and applies migrations on start. Session cookies are `Secure`,
which browsers accept over plain HTTP only on `localhost`; to serve another
host, change the published port and put TLS in front, or set
`ZIRV_FLOW_SERVER__COOKIE_SECURE=false` for development. Stop it with `docker compose down`, add `-v` to delete the
database volume as well.

The image is published to GHCR on each release as
`ghcr.io/glubiz/zirv-flow:<version>` and `:latest` for stable releases, or
`:<version>` and `:rc` for prereleases, for linux/amd64 only. Prereleases leave
GitHub Latest and `:latest` on the last stable release. See the
[API gate and release candidate process](CONTRIBUTING.md#api-compatibility-gate).
It is a static binary on a distroless base that runs as a non-root
user, with `zirv-flow all` as its default command and `0.0.0.0:8080` as its bind
address; supply `ZIRV_FLOW_DATABASE__URL` at run time.

The image declares a health check, `zirv-flow healthcheck`, which requests
`/readyz` on `server.bind` and exits 0 only on a 200. A container that runs only
the `worker` or `scheduler` role has no HTTP listener yet, so give it
`healthcheck: { disable: true }` in compose (or `--no-healthcheck` with
`docker run`), or it will be reported unhealthy.

## Configuration

Pass an optional TOML file with `zirv-flow --config <path>`, or set
`ZIRV_FLOW_CONFIG`. Environment overrides use the case-insensitive form
`ZIRV_FLOW_<SECTION>__<KEY>` and take precedence over the file, for example
`ZIRV_FLOW_DATABASE__MAX_CONNECTIONS=20`. See [sizing and PostgreSQL tuning](docs/operations/sizing.md) for capacity, pool budgets and maintenance.

| Key | Type | Default | Rule |
|-----|------|---------|------|
| `server.bind` | socket address | `127.0.0.1:8080` | must parse |
| `server.cookie_secure` | bool | true | none; set false only for plain-HTTP development |
| `server.public_url` | http(s) URL | none | required when `auth.oidc` is set; the URL users reach the server at |
| `ops.bind` | socket address | none (off) | must parse; serves `/metrics`, `/healthz` and `/readyz` for `worker` and `scheduler` processes, ignored when `serve` runs |
| `auth.local.enabled` | bool | true | none; false turns off `POST /api/v1/session` (404) |
| `auth.oidc.issuer_url` | URL | none | https, or http for localhost; setting any `auth.oidc` key turns OIDC on |
| `auth.oidc.client_id` | string | required with `auth.oidc` | not empty |
| `auth.oidc.client_secret` | string, secret | required with `auth.oidc` | not empty; redacted as `***` |
| `auth.oidc.scopes` | list of strings | `["openid", "email", "profile"]` | `openid` is always requested |
| `auth.oidc.auto_create_users` | bool | false | none |
| `secrets.master_key_file` | path | none | file holding the base64 master key; set at most one of the two `secrets` keys; required by `serve`, `worker` and `all` |
| `secrets.master_key` | string, secret | none | the base64 master key itself (`ZIRV_FLOW_SECRETS__MASTER_KEY`); 32 bytes; redacted as `<redacted>` |
| `secrets.previous_master_key_file` | path | none | file holding the base64 key the current one replaces, while a rotation is under way; set at most one of the two `previous_master_key` keys; must differ from the current key |
| `secrets.previous_master_key` | string, secret | none | the base64 previous key itself (`ZIRV_FLOW_SECRETS__PREVIOUS_MASTER_KEY`); redacted as `<redacted>` |
| `database.url` | URL, secret | required | scheme `postgres` or `postgresql` |
| `database.max_connections` | u32 | 10 | at least 1 |
| `database.migrate_on_start` | bool | false | none |
| `worker.concurrency` | u32 | 4 | at least 1 |
| `worker.lease_seconds` | u32 | 30 | at least 1 |
| `worker.max_running_per_workspace` | u32 | unlimited | at least 1 when set |
| `events.retention_days` | u32 | 760 | at least 1; within the two bounds below; see [Retention windows](docs/people.md#retention-windows) |
| `events.min_retention_days` | u32 | 30 | at least 1; the shortest per-name override a workspace may set |
| `events.max_retention_days` | u32 | 1095 | at least 1; the longest per-name override; not below the minimum |
| `events.reject_retention_days` | u32 | 30 | at least 1; how long ingest rejects are kept |
| `retention.messages_days` | u32 | 395 | at least 1; within the two bounds below |
| `retention.messages_min_days` | u32 | 30 | at least 1; the shortest per-workspace message window |
| `retention.messages_max_days` | u32 | 1095 | at least 1; the longest per-workspace message window; not below the minimum |
| `retention.decision_days` | u32 | 395 | at least 1; within the two bounds below |
| `retention.decision_min_days` | u32 | 30 | at least 1; the shortest per-workspace decision-log window |
| `retention.decision_max_days` | u32 | 760 | at least 1; the longest per-workspace decision-log window; not below the minimum |
| `retention.job_history_days` | u32 | 14 | 1 to 90 |
| `retention.audit_days` | u32 | 730 | 90 to 3650 |
| `retention.import_file_days` | u32 | 7 | 1 to 90 |
| `retention.import_reject_days` | u32 | 30 | 1 to 365 |
| `retention.export_hours` | u32 | 24 | 1 to 168 |
| `retention.test_send_months` | u32 | 13 | 1 to 36 |
| `ingest.max_request_bytes` | u64 | 1048576 | at least 1; a larger ingestion body is refused with 413 |
| `ingest.max_batch_items` | u32 | 500 | at least 1; more items in one `batch` request is a 400 |
| `ingest.rate_per_key` | u32 | 10000 | at least 1; items per second each API key may send, per `serve` process |
| `ingest.burst_per_key` | u32 | 20000 | at least `ingest.max_batch_items`; items a key may send at once after a quiet period, per `serve` process |
| `ingest.max_webhook_bytes` | u64 | 262144 | at least 1; a larger webhook body is refused with 413 |
| `ingest.rate_per_hook` | u32 | 50 | at least 1; requests per second each webhook may receive, per `serve` process |
| `ingest.burst_per_hook` | u32 | 200 | at least 1; requests a webhook may receive at once after a quiet period, per `serve` process |
| `ingest.drain_batch_size` | u32 | 500 | at least 1; inbox rows `ingest.drain` applies per transaction |
| `ingest.drain_shards` | u32 | 8 | 1 to 16; concurrent drain loops per workspace in `ingest.drain` |
| `imports.max_file_bytes` | u64 | 536870912 | at least 1; a larger CSV upload is refused with 413 |
| `assets.max_file_bytes` | u64 | 5242880 | at least 1; a larger image upload is refused with 413 (`asset_too_large`) |
| `assets.max_workspace_bytes` | u64 | 1073741824 | at least 1; an upload that would take a workspace's images past this is refused with 413 (`asset_quota_exceeded`) |
| `explorer.interactive_timeout_ms` | u64 | 5000 | at least 1; how long an explorer count, breakdown or page of rows may run before it is cancelled (`503`, code `query_timeout`) |
| `explorer.export_timeout_ms` | u64 | 60000 | at least 1; the same for each page query of a CSV export |
| `explorer.interactive_row_limit` | u32 | 10000 | at least 1; the most buckets or rows an interactive explorer query returns (`truncated` is set when more matched) |
| `explorer.export_row_limit` | u32 | 1000000 | at least 1; the most rows an export writes; a final `#` comment line says when it stopped there |
| `render.pool_size` | u32 | 2 | 1 to 32; render-host child processes, and so renders at once, in each `serve` and `worker` process; see [Render host](docs/operations/runtime.md#render-host) |
| `render.deadline_ms` | u64 | 5000 | 1 to 600000; how long one render may take before its child is killed and replaced |
| `render.memory_limit_mib` | u64 | 512 | 64 to 1048576; address space a child may use (`RLIMIT_AS`); applied on Linux only |
| `privacy.export_max_bytes` | u64 | 268435456 | 1048576 to 4294967296; the most uncompressed JSON one person export may hold; past it the export fails with `export_too_large` |
| `privacy.ledger_dir` | path | unset | a directory on storage shared by every worker where each completed erasure is also written as a file; see [After a restore](docs/people.md#privacy-ledger-dir) |
| `sending.recheck_hours` | u32 | 6 | 1 to 720; hours a sending domain may go without a DNS check of its DKIM, SPF and DMARC records before the scheduler queues another; see [Sending domains](docs/guide/channels.md#sending-domains) |
| `egress.allow` | list of CIDR ranges | `[]` | each entry a CIDR such as `10.0.5.7/32`, with no host bits set; see [Egress](docs/guide/sources.md#egress) |
| `tracking.prefetch_user_agents` | list of strings | `[]` | extra user-agent fragments (case-insensitive) that mark an open as automated, added to the generic markers; see [Tracking and unsubscribe links](docs/guide/channels.md#tracking-and-unsubscribe-links) |
| `ai.providers` | list | `[]` | operator-approved providers and models; see [assistant setup](docs/guide/templates.md#ai-assistant) |
| `ai.rate_limit_per_user_per_minute` | u32 | 20 | at least 1; calls per user across instances |
| `ai.max_repair_attempts` | u32 | 2 | 0 to 5; additional gated calls after an invalid draft |
| `log.filter` | string | `info` | a `tracing` filter directive, such as `warn,zirv_flow_jobs=debug` |
| `log.format` | string | `json` | `json` (one object per line) or `text` (for local development) |
| `log.otlp.endpoint` | URL | none (export off) | `http` or `https` OTLP/HTTP base URL, such as `http://collector:4318` |

## Requirements and database migrations

Zirv Flow requires PostgreSQL 16 or newer. Apply pending migrations before
starting a role:

```sh
zirv-flow migrate
```

Alternatively, set `database.migrate_on_start = true`. Role startup refuses a
database with pending migrations when this setting is false, a partially
applied migration, or a schema newer than the running binary.

For a populated installation, follow the [upgrade guide](docs/operations/upgrade.md): back up,
stop every role, migrate with the new binary, then start. It includes the migration lock review,
rollback procedure and the CI drill from the previous release. Rolling upgrades are not supported.

## Reference index

- <a id="the-file-source"></a>[The file source](docs/guide/sources.md#the-file-source).
- <a id="mapping-reference"></a>[Mapping reference](docs/guide/sources.md#mapping-reference).
- <a id="sync-runs"></a>[Sync runs](docs/guide/sources.md#sync-runs).
- <a id="the-http-manifest-source"></a>[The HTTP manifest source](docs/guide/sources.md#the-http-manifest-source).
- <a id="database-sources"></a>[Database sources](docs/guide/sources.md#database-sources).
- <a id="kafka-sources"></a>[Kafka sources](docs/guide/sources.md#kafka-sources).
- <a id="source-freshness"></a>[Source freshness](docs/guide/sources.md#source-freshness).
- <a id="workspace-credentials"></a>[Workspace credentials](docs/guide/sources.md#workspace-credentials).
- <a id="egress"></a>[Egress](docs/guide/sources.md#egress).
- <a id="inbound-webhooks"></a>[Inbound webhooks](docs/guide/sources.md#inbound-webhooks).
- <a id="connector-runtime"></a>[Connector runtime](docs/guide/sources.md#connector-runtime).
- <a id="csv-import"></a>[CSV import](docs/guide/sources.md#csv-import).
- <a id="mapping-and-manifest-assistant"></a>[Mapping and manifest assistant](docs/guide/sources.md#mapping-and-manifest-assistant).
- <a id="lists"></a>[Lists](docs/guide/segments.md#lists).
- <a id="segment-assistant"></a>[Segment assistant](docs/guide/segments.md#segment-assistant).
- <a id="template-editor-and-delivery-counts"></a>[Template editor and delivery counts](docs/guide/templates.md#template-editor-and-delivery-counts).
- <a id="template-test-sends"></a>[Template test sends](docs/guide/templates.md#template-test-sends).
- <a id="ai-assistant"></a>[AI assistant](docs/guide/templates.md#ai-assistant).
- <a id="sending-domains"></a>[Sending domains](docs/guide/channels.md#sending-domains).
- <a id="tracking-and-unsubscribe-links"></a><a id="automatic-utm-parameters"></a>[Tracking and unsubscribe links](docs/guide/channels.md#tracking-and-unsubscribe-links).
- <a id="email-provider-destinations"></a>[Email provider destinations](docs/guide/channels.md#email-provider-destinations).
- <a id="event-forwarding"></a>[Event forwarding](docs/guide/channels.md#event-forwarding).
- <a id="consent-and-suppressions"></a>[Consent and suppressions](docs/guide/channels.md#consent-and-suppressions).
- <a id="logs-and-traces"></a><a id="tracing-export"></a>[Logs and traces](docs/operations/runtime.md#logs-and-traces).
- <a id="metrics"></a>[Metrics](docs/operations/runtime.md#metrics).
- <a id="http-server"></a>[HTTP server](docs/operations/runtime.md#http-server).
- <a id="background-jobs"></a>[Background jobs](docs/operations/runtime.md#background-jobs).
- <a id="render-host"></a>[Render host](docs/operations/runtime.md#render-host).
- <a id="accounts-workspaces-and-authentication"></a>[Accounts, workspaces and authentication](docs/operations/access.md#accounts-workspaces-and-authentication).
- <a id="sign-in-with-keycloak"></a>[OIDC sign-in](docs/operations/access.md#sign-in-with-oidc).
- <a id="people-and-fields"></a>[People and fields](docs/people.md#people-and-fields).
- <a id="people"></a>[People](docs/people.md#people).
- <a id="contacts"></a>[Contacts](docs/people.md#contacts).
- <a id="events"></a><a id="retention-windows"></a>[Events](docs/people.md#events).
- <a id="event-debugger-and-explorer"></a>[Event debugger and explorer](docs/people.md#event-debugger-and-explorer).
- <a id="deletion-erasure-and-export"></a><a id="privacy-ledger-dir"></a>[Deletion, erasure and export](docs/people.md#deletion-erasure-and-export).
- <a id="ingestion"></a>[Ingestion](docs/people.md#ingestion).
- <a id="master-key"></a><a id="secrets"></a>[Master key and secrets](docs/operations/secrets.md#master-key).

## Web UI

The server serves a React single-page app (`web/`, TypeScript and Vite) from the same
port as the API. API, health, metrics, assets, webhook, tracking and unsubscribe routes are handled
by the server; other paths serve built files or fall back to `index.html`
so deep links work. An unknown `/api/...` path is still a JSON problem, never the app.
The sign-in screen asks `GET /api/v1/session/methods` which of local and SSO sign-in
are enabled.

After sign-in, Home opens for the remembered workspace (or the first available
one). The sidebar groups daily work into Audience, Data, Messaging and Insights;
**Settings** holds workspace setup, sending, integrations, compliance and system
pages. **Back to <workspace name>** returns from Settings to Home. Use the
workspace switcher for **All workspaces**, or **Search** (Cmd/Ctrl+K) to jump to a
page. The sidebar remembers whether it is folded, folds automatically in the
flow editor, and opens from a menu button on narrow screens.

The app is compiled into the binary. `cargo build` embeds `web/dist` when
`web/dist/index.html` exists, and otherwise a small placeholder page from
`crates/zirv-flow-api/web-placeholder/`, so a checkout without Node still builds.
If `/` shows "The web UI was not built", build the app and then the server again:

```sh
cd web
npm ci
npm run build
cd ..
cargo build
```

For UI work, run the server (`cargo run -- all`), then in `web/` run `npm ci` once and
`npm run dev`. The Vite dev server proxies `/api`, `/healthz` and `/readyz` to
`http://127.0.0.1:8080`. `npm run lint`, `typecheck`, `test` and `build` are the checks CI runs;
`npm run gen:api` regenerates the API client from `crates/zirv-flow-api/openapi.json`
and `npm run gen:api:check` fails when it is stale. The Docker image build compiles
the app first, so the published image always carries it.

### Browser tests

Playwright drives Chromium against a test server that runs the end-to-end harness (architecture
section 18). The server is the `zirv-flow-browser-test` binary of the `browser-test` cargo feature,
which refuses to compile unless `ZIRV_FLOW_BROWSER_TEST_BUILD` is set; it is never part of the
release build or the Docker image. It needs a PostgreSQL server (`DATABASE_URL`), on which it
makes and drops its own scratch database.

```sh
npm --prefix web ci
npm --prefix web run build            # the binary embeds web/dist
ZIRV_FLOW_BROWSER_TEST_BUILD=1 cargo build --features browser-test --bin zirv-flow-browser-test
npm --prefix web run e2e:install      # Chromium, once
DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres npm --prefix web run e2e
```

Playwright starts the binary, waits for its `READY` line and generates a password for the run. The
tests are in `web/e2e/`; `ZIRV_FLOW_BROWSER_TEST_PORT` (default 47700) and
`ZIRV_FLOW_BROWSER_TEST_BIN` (the binary, default under `target/debug`) change where it runs.
The run cleans up after itself: a global teardown asks the server to stop (`POST /__test/shutdown`)
which answers once it has dropped its scratch database. CI runs the same steps in the `browser` job.

## Benchmarks

Reports record their environment and limitations; development-machine results are not
reference-hardware guarantees. The load generator is `tools/loadgen`; see the
[benchmark runbook](scripts/bench/README.md).
The [M8 driver](scripts/bench/README.md#13-m8-reference-resource-emulation-issue-393) adds resumable
scale seeds and reference-resource emulation; its Docker smoke run and hardware validation remain outstanding.

- [m2-ingestion](docs/benchmarks/m2-ingestion.md)
- [m4-segments](docs/benchmarks/m4-segments.md)
- [m5-flows](docs/benchmarks/m5-flows.md)
- [m6-send](docs/benchmarks/m6-send.md)
- [m7-render](docs/benchmarks/m7-render.md)

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

CI also gates dependencies with cargo-deny 0.20.2 (`cargo deny --locked check`) and
`npm audit --omit=dev --audit-level=high` in `web/`. Run
`bash scripts/check-dependency-policy.sh` and its `--self-test` mode to check Docker
image digests and advisory review coverage without compiling. The weekly audit
checks `main` for new advisories; see [the dependency policy](SECURITY.md#dependency-audits)
for the reviewed exceptions and their exit conditions.

Parser fuzz properties run in the normal suite on every platform. The separate `fuzz/`
workspace and bounded nightly libFuzzer runs are documented in
[Contributing](CONTRIBUTING.md#parser-properties-and-fuzzing).

PostgreSQL-backed tests are ignored by default. To run them against the two CI
versions locally:

```sh
docker run --name zf-pg16 -e POSTGRES_PASSWORD=postgres -p 55416:5432 -d postgres:16
DATABASE_URL=postgres://postgres:postgres@localhost:55416/postgres cargo nextest run --run-ignored only --no-fail-fast

docker run --name zf-pg18 -e POSTGRES_PASSWORD=postgres -p 55418:5432 -d postgres:18
DATABASE_URL=postgres://postgres:postgres@localhost:55418/postgres cargo nextest run --run-ignored only --no-fail-fast
```

The MySQL source tests also need a MySQL 8.4 server, named by `ZIRV_FLOW_TEST_MYSQL_URL` (CI starts one
as a service next to PostgreSQL). A test that needs a server it was not given fails and names the
variable instead of passing silently:

```sh
docker run --name zf-mysql -e MYSQL_ROOT_PASSWORD=mysql -p 53306:3306 -d mysql:8.4
ZIRV_FLOW_TEST_MYSQL_URL=mysql://root:mysql@127.0.0.1:53306/mysql DATABASE_URL=postgres://postgres:postgres@localhost:55416/postgres cargo nextest run --run-ignored only --no-fail-fast
```

## Contributing, security, and license

- [Contributing](CONTRIBUTING.md) and the [Code of Conduct](CODE_OF_CONDUCT.md)
- [Security policy](SECURITY.md) -- report vulnerabilities privately
- [Threat model](docs/threat-model.md) -- trust boundaries, controls, test evidence and accepted risks
- [Disclaimer](DISCLAIMER.md) -- operator responsibilities for a self-hosted tool
- [MIT License](LICENSE)
