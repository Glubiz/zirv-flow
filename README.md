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
database bootstrap, the HTTP server skeleton, local accounts with workspaces,
sessions and API keys, and the durable job queue and scheduler are implemented, but nothing is ready to use yet.

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
| `server.cookie_secure` | bool | true | none; set false only for plain-HTTP development |
| `database.url` | URL, secret | required | scheme `postgres` or `postgresql` |
| `database.max_connections` | u32 | 10 | at least 1 |
| `database.migrate_on_start` | bool | false | none |
| `worker.concurrency` | u32 | 4 | at least 1 |
| `worker.lease_seconds` | u32 | 30 | at least 1 |
| `worker.max_running_per_workspace` | u32 | unlimited | at least 1 when set |

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

`zirv-flow serve` (and `all`) runs the startup database check, binds
`server.bind` and serves until Ctrl-C or SIGTERM, draining in-flight requests.

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

## Background jobs

Background work runs through a durable job queue in PostgreSQL. Three roles
use it:

| Command | What it does |
|---------|--------------|
| `zirv-flow worker` | Claims and runs jobs, `worker.concurrency` at a time. Run as many as you like. |
| `zirv-flow scheduler` | Elected leader (an advisory lock; the others stand by and take over within seconds): enqueues due cron schedules, returns jobs with expired leases to the queue and exports queue gauges. |
| `zirv-flow all` | `serve`, `worker` and `scheduler` in one process (the default). |

A running job holds a lease of `worker.lease_seconds`, which its worker extends
every third of that while the handler runs. A worker that dies leaves its job to
the scheduler, which makes it runnable again once the lease has expired; a job
that fails is retried with exponential backoff and jitter, and is kept as
`dead` after its last attempt. A job's database writes commit together with its
completion, so a job never completes twice. Work that reaches outside the
database must be idempotent: a crash can run a job again. A workspace is limited
to `worker.max_running_per_workspace` running jobs when that is set, so one busy
workspace cannot take every worker.

The worker runs on pools of its own, so HTTP traffic and running jobs cannot
starve a heartbeat: it opens up to `2 * worker.concurrency + 1` connections (one
pool for claims and job transactions, one for heartbeats), in addition to
`database.max_connections`. The `LISTEN` connection and the scheduler's leader
lock each take one connection from `database.max_connections`, as does all HTTP
and admin work, so keep it above a few.

On `SIGTERM` or Ctrl-C a worker stops claiming and has 30 seconds to finish
running jobs; any still running are left to lease expiry. `/metrics` carries
`zirv_flow_jobs_total{kind,outcome}`, `zirv_flow_job_duration_seconds{kind}`,
`zirv_flow_jobs_ready` and `zirv_flow_jobs_running`.

## Accounts, workspaces and authentication

People sign in with a local account (e-mail and password, hashed with argon2id).
One account is the instance **owner**; the owner administers every workspace.
Other accounts are members or admins of individual workspaces. Create the owner
once, from the command line (the first-run UI setup arrives with the UI shell):

```sh
printf '%s
' "$OWNER_PASSWORD" | zirv-flow admin create-owner --email owner@example.com
printf '%s
' "$NEW_PASSWORD"   | zirv-flow admin reset-password --email someone@example.com
```

Both commands read the password from one line of standard input (never from an
argument), connect, run the same startup schema check as `serve`, and exit 1 on
any error. `reset-password` also clears a lockout and ends the user's sessions.
Passwords are 12 to 1024 characters.

- **Sessions.** `POST /api/v1/session` with `{email, password}` returns the user
  and a `csrf_token` and sets the `zf_session` cookie (HttpOnly, SameSite=Lax,
  Secure unless `server.cookie_secure` is false). Sessions expire after 12 hours
  idle or 7 days in total. `GET /api/v1/session` shows the current user and CSRF
  token; `DELETE /api/v1/session` signs out.
- **CSRF.** A request authenticated by the cookie that uses POST, PUT, PATCH or
  DELETE must send the token in an `x-csrf-token` header, or it gets 403. Sign-in
  needs none.
- **API keys.** A workspace admin creates a key with scopes (`ingest`, `read`,
  `write`, `admin`); the response shows the `zfk_...` key once, and only a hash
  is stored. Send it as `Authorization: Bearer <key>`. A key works only in its
  own workspace and never on instance-level endpoints (`/users`, creating
  workspaces, `/session`). `read` allows GETs, `admin` allows members and keys;
  scopes do not imply one another.
- **Sign-in throttling.** Five failures in a row lock the account for
  `min(2^(failures-5) seconds, 15 minutes)`. An unknown e-mail, a wrong password
  and a locked account give the same 401.
- **Isolation.** A caller with no access to a workspace gets 404 for it, never
  403. Workspace members can read the workspace and its member list; admins and
  the owner also manage members and keys.
- **Audit log.** Sign-ins (including failures), sign-outs, user, workspace,
  membership and key events and password resets are appended to `audit_log`.
  Rows hold ids, the request id and a small summary, never e-mails, passwords,
  tokens or key secrets. The log is append-only: the code offers no update or
  delete.

| Endpoint | Who |
|----------|-----|
| `POST/GET/DELETE /api/v1/session` | anyone / signed in / signed in |
| `GET/POST /api/v1/users` | owner |
| `GET /api/v1/workspaces`, `POST /api/v1/workspaces` | signed in / owner |
| `GET /api/v1/workspaces/{ws}`, `.../members` | members, admins, owner, `read` keys |
| `PUT/DELETE /api/v1/workspaces/{ws}/members/{user_id}` | workspace admins, owner, `admin` keys |
| `GET/POST /api/v1/workspaces/{ws}/api-keys`, `DELETE .../api-keys/{id}` | workspace admins, owner, `admin` keys |

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
