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
sessions and API keys, OIDC sign-in (Keycloak is the reference provider), the
durable job queue and scheduler, and structured logs with a trace id that follows
a request into its jobs and a per-workspace decision log are implemented, as are the profile store's people, identifiers and field registry, but nothing is ready to use yet.

- [Architecture](docs/architecture.md)
- [Roadmap](docs/roadmap.md)

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
`ghcr.io/glubiz/zirv-flow:<version>` and `:latest` for linux/amd64 and
linux/arm64. It is a static binary on a distroless base that runs as a non-root
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
`ZIRV_FLOW_DATABASE__MAX_CONNECTIONS=20`.

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
| `database.url` | URL, secret | required | scheme `postgres` or `postgresql` |
| `database.max_connections` | u32 | 10 | at least 1 |
| `database.migrate_on_start` | bool | false | none |
| `worker.concurrency` | u32 | 4 | at least 1 |
| `worker.lease_seconds` | u32 | 30 | at least 1 |
| `worker.max_running_per_workspace` | u32 | unlimited | at least 1 when set |
| `log.filter` | string | `info` | a `tracing` filter directive, such as `warn,zirv_flow_jobs=debug` |
| `log.format` | string | `json` | `json` (one object per line) or `text` (for local development) |
| `log.otlp.endpoint` | URL | none (export off) | `http` or `https` OTLP/HTTP base URL, such as `http://collector:4318` |

### Master key

`serve`, `worker` and `all` need the operator master key and refuse to start
without it (`scheduler`, `migrate`, `config check` and the other `admin`
commands do not). It is 32 random bytes as base64; per-purpose HMAC keys for
identifier and redaction hashes are derived from it with HKDF-SHA256. Create it
once and point `secrets.master_key_file` at it:

```sh
zirv-flow admin generate-master-key --out /etc/zirv-flow/master.key
```

The command writes the file readable only by its owner (mode 0600 on Unix),
refuses to overwrite an existing file, prints the key id (the first 8 bytes of
its SHA-256 as hex, also logged at start as `master key loaded`) and never
prints the key. `--if-missing` keeps an existing valid key and succeeds.
Keep the key and back it up separately from your database backups (a dump and
the key together defeat the point of having a key). Losing or replacing it
breaks suppression matching against hashes already stored; rotation will be
an explicit migration, not a config change.

Run `zirv-flow config check` to validate the effective configuration and print
it as TOML with the database password, the OIDC client secret and the master key redacted.
Supply the client secret as `ZIRV_FLOW_AUTH__OIDC__CLIENT_SECRET` rather than in
a file.

## Logs and traces

The long-running commands (`serve`, `worker`, `scheduler`, `all`) write
structured logs to standard error, as one JSON object per line by default
(`log.format = "text"` for development, `log.filter` to choose levels). One-shot
commands (`migrate`, `config check`, `admin`) print plain text.

Every HTTP request runs in an `http.request` span with `request_id`, `trace_id`,
`method` and the matched `route`, and ends with one `request completed` event
with `status` and `duration_ms` (at `debug` for `/healthz`, `/readyz` and
`/metrics`). Every job runs in a `job` span with `job_id`, `kind`, `workspace_id`,
`attempt`, `trace_id`, `cause_kind` and `cause_id`; payloads are never logged.

The server mints a fresh trace id for each request and returns it in the
`x-trace-id` response header and in the `trace_id` of problem bodies; an incoming
trace header is ignored. Jobs enqueued by the request, and by the jobs that
follow, carry the same trace id. Failed attempts and lease expiries of workspace
jobs are recorded in the decision log, which
`GET /api/v1/workspaces/{ws}/traces/{trace_id}` returns for a member or a `read`
API key (400 for a malformed id, 404 when the workspace has no records for it).

### Tracing export

