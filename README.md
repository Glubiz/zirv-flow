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
a request into its jobs and a per-workspace decision log are implemented, as are the profile store's people, identifiers, field registry and merges, static lists, consent, topics and suppressions, and the governed event store's schemas, rejects and retention, and ingestion over HTTP with a durable inbox, but nothing is ready to use yet.

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
| `events.retention_days` | u32 | 760 | at least 1; how long events are kept unless their name has an override; within the two bounds below |
| `events.min_retention_days` | u32 | 30 | at least 1; the shortest per-name override a workspace may set |
| `events.max_retention_days` | u32 | 1095 | at least 1; the longest per-name override; not below the minimum |
| `events.reject_retention_days` | u32 | 30 | at least 1; how long ingest rejects are kept |
| `ingest.max_request_bytes` | u64 | 1048576 | at least 1; a larger ingestion body is refused with 413 |
| `ingest.max_batch_items` | u32 | 500 | at least 1; more items in one `batch` request is a 400 |
| `ingest.rate_per_key` | u32 | 10000 | at least 1; items per second each API key may send, per `serve` process |
| `ingest.burst_per_key` | u32 | 20000 | at least `ingest.max_batch_items`; items a key may send at once after a quiet period, per `serve` process |
| `ingest.drain_batch_size` | u32 | 500 | at least 1; inbox rows `ingest.drain` applies per transaction |
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
| `zirv-flow scheduler` | Elected leader (an advisory lock; the others stand by and take over within seconds): enqueues due cron schedules, returns jobs with expired leases to the queue, keeps the decision log and event partitions for this and next month, and exports queue gauges. |
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
starve a heartbeat: it opens up to `3 * worker.concurrency + 1` connections (one
pool for claims, job transactions and the transactions a handler such as
`ingest.drain` commits on its own, one for heartbeats), in addition to
`database.max_connections`. The `LISTEN` connection and the scheduler's leader
lock each take one connection from `database.max_connections`, as does all HTTP
and admin work, so keep it above a few.