Set `log.otlp.endpoint` (`ZIRV_FLOW_LOG__OTLP__ENDPOINT`) to send the
`http.request` and `job` spans, and the spans inside them, to an OTLP collector
over HTTP/protobuf (spans go to `<endpoint>/v1/traces`). Exported spans use the
zirv trace id as their trace id, so a trace in the collector matches
`x-trace-id`, the logged `trace_id` and the decision log, and a request and the
jobs it enqueued share one trace. A valid W3C `traceparent` on a request is
attached to its span as a link and never becomes its parent or changes the
trace id. Without the setting nothing is created or sent. Export failures never
fail a request or job; spans are batched, a full queue drops spans, and the
process flushes what is queued (up to 5 seconds) when it shuts down. The log
output is the same with or without export. `log.filter` also decides which
spans are exported, so a filter such as `warn` exports none.

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
| `GET /metrics` | Prometheus text, including `zirv_flow_http_requests_total`, `zirv_flow_http_request_duration_seconds` and `zirv_flow_db_pool_connections{pool,state}`. Only a process that runs `serve` (or `all`) has this endpoint. |
| `GET /api/v1/version` | The running version. |

Errors are `application/problem+json` (RFC 9457). Every response carries an
`x-trace-id` and an
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
`zirv_flow_job_claim_lag_seconds{kind}` and the queue depth
`zirv_flow_jobs{kind,state}` (`ready`, `running`, `dead`), set by the scheduler
leader. A `worker` or `scheduler` process has no HTTP server, so set `ops.bind` (for
example `ZIRV_FLOW_OPS__BIND=0.0.0.0:9100`) to serve `/metrics`, `/healthz` and
`/readyz` from it; `/readyz` is 503 once a role loop has stopped or shutdown has
begun. With `serve` or `all` the same endpoints are on `server.bind` and
`ops.bind` is ignored.

## Accounts, workspaces and authentication

People sign in with a local account (e-mail and password, hashed with argon2id).
One account is the instance **owner**; the owner administers every workspace.
Other accounts are members or admins of individual workspaces. Create the owner
once, from the command line or through the first-run setup below:

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

- **First-run setup.** While no owner exists, `serve` mints a one-time setup
  token and logs it once, as a `warn` line: `no owner exists; finish setup at
  <public_url or http://<bind>>/setup with setup token <token>, or run
  zirv-flow admin create-owner`. `GET /api/v1/setup` answers
  `{"required": true}` until an owner exists. `POST /api/v1/setup` with
  `{token, email, password}` creates the owner and signs them in exactly like
  `POST /api/v1/session` (cookie, user and `csrf_token`); a missing or wrong
  token is 403, an existing owner 409. The token lives only in that process's
  memory (as a SHA-256) and ends when an owner exists by any path or the process
  restarts, which mints a new one. With several `serve` processes each has its
  own token and any one works against its own process; on a multi-instance
  deployment use `admin create-owner`.
- **Jobs administration.** Owner session only; API keys get 403.
  `GET /api/v1/admin/jobs` shows ready, running and dead counts per job kind and
  the age of the oldest runnable job. `GET /api/v1/admin/jobs/dead?limit=&before=`
  lists dead jobs newest first (payloads are never shown), with their
  `trace_id`; `POST /api/v1/admin/jobs/{id}/retry` puts a dead job back in the
  queue under its original trace, and is audited.
- **Sessions.** `POST /api/v1/session` with `{email, password}` returns the user
  and a `csrf_token` and sets the `zf_session` cookie (HttpOnly, SameSite=Lax,
  Secure unless `server.cookie_secure` is false). Sessions expire after 12 hours
  idle or 7 days in total. `GET /api/v1/session` shows the current user and CSRF
  token; `DELETE /api/v1/session` signs out.