On `SIGTERM` or Ctrl-C a worker stops claiming and has 30 seconds to finish
running jobs; any still running are left to lease expiry. The
`ingest.drain` kind (one per workspace) applies that workspace's ingestion inbox; see
[Ingestion](#ingestion). `/metrics` carries
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

## Events

The event store holds the typed events people produce. Each event name has a schema; the
store checks every incoming event against it before it is kept. Events arrive through the
[ingestion API](#ingestion); the schemas, settings and rejects are managed under **Events**
in the web UI.

**Schemas and versions.** A schema lists the properties of one event name: each has a type
(`string`, `number`, `integer`, `boolean`, `timestamp`, `date`, `string_list` or `object`),
may be required, may list allowed values (text only) and may be marked as personal data.
Admins edit a *draft* and *publish* it as the next version; a published version never
changes (the database refuses it), and publishing clears the draft. Until a version is
published the schema is not in force. Admins manage schemas through
`GET` and `POST /api/v1/workspaces/{ws}/event-schemas`, `GET` and `PATCH .../event-schemas/{name}`,
`PUT .../event-schemas/{name}/draft` and `POST .../event-schemas/{name}/versions`; any member
can read them. Event names in paths are percent-encoded.

**Modes.** A schema is in one mode. `allow` accepts an event that breaks it without flagging
it, `warn` accepts and flags it (`validation_state = flagged`), `block` rejects it. The
reasons name the property and the problem (`missing_required`, `wrong_type`, `not_in_enum`,
`unplanned_property`), never a value. Validation sees the values as they were sent, before
redaction.

**Unplanned names.** An event whose name has no published version follows the workspace
policy (`GET` and `PUT .../event-policy`): `draft` (the default) stores it as `unplanned` and
drafts a schema from the observed shape, marked inferred; `reject` refuses it. Inference never
publishes.

**Time.** `occurred_at` is the client timestamp, or the receipt time when there is none. A
timestamp more than `future_skew_seconds` (default 300) ahead of the receipt time is replaced
by it and flagged `clamped`. One more than `lateness_seconds` (default 86400) behind is kept
and flagged `late`, and the person's pending change records the earliest such time
(`late_from`) so time-window segments can be recomputed from it.

**Repeats.** A client `messageId` gives effectively-once storage: only an accepted event
claims it, a repeat is counted and linked to the original rather than stored, and a rejected
message can be sent again once fixed. A message that already has an open reject writes no
second reject.

**Redaction.** Before an event or a reject payload is stored, the policy's rules drop
properties, replace them with a keyed hash (derived from the master key; equal values stay
equal inside one workspace) and truncate IP addresses (the last IPv4 octet is zeroed, an IPv6
address keeps its first 48 bits; `ip` and `context.ip` by default). Paths are dot-separated.

**Rejects.** A refused event is listed under **Rejects** (`GET .../ingest-rejects`, newest
first, filtered by `name` and `reason`, paged with `before`) with its reasons and a masked
sample: only the values of properties the published schema marks as not personal data are
shown, and every value of an event without a published schema is hidden. The stored payload
is kept for a later replay and is never returned.

**Retention.** Events are kept for `events.retention_days`, or a per-name number of days an
admin sets within `events.min_retention_days` and `events.max_retention_days`; rejects for
`events.reject_retention_days`. A daily job (03:17 UTC) deletes what is past its limit in
batches, drops whole monthly partitions older than the longest limit and removes the repeat
records of events that no longer exist. The scheduler leader creates the partitions for this
and next month.
## Consent and suppressions

The profile store also holds what people have agreed to receive, and the addresses that must
never receive anything. Nothing sends messages yet, so this is the data the send checks of a
later milestone will read.

**Consent** is kept per person, channel and topic. The only channel is `email`. A state is
`subscribed`, `unsubscribed`, `pending_confirmation` or `unknown` (no row: nobody has said
anything); `subscribed` and `unsubscribed` are the explicit ones. A person has a
**channel-wide** state and a state per **topic**. A channel-wide `unsubscribed` blocks every
topic. Otherwise a topic's own state is read through its policy: an `opt_in` topic needs
`subscribed`, an `opt_out` topic reaches everyone who is not `unsubscribed`.

Every change is appended to a history with its source (`api`, `ui`, `import`, `merge`,
`unsubscribe_link` or `ingest`), the user behind it, when it happened and free-form evidence
(for example `ip`, `user_agent`, `form` or `text`; a JSON object of at most 4096 bytes written compactly). Writing
the status a person already has changes nothing and leaves no history. History rows cannot be
edited. Read and write a person's consent with
`GET` and `PUT /api/v1/workspaces/{ws}/people/{id}/consent` (`{"channel": "email",
"topic_key": "newsletter", "status": "unsubscribed", "evidence": {...}}`; leave out
`topic_key` for the channel as a whole; `unknown` cannot be set) and read the history with
`GET .../consent/history?limit&before`. Any member or `write` key can record a decision;
the source is `ui` for a session and `api` for a key.

**Merging people** combines their consent for each channel and topic: the most recent
explicit decision wins, and `unsubscribed` wins a tie at the same moment. An explicit
decision beats a state that is not explicit. If the survivor's state changes, the history
gets a `merge` entry naming the merged person, and the merged person's history moves to the
survivor.

**Topics** are what people can subscribe to. Admins manage them under **Topics** in the web
UI or with `GET` and `POST /api/v1/workspaces/{ws}/topics` and `PATCH .../topics/{id}`
(`?archived=true` lists archived ones too). A key is 1 to 64 lowercase letters, digits,
underscores or hyphens and never changes, and neither does the channel; the name, the
description, the policy and the archived flag can change. Every change is audited.

**Suppressions** block an email address or phone number whatever anyone has consented to.
Each reason is one of `hard_bounce`, `complaint`, `manual` or `erased`, and suppression is
per workspace. The value is never stored: a suppression is kept under an HMAC-SHA256 of
`<kind>:<normalized value>`, keyed from the master key (the `identifier` purpose), so it
survives the erasure of the person it came from. This needs the master key; without one the
suppression endpoints answer 503.

- Adding an address that is already suppressed keeps its first reason, except that `erased`
  replaces any other. An `erased` suppression can never be removed (409).
- `POST .../suppressions/check`, `POST .../suppressions` (add) and `POST .../suppressions/remove`
  take `{"kind": "email", "value": "..."}`; they are POSTs so a value never sits in a URL. Checking
  is open to anyone who can read the workspace, changing needs an admin. Notes are at most
  500 characters and must not contain the address.
- `GET .../suppressions?limit&before&reason` lists newest first as the first 8 hex characters
  of the hash, never the full hash or the value.
- `POST .../suppressions/import` takes `text/csv` (at most 10 MiB and 100,000 rows) with the
  header `kind,value` or `kind,value,note`. Add `?dry_run=true` to preview. The answer is
  `{added, existing, invalid: [{row, reason}]}`, where `row` is the line of the file the row
  starts on (the header is line 1; blank lines are skipped but counted, and a line break inside
  a quoted value counts) and `reason` a code such as `invalid_email`; values are never echoed.
- `GET .../suppressions/export` (admin) streams `hash,kind,reason,source,created_at`. Because
  values are not stored, the export holds hashes only.

The web UI has a **Topics** page and a **Suppressions** page (check an address, add, import
with a dry-run preview, export, filter by reason). Non-admins see both read-only. A
per-person consent view comes with the profile page.

## Lists

A list is a named set of people that you fill by hand, from a CSV or through the API. Flow
actions that add people come with flows. Open **Lists** in the web UI to create, rename and
delete lists and to see who is in one. Deleting a list asks first, naming the list and how
many people it holds; it removes the list, its members and their membership history, and the
people stay. A list name is unique in a workspace, ignoring case. Any member of the
workspace can manage lists; an API key needs the `write` scope to change them and `read` to
look at them.

**Adding people.** Three ways, all of which add only people who already exist and never
create one:

- by id: `POST /api/v1/workspaces/{ws}/lists/{id}/members` with `{"person_ids": [...]}`;
- by identifier: the same call with `{"identifiers": [{"kind": "email", "value": "..."}]}`, or
  the **Add a person** form on the list page. Identifiers are normalized like everywhere
  else, and one that no person owns comes back in `not_found` with its position;
- by CSV: `POST .../lists/{id}/members/import` with `Content-Type: text/csv`, a header row
  `kind,value` and one identifier per row, up to 10 MiB and 100,000 rows. Add
  `?dry_run=true` to see `{matched, added, already_member, unmatched}` without changing
  anything; the web UI always previews first. `unmatched` lists the line of the file each skipped row
  starts on (the header is line 1, as an editor shows it; the field is named `row`) with a
  reason code and never echoes a value.

Remove people with `POST .../lists/{id}/members/remove` and `{"person_ids": [...]}`, or tick
them on the list page. One call takes at most 1,000 people.

**Idempotent.** Adding someone who is already in the list, or removing someone who is not,
changes nothing and writes nothing. The member count on a list is exact: it is kept in the
same transaction as every change. A person id that was merged into another is read as the
person it was merged into.

**History.** Every real change writes a row of the person's membership history (list, added
or removed, source, who and the trace) and a `list.membership` decision that holds ids only.
Read it with `GET /api/v1/workspaces/{ws}/people/{id}/lists` (the lists a person is in) and
`GET .../people/{id}/lists/history`. Creating, renaming and deleting a list is audited as
`list.created`, `list.updated` and `list.deleted`; membership changes are not audited.

**Masking.** The member page shows each person's strongest identifier. As with identity
conflicts, workspace admins, the owner and `admin` keys see the value; members and `read`
keys see its kind and a `masked` flag instead.

**Merges.** When two people are merged, the survivor is in every list either of them was in,
keeps the earlier of the two `added_at` times, and the counts follow. The merged person's
history moves to the survivor.

## Ingestion

Applications send profile updates and events over HTTP with a write-only API key. There are
no client SDKs; the API is three JSON endpoints.

**Create a key.** A workspace admin creates a key whose only scope is `ingest`
(`POST /api/v1/workspaces/{ws}/api-keys` with `{"name": "web app", "scopes": ["ingest"]}`).
The response shows the `zfk_...` key once. The key implies its workspace. A key without the
`ingest` scope, and a signed-in session, get 403; a missing or invalid key gets 401. Ingestion
needs no CSRF token.

| Endpoint | Body |
|----------|------|
| `POST /api/v1/ingest/identify` | one profile update |
| `POST /api/v1/ingest/track` | one event |
| `POST /api/v1/ingest/batch` | `{"batch": [item, ...]}`, each item an identify or a track with `"type": "identify"` or `"type": "track"` |

```sh
curl -X POST https://flow.example.com/api/v1/ingest/identify \
  -H "Authorization: Bearer $ZIRV_FLOW_KEY" -H "Content-Type: application/json" \
  -d '{"userId": "u-1042", "identifiers": {"email": "sam@example.com"},
       "traits": {"plan": "pro"}, "messageId": "signup-u-1042"}'

curl -X POST https://flow.example.com/api/v1/ingest/track \
  -H "Authorization: Bearer $ZIRV_FLOW_KEY" -H "Content-Type: application/json" \
  -d '{"userId": "u-1042", "event": "order_placed", "properties": {"total": 59.9},
       "timestamp": "2026-03-01T09:30:00Z", "messageId": "order-7731"}'

curl -X POST https://flow.example.com/api/v1/ingest/batch \
  -H "Authorization: Bearer $ZIRV_FLOW_KEY" -H "Content-Type: application/json" \
  -d '{"batch": [
        {"type": "identify", "userId": "u-1042", "traits": {"plan": "team"}},
        {"type": "track", "userId": "u-1042", "event": "plan_changed", "messageId": "chg-55"}]}'
```

```js
const response = await fetch("https://flow.example.com/api/v1/ingest/track", {
  method: "POST",
  headers: {
    Authorization: `Bearer ${process.env.ZIRV_FLOW_KEY}`,
    "Content-Type": "application/json",
  },
  body: JSON.stringify({
    anonymousId: "a-77",
    event: "page_viewed",
    properties: { path: "/pricing" },
    messageId: crypto.randomUUID(),
  }),
});
if (response.status === 429) {
  const seconds = Number(response.headers.get("Retry-After")); // wait, then resend the same body
}
```

**Fields.** Every item takes the following; any other top-level field is refused with
`unknown_field`, never dropped.

| Field | Meaning |
|-------|---------|
| `userId` | your id for the person; stored as the identifier `external:user_id` |
| `anonymousId` | stored as the identifier `anonymous_id` |
| `identifiers` | an object of identifier kind to value: `email`, `phone`, `anonymous_id` or `external:<namespace>`, such as `{"email": "...", "external:crm": "..."}` |
| `timestamp` | RFC 3339; when it happened. The arrival time when absent |
| `messageId` | 1 to 200 characters; the retry key |
| `traits` | identify only: an object of trait key to value; `null` removes the trait this source set |
| `event`, `properties` | track only: the event name (required, 1 to 128 characters) and an object |

At least one of `userId`, `anonymousId` and `identifiers` is required. Identifiers are checked
and normalised as everywhere else (an e-mail is lower-cased, a phone number is E.164). The
request is checked for structure only; whether an event name is planned or its properties fit
its schema is decided when the event is applied, and a refused event appears under **Rejects**.
If any item is invalid the whole request is a 400 and nothing is stored; the problem body lists
`errors: [{index, field, reason}]` with field names and reason codes, never values.

**Limits.** A body over `ingest.max_request_bytes` is a 413 and more than `ingest.max_batch_items`
items is a 400 (`too_many_items`). Each key has a token bucket of `ingest.burst_per_key` items
refilled at `ingest.rate_per_key` per second, counted in items, in each `serve` process; with
several, a key may send each its own share. A request that does not fit is a 429 with a
`Retry-After` header in whole seconds (at least 1): wait that long and resend it unchanged.
When PostgreSQL is unavailable the answer is 503 and nothing was stored; there is no local
buffer, so retry.

**What 202 means.** `{"accepted": n}` is returned after the items have been appended to a
durable inbox in PostgreSQL. It means durable, not applied: workers, woken at once, apply the
inbox in order, normally within moments. The request's trace id (the `x-trace-id` response
header) is carried into the decisions the items cause, so
`GET /api/v1/workspaces/{ws}/traces/{trace_id}` shows what became of them.

**Retries.** Send the same `messageId` when you resend an item: an event with a `messageId`
that was already stored is counted as a repeat and not stored twice, and a repeated identify
changes nothing. An item without a `messageId` is given a server-generated one when it is
appended, so a retry of the same request by the client without its own `messageId` can store
an event twice. An item that cannot be applied at all becomes an `apply_failed` reject instead
of blocking the workspace's inbox.

**Operation.** The `ingest.drain` job applies up to `ingest.drain_batch_size` inbox rows per
transaction, in arrival order, and deletes them in that transaction. Concurrent workers never
apply a row twice. The scheduler leader enqueues a drain for any workspace whose oldest row
has waited more than 2 seconds, which covers an append that lands as a drain finishes. The
inbox holds the raw item only until it is applied. Metrics: a `serve` process exports
`zirv_flow_ingest_requests_total{endpoint,status}` and `zirv_flow_ingest_items_total{kind}`;
a `worker` process exports `zirv_flow_ingest_applied_total{kind,outcome}` (`valid`, `flagged`,
`unplanned`, `rejected`, `duplicate`, `apply_failed`) and `zirv_flow_ingest_late_total`; the
scheduler leader exports `zirv_flow_ingest_inbox_age_seconds`, the age of the oldest row.

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