- **OIDC sign-in.** With `auth.oidc` configured, `GET /api/v1/session/oidc/start`
  redirects (303) to the identity provider using the authorization-code flow
  with PKCE (S256); the provider sends the browser back to
  `{server.public_url}/api/v1/session/oidc/callback`, which starts the same
  session as a password sign-in and redirects to `server.public_url`. The login
  in progress is stored server-side, single-use and valid for 10 minutes, and is
  tied to the browser by the HttpOnly `zf_oidc_state` cookie. An identity is
  matched by issuer and subject; failing that, by a **verified** e-mail address
  to an existing user, which is then linked. With `auto_create_users`, an
  unknown user with a verified address is created without a password and
  without workspace access: provider roles and groups are not mapped, so an
  admin grants memberships. Any failure is one generic 401 (the reason goes to
  the audit log only). Discovery is lazy and cached for 15 minutes, so the
  server starts while the provider is down; `start` then answers 503. With
  `auth.local.enabled = false`, `POST /api/v1/session` is 404;
  `zirv-flow admin reset-password` still works.
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
| `GET /api/v1/session/oidc/start`, `GET .../oidc/callback` | anyone; 404 unless `auth.oidc` is configured |
| `GET/POST /api/v1/users` | owner |
| `GET /api/v1/workspaces`, `POST /api/v1/workspaces` | signed in / owner |
| `GET /api/v1/workspaces/{ws}`, `.../members` | members, admins, owner, `read` keys |
| `PUT/DELETE /api/v1/workspaces/{ws}/members/{user_id}` | workspace admins, owner, `admin` keys |
| `GET/POST /api/v1/workspaces/{ws}/api-keys`, `DELETE .../api-keys/{id}` | workspace admins, owner, `admin` keys |

## Sign in with Keycloak

Any OpenID Connect provider that publishes discovery works; Keycloak is the
reference. This runs Keycloak 26.8.0 locally on port 8081 and zirv-flow on 8080.

1. Start Keycloak (development mode, admin console at http://localhost:8081):

   ```sh
   docker run -d --name zf-keycloak -p 8081:8080 \
     -e KC_BOOTSTRAP_ADMIN_USERNAME=admin -e KC_BOOTSTRAP_ADMIN_PASSWORD=admin \
     quay.io/keycloak/keycloak:26.8.0 start-dev
   ```

2. Create the realm, a confidential client and a user with a verified e-mail
   (wait until the container logs `Listening on`). Git Bash on Windows needs
   `export MSYS_NO_PATHCONV=1` first:

   ```sh
   kcadm="docker exec zf-keycloak /opt/keycloak/bin/kcadm.sh"
   $kcadm config credentials --server http://localhost:8080 --realm master --user admin --password admin
   $kcadm create realms -s realm=zirv -s enabled=true
   $kcadm create clients -r zirv -s clientId=zirv-flow -s enabled=true \
     -s protocol=openid-connect -s publicClient=false -s secret=zirv-flow-dev-secret \
     -s standardFlowEnabled=true -s directAccessGrantsEnabled=false \
     -s 'redirectUris=["http://localhost:8080/api/v1/session/oidc/callback"]' \
     -s 'attributes."pkce.code.challenge.method"=S256'
   $kcadm create users -r zirv -s username=jane -s email=jane@example.com \
     -s emailVerified=true -s firstName=Jane -s lastName=Doe -s enabled=true
   $kcadm set-password -r zirv --username jane --new-password jane-password
   ```

   The same in the admin console: create realm `zirv`; under Clients create an
   OpenID Connect client `zirv-flow` with Client authentication on, Standard flow
   on (everything else off), valid redirect URI
   `http://localhost:8080/api/v1/session/oidc/callback`, then on its Advanced tab
   set Proof Key for Code Exchange Code Challenge Method to `S256` and copy the
   secret from the Credentials tab; under Users create `jane` with Email
   `jane@example.com`, Email verified on, and a password (Temporary off).

3. Configure zirv-flow (`zirv-flow --config zirv-flow.toml serve`), with the
   database settings as usual. `cookie_secure = false` is for plain-HTTP
   development only:

   ```toml
   [server]
   public_url = "http://localhost:8080"
   cookie_secure = false

   [auth.oidc]
   issuer_url = "http://localhost:8081/realms/zirv"
   client_id = "zirv-flow"
   client_secret = "zirv-flow-dev-secret" # prefer ZIRV_FLOW_AUTH__OIDC__CLIENT_SECRET
   auto_create_users = true               # or create jane first, see below
   ```

4. Open http://localhost:8080/api/v1/session/oidc/start, sign in as `jane`, and
   you land on `http://localhost:8080/` with a session. `GET /api/v1/session`
   shows the user. With `auto_create_users = false`, create the user first
   (`zirv-flow admin create-owner --email jane@example.com`, or
   `POST /api/v1/users`); the verified address links it on first sign-in.

## People and fields

The profile store holds the people of a workspace, the identifiers they are known by, and
the fields (traits) sources fill in on them. Sources are not wired up yet, so for now the
store is exercised through its Rust API and the field registry is the part you can use.

**The field registry** lists every field a workspace knows: a key (`plan_tier`), a type, a
precedence rule and a few flags. Admins manage it under **Fields** in the web UI or through
`GET` and `POST /api/v1/workspaces/{ws}/traits` and `PATCH .../traits/{key}`; any member can
read it. A key is lowercase letters, digits and underscores, starting with a letter. The type
is one of `string`, `number`, `boolean`, `timestamp`, `date` or `string_list` and never
changes: to use another type, archive the field and create a new key. A value that arrives
for a key nobody registered registers it, with the type inferred from the value (never
`timestamp` or `date`), `latest` precedence and `inferred` set. A value that does not fit
the registered type, or that belongs to an archived field, is left out of the record with a
reason; the rest of the record still applies.

**Identifier kinds.** A person is recognised by `email` (trimmed, lowercased), `phone`
(international format only: spaces, dashes, dots and brackets are removed, a leading `00`
becomes `+`, then 8 to 15 digits), `anonymous_id`, or `external:<namespace>` for ids from
another system.

**Merges.** When a record's identifiers match several people, they are merged into the
oldest one: it keeps every identifier and source record and the others point at it. Merging
is refused when the combined identifiers would break a rule. The rules are the same for every
workspace for now (they are not configurable yet): a person holds one value of each
`external:<namespace>` kind, up to 10 emails, up to 10 phone numbers and up to 100
anonymous ids. A refused merge attaches the record to the owner of the strongest kind and
records an **identity conflict** for each other person. Every merge keeps a provenance row
(the identities and source records that moved) and a decision record, and there is no
unmerge. A background job (`person.merge.rewrite`) then re-points what the merge did not
move; reading a merged person follows it to the person it was merged into.

**Conflict review.** Admins work through conflicts under **Conflicts** in the web UI, which
shows the two people side by side with the shared identifier marked and offers two outcomes:
keep the people separate, or move the identifier to one of them (refused when it would break
a rule for that person). Members can read conflicts but see identifier values masked and
cannot resolve them. The API is `GET /api/v1/workspaces/{ws}/identity-conflicts` (filter with
`status=open|resolved`, page with `before`), `GET .../identity-conflicts/{id}` and
`POST .../identity-conflicts/{id}/resolve` with `{"outcome": "keep_separate"}` or
`{"outcome": "move_identifier", "to_person_id": "..."}`. Each resolution is audited and
recorded as a decision.

**Precedence.** When several sources supply the same field for one person, the field's
policy picks the value, and the person shows which source it came from:

- `latest`: the most recently observed value wins (the default).
- `source_priority`: the first source in the field's source order that has a value wins;
  sources not listed rank after the list, by `latest`.
- `first`: the earliest observed value wins.

Sending `null` for a field removes that source's value, and the next source's value
(if any) takes over.

Changing a field's precedence, source order or archived flag also re-evaluates the people
who already hold a value for it, through a background job. An archived field is no longer
shown on people; unarchiving brings its values back.

## Web UI

The server serves a React single-page app (`web/`, TypeScript and Vite) from the same
port as the API. Everything that is not `/api`, `/healthz`, `/readyz` or `/metrics`
is the app: files are served as built, and any other path falls back to `index.html`
so deep links work. An unknown `/api/...` path is still a JSON problem, never the app.
The sign-in screen asks `GET /api/v1/session/methods` which of local and SSO sign-in
are enabled.

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
and `npm run gen:api:check` fails when it is stale. The Docker image and the release
build compile the app first, so released binaries always carry it.

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
