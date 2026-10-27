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
a request into its jobs and a per-workspace decision log are implemented, as are the profile store's people, identifiers, field registry and merges, static lists, consent, topics and suppressions, people search with a profile, a person timeline and a per-person message log (why each message was or was not sent), consent dimensions, a consent policy builder with ordered rules, a person check and version history, sending limits (frequency caps and quiet hours), and the governed event store's schemas, rejects and retention, ingestion over HTTP with a durable inbox, person deletion, erasure and export, CSV imports with mapping, a dry run and checkpointed runs, the live event debugger, explorer and reject replay, native contacts, sources with their streams, a file source, inbound webhooks, sync runs with events and lists (including PostgreSQL, MySQL and Kafka sources), a mapping editor with preview, an HTTP manifest source, a connector runtime (auth, retries, rate limits and pagination), segments (a builder compiled to SQL, full refresh, incremental re-evaluation and explain), the flow engine (versioned flows, a durable runtime with timers, event waits and re-entry rules, operator retry and skip, test mode and a trace inspector), and the AI provider layer and the mapping, manifest, segment and flow assistant (provider settings and encrypted keys, a budget, a rate limit and a content-free call log), and channels and destinations (email templates in code mode, as HTML or MJML, or in a WYSIWYG block editor, rendered in an isolated render host; sending domains with SPF, DKIM and DMARC checks; webhook, SMTP with DKIM signing and provider-manifest destinations with profile sync and erase calls; the send pipeline with consent policies, frequency caps, quiet hours and a destination rate bucket; click and open tracking with unsubscribe pages, UTM parameters and test sends; and event forwarding with retries, dead letters and re-drive), and the template builder (block documents at schema 2 with versioned migrations; brand themes with a workspace default; versioned snippets with where-used; image assets served from `/a/`; per-block visibility conditions; the assistant drafting copy, subject variants, layouts and translations with a render diff; a Send a test page, a Review and publish page with changes, checks and reach, and a Versions and use page), but nothing is ready to use yet.

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
| `secrets.previous_master_key_file` | path | none | file holding the base64 key the current one replaces, while a rotation is under way; set at most one of the two `previous_master_key` keys; must differ from the current key |
| `secrets.previous_master_key` | string, secret | none | the base64 previous key itself (`ZIRV_FLOW_SECRETS__PREVIOUS_MASTER_KEY`); redacted as `<redacted>` |
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
| `retention.messages_days` | u32 | 395 | at least 1; how long ended messages are kept (about 13 months); within the two bounds below; the purge writes the erasure handled markers |
| `retention.messages_min_days` | u32 | 30 | at least 1; the shortest per-workspace message window |
| `retention.messages_max_days` | u32 | 1095 | at least 1; the longest per-workspace message window; not below the minimum |
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
| `render.pool_size` | u32 | 2 | 1 to 32; render-host child processes, and so renders at once, in each `serve` and `worker` process; see [Render host](#render-host) |
| `render.deadline_ms` | u64 | 5000 | 1 to 600000; how long one render may take before its child is killed and replaced |
| `render.memory_limit_mib` | u64 | 512 | 64 to 1048576; address space a child may use (`RLIMIT_AS`); applied on Linux only |
| `sending.recheck_hours` | u32 | 6 | 1 to 720; hours a sending domain may go without a DNS check of its DKIM, SPF and DMARC records before the scheduler queues another; see [Sending domains](#sending-domains) |
| `egress.allow` | list of CIDR ranges | `[]` | each entry a CIDR such as `10.0.5.7/32`, with no host bits set; see [Egress](#egress) |
| `tracking.prefetch_user_agents` | list of strings | `[]` | extra user-agent fragments (case-insensitive) that mark an open as automated, added to the generic markers; see [Tracking and unsubscribe links](#tracking-and-unsubscribe-links) |
| `log.filter` | string | `info` | a `tracing` filter directive, such as `warn,zirv_flow_jobs=debug` |
| `log.format` | string | `json` | `json` (one object per line) or `text` (for local development) |
| `log.otlp.endpoint` | URL | none (export off) | `http` or `https` OTLP/HTTP base URL, such as `http://collector:4318` |

### Master key

`serve`, `worker` and `all` need the operator master key and refuse to start
without it (`scheduler`, `migrate`, `config check` and the other `admin`
commands do not). It is 32 random bytes as base64. It wraps the keys that
encrypt stored secrets (see Secrets below). Create it once and point
`secrets.master_key_file` at it:

```sh
zirv-flow admin generate-master-key --out /etc/zirv-flow/master.key
```

The command writes the file readable only by its owner (mode 0600 on Unix),
refuses to overwrite an existing file, prints the key id (the first 8 bytes of
its SHA-256 as hex, also logged at start as `master key loaded`) and never
prints the key. `--if-missing` keeps an existing valid key and succeeds.
Keep the key and back it up separately from your database backups (a dump and
the key together defeat the point of having a key). Never swap the key in
place: change it with the rotation below.

### Secrets

Connector credentials, OAuth tokens and AI provider keys are stored encrypted.
Each secret gets its own random data key (XChaCha20-Poly1305), and the master
key wraps that data key; the database holds only ciphertext, the wrapped key and
the id of the wrapping master key. A database dump without the master key
reveals no credential, and the API never returns one. A lost master key means
the stored credentials must be entered again.

#### Workspace credentials

A **credential** is a named object that holds the keys and secrets of one external account. It is
entered once, sealed, shared by any number of sources and destinations, rotated in one place, tested,
and shows where it is used. Under `/api/v1/workspaces/{ws}`, all for workspace admins (no access is
404):

| Request | What it does |
|---------|--------------|
| `GET /credential-kinds` | the kinds and their schemas (fields, types, which are secret), so a form can be drawn from data; never a value |
| `GET /credentials`, `POST /credentials` | list the live credentials; create one |
| `GET`, `PATCH`, `DELETE /credentials/{id}` | read it with its `used_by` list; change its name or non-secret `fields`; archive it |
| `PUT /credentials/{id}/secret` | rotate: seal the new secret values, swap them in and delete the old secret in one transaction |
| `POST /credentials/{id}/test` | run the check of the source or destination named in the body (`source_id` or `destination_id`, which must use the credential); answers `{ok, message, code}` with a fixed sentence that names no host, URL or value, and records `last_tested_at` and the outcome |

A schema is a list of fields; each has a `key`, a `label`, a `type` (`string`, `integer`, `enum` or
`url`), `required` and `secret`. The built-in kinds are `api_key` (a key sent as a named header or as
a bearer token), `basic` (username and password), `smtp` (host, port, TLS mode, username and
password), `oauth2_client` (client id, client secret and an optional token URL: a manifest carries
the URL it uses and wins over the credential's), `signing_secret` (the current secret and an
optional `previous_signing_secret` for a rollover), `bearer_token` (a secret `token`) and
`tls_client` (a client certificate and its secret private key, for mutual TLS). A
credential of kind `custom` carries its own schema in the create request, which is how an
implementation described by a manifest gets the same form (see The HTTP manifest source). Create
with `{"kind", "name", "fields", "secret"}`: the non-secret values go in `fields`, every secret value
in `secret`, and a secret value given in `fields` (or the reverse) is refused. All secret fields are
sealed together as one JSON object (purpose `workspace_credential`); the non-secret ones are stored
as plain JSON.

Nothing secret comes back. A credential shows its `fields`, a **hint** per secret field (its last
four characters when the value has at least 16 characters, otherwise nothing, so it is fully masked),
and a **fingerprint**, the redaction HMAC of the sealed object, which the UI uses to warn when the
same secret is entered twice (`same_secret_as`). The audit trail records `credential.created`,
`credential.updated`, `credential.rotated`, `credential.tested` and `credential.archived` with field
names only.

A source or destination takes a `credential_id` instead of inline credential fields, and must use a
credential that holds every credential field its type requires. It opens the current secret right
before each use, so a rotation reaches all of them on their next call and nothing else changes.
Deleting a source does not delete the credential. Archiving is refused with 409 `credential_in_use`
while a live source or destination uses the credential, and an archived credential cannot be
referenced (409 `credential_archived`). Credentials a source or destination had sealed on its own
before this existed became credentials of kind `imported` when the database was migrated: they point
at the same encrypted row, are never re-sealed by the migration, keep working, and are re-sealed as
`workspace_credential` on their next rotation (which must give the same field names). OAuth access
and refresh tokens stay per source, because they are runtime state. In the web UI, **Credentials**
(in the workspace menu) lists, creates, edits, rotates, tests and archives them, and a source's
credential field is a picker with **New credential**.

The identifier, suppression and redaction hashes derive from a stable HMAC root,
not from the master key. The first `serve` or `worker` start stores that root,
sealed like any secret, made from the master key it starts with (the key the
existing hashes were made with), so a rotation never changes a hash. Start once
with only the current key before rotating.

To rotate the master key:

1. Create a new key with `zirv-flow admin generate-master-key`.
2. Set it as `secrets.master_key_file`, and set the old key as
   `secrets.previous_master_key_file`.
3. Restart every `serve` and `worker` process. A process left on the old key cannot
   write secrets once the rotation has run: it is refused with an error until it
   restarts with the current key.
4. Run `zirv-flow admin rotate-master-key`. It re-wraps every data key, and the
   HMAC root, in one transaction, leaves the ciphertexts alone, writes an audit
   entry with the key ids and the count, and prints the count. A second run
   re-wraps 0. It aborts, changing nothing, if a row is wrapped by a key you
   have not configured, and names that key id.
5. Remove the previous key from the configuration and restart.

A start with a previous key configured and no stored HMAC root is refused, as is
a previous key equal to the current one. `rotate-master-key` is refused, changing
nothing, when no HMAC root exists. The rotation holds every row in one
transaction, which suits secrets in the thousands.

Run `zirv-flow config check` to validate the effective configuration and print
it as TOML with the database password, the OIDC client secret and the master key redacted.
Supply the client secret as `ZIRV_FLOW_AUTH__OIDC__CLIENT_SECRET` rather than in
a file.

## Egress

Outbound HTTP calls made for data sources and destinations go through a guarded client that
refuses any destination that is not a public address: loopback, private ranges, link-local,
carrier-grade NAT, cloud metadata, multicast and other special ranges are all blocked, whether the
URL names them directly, a host name resolves to them, or a redirect points at them. A host name
that resolves to a mix of allowed and blocked addresses is refused whole. Proxy environment
variables (`HTTP_PROXY`, `HTTPS_PROXY`) are ignored by this client.

To reach an internal system on purpose, list it in `egress.allow` as CIDR ranges:

```toml
[egress]
allow = ["10.0.5.7/32", "192.168.0.0/16"]
```

or `ZIRV_FLOW_EGRESS__ALLOW='["10.0.5.7/32", "192.168.0.0/16"]'`. Entries are CIDR only (no host
names or ports), and an invalid entry fails startup and `zirv-flow config check` with the key and
the reason. A cloud metadata address such as `169.254.169.254` (or the Azure WireServer, `168.63.129.16`) is unlocked only by an entry
naming exactly that address (`169.254.169.254/32`); a broader range never unlocks it. Each refusal
increments `zirv_flow_egress_denied_total{class}` and logs the host and class. Sign-in with an OIDC
provider is not covered, because the operator configures that URL.

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
| `zirv-flow scheduler` | Elected leader (an advisory lock; the others stand by and take over within seconds): enqueues due cron schedules, returns jobs with expired leases to the queue, keeps the decision log and event partitions for this and next month, expires person exports hourly, deletes import files and rejects past their retention (see [CSV import](#csv-import)), and exports queue gauges. |
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
starve a heartbeat: it opens up to `(2 + ingest.drain_shards) * worker.concurrency + 1`
connections (41 at the defaults, `(2 + 8) * 4 + 1`) (one pool for claims, job transactions and the transactions a handler such as
`ingest.drain` commits on its own, one per drain shard, one for heartbeats), in addition to
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
| `/api/v1/workspaces/{ws}/credentials`, `.../credential-kinds` (see Workspace credentials) | workspace admins, owner, `admin` keys |

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
records of events that no longer exist. Each batch and each partition drop commits separately,
with its handled markers, so a failed run keeps completed work and the next run continues.
The scheduler leader creates the partitions for this and next month.
## Event debugger and explorer

Under **Events** in the web UI, **Debugger**, **Explorer** and **Rejects** show what the event
store did with what was sent.

**Debugger.** A live list of events as they are decided, newest first (`GET
/api/v1/workspaces/{ws}/debugger/stream`, server-sent events, `read` access). Each frame
is an `item` with the outcome (`accepted`, `flagged`, `unplanned`, `rejected` or `duplicate`),
the reasons, the properties and a trace link; filter by source, event name and outcome. The
page can pause and resume, shows a notice when it could not keep up (`event: gap`) and
reconnects from `Last-Event-ID`. It is live: opening it shows nothing from before except the last few seconds. A
workspace has at most 20 streams per server process (more get 429), and the page says when
the server's notifier is down and updates are delayed.
Ingestion writes the feed only while someone watches the workspace (a viewer renews a 30 s
watch every 10 s); with nobody watching it pays nothing extra, and feed rows older than 15
minutes are deleted. The feed is unlogged and holds redacted values only.

**Masking and reveal.** Values are masked per viewer as in the person timeline: only
properties the published schema declares non-PII are shown, and every other value is null and
listed as masked. Owners, workspace admins and `admin` keys may ask for `reveal=true` (the
**Reveal PII** toggle) to see the stored values. They are the values after ingest redaction,
so dropped and hashed properties stay dropped and hashed. Members and `read` keys never see
them.

**Explorer.** `POST /api/v1/workspaces/{ws}/events/explore/count`, `.../breakdown`,
`.../list` and `.../export` (POSTs, so filters stay out of URLs). A query names a time range
of at most 400 days, optionally an event name, a person (merged people included) and
property filters. Operators follow the schema type of the property (`eq`, `ne`, `in`,
`prefix`, `gt`, `gte`, `lt`, `lte`, `exists`); undeclared properties compare as text. The page
draws counts per hour, day or week as an inline SVG bar chart with the same numbers in a table,
a top-50 breakdown of one property, paged rows and a CSV export (audited as `events.exported`
with the shape of the filters and the row count, never values). A caller without access to
values can filter PII and undeclared properties only with `eq` and `exists` and cannot break
them down. Queries always carry the workspace and the time range, run under the `explorer.*`
timeouts, and answer `503` with the code `query_timeout` instead of running unbounded.

**Replay.** After fixing a schema or the settings, an admin can replay rejects from
**Rejects**: select some, or replay everything that matches the filter (at most 10,000). A
preview checks each reject's stored, redacted payload against what is in force now and
predicts the outcome without writing; confirming queues an `ingest.replay` job. An event that
now passes is stored once, flagged `replayed`, with an `event.replayed` decision that records
why. A second replay of the same reject does nothing, and a message id that is already stored
counts as a duplicate. A reject that never named a person cannot be replayed. **Downstream
triggers stay off by default**: replayed events also carry the flag `no_triggers`, which the
event-trigger path (added with flows) must skip; turn on **Emit downstream triggers** only if
people should still receive what these events would have started.

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

**Consent policy.** A workspace has one consent policy: an ordered list of named, deny-only
rules that decide a marketing send at gate 3, after the recorded consent is read. A rule is a
condition in the segment definition format; the send proceeds when no rule matches, and the first
rule that matches denies it and is named on the message (`deciding_rule`) and in the gate's
decision. The built-in `recorded_consent` rule matches when the recorded consent does not allow
the send; it can be moved but not removed, and a workspace with no published policy has only that
rule. Two leaves are valid in a policy only: `consent` with no channel, whose status is `granted`,
`denied` or `none` for the message being sent, and `messages`, which counts the messages sent to
the person on a channel in a window, or reads the time of the last one, filtered by dimension
values (a cooldown is `{"messages": {"channel": "email", "agg": {"fn": "last_time", "op":
"within_last", "value": "P1D"}}}`). Edit the draft with `GET` and `PUT
/api/v1/workspaces/{ws}/consent/policy/draft` (`write` role), publish it with `POST
.../consent/policy/publish` (`admin`; this appends a `definition_change` decision), list the
versions with `GET .../consent/policy/versions`, and ask what the published policy decides for a
person with `POST .../consent/policy/evaluate` (`admin`; the reply is a rule id and name or none,
and no personal data). A rule that no longer compiles, because something it reads was archived
since it was published, denies the send. A transactional message skips the gate. The builder UI
comes later.

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

## Segments

A segment is a named, versioned definition of a group of people, built from traits, events,
lists, consent and other segments. The definition format is the AST in the OpenAPI document
(`SegmentDefinition`). You can save and preview, and a `segment.refresh` job computes the
members: `GET .../segments/{id}/members` and `.../history` read them, and
`POST .../segments/{id}/refresh` (a member or a `write` key) queues a refresh now (202).

- **Create and version.** `POST /api/v1/workspaces/{ws}/segments` with `{name, description?,
  cadence_minutes?, definition}` creates version 1. `PATCH .../segments/{id}` with a
  `definition` saves the next version and makes it current; there is no separate publish, so
  any member (or a `write` key) saves. Versions are immutable: `GET .../segments/{id}/versions`
  and `.../versions/{n}`. `cadence_minutes` is one of 5, 10, 15, 30, 60, 120, 180, 240, 360,
  480, 720, 1440 or 10080 (default 60).
- **Checked at save.** A definition is checked against the workspace's traits, event schemas,
  lists, live segments and consent topics. A failure is 422 `segment_invalid` with
  `issues: [{pointer, code}]`, never a value. A segment that refers to another that refers back
  is 422 `segment_cycle`, with `cycle: [{id, name}]` in path order and a detail such as
  "A → B → A". A reference to a missing or archived segment is `unknown_segment`.
- **Archive.** `POST .../segments/{id}/archive` is refused with 409 `segment_referenced` while a
  current version of another live segment references it. The list hides archived segments
  unless `?archived=true`.
- **Preview.** `POST .../segments/preview` with `{definition, sample_size?}` (20 by default,
  100 at most) runs an unsaved definition on the primary and returns `{count, sample,
  data_age, compiled_at}`. It reads in one read-only snapshot and stops at 20 seconds with 503
  `segment_preview_timeout`. Sample identifiers are masked like the people search. `data_age`
  is the oldest `last_success_at` of the enabled streams whose saved mapping writes a trait,
  event or list the definition reads (including through segments it references), with each
  feeding source; with no feeding stream `as_of` is null and the data is live.
- **Access and audit.** `read` looks and previews; creating, saving and archiving need a
  workspace member or a `write` key. Writes are audited as `segment.created`,
  `segment.updated` (with the version) and `segment.archived`; the audit never holds a
  definition's values.
- **In the web UI.** Open **Segments** to build a segment from nested all, any and not groups and conditions, watch the matching count update as you edit, save versions, see members and history, and ask on a person's profile why they are or are not in a segment.

## Contacts

For a company with no customer system of its own, zirv flow can hold the contacts itself. A
**contact** is a person with a record from the built-in `native` source (stream `contacts`),
identified by your **contact key**: 1 to 256 characters, stored as the identifier
`external:contact_key` and used in URLs. Because a contact is an ordinary person, identity
resolution, precedence, consent, suppression, lists, search and erasure work on it unchanged.
`search` and `bulk` are not valid keys, because they name endpoints.

**The definition.** `GET` and `PUT /api/v1/workspaces/{ws}/contact-definition` read and save
the shape of a workspace's contacts: a list of `{key, required, identifier}`, where `key`
names a field of the [field registry](#people-and-fields), which supplies the type;
`required` makes a contact without it a `422`; and `identifier` (`email`, `phone`,
`anonymous_id` or `external:<name>`) also makes the value an identity of the person, with the
usual normalisation. An identifier field must be a `string` field. Saving needs the workspace
admin role (an `admin` key) and is audited as `contact_definition.updated`; unknown and
archived registry keys come back as a `422` listing each by position. Without a saved
definition a contact needs only its key and accepts any valid registry key; once one is
saved, a contact may hold only the fields it lists. Open **Contacts**, then **Definition**,
in the web UI to edit it.

**Single contacts are synchronous.** Each write is validated against the definition and
applied in the request, in one transaction with its audit entry, so the answer is the stored
contact:

```sh
KEY=zfk_...   # an API key with the write scope
BASE=http://127.0.0.1:8080/api/v1/workspaces/$WS

# create: 201 with the contact, 409 when the key exists
curl -X POST $BASE/contacts -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -H 'idempotency-key: signup-8841' \
  -d '{"key": "c-1001", "fields": {"email": "ada@example.test", "age": 36}}'

# read, list (keyset-paginated by key), search
curl $BASE/contacts/c-1001 -H "authorization: Bearer $KEY"
curl "$BASE/contacts?limit=50&after=<next_after>" -H "authorization: Bearer $KEY"
curl "$BASE/contacts/search?q=c-10" -H "authorization: Bearer $KEY"

# replace all the fields (200), or a JSON merge patch where null removes a field (200)
curl -X PUT $BASE/contacts/c-1001 -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"fields": {"email": "ada@example.test"}}'
curl -X PATCH $BASE/contacts/c-1001 -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"fields": {"age": 37}}'

# delete: 204
curl -X DELETE $BASE/contacts/c-1001 -H "authorization: Bearer $KEY"
```

A missing required field, a wrong type or a field the definition does not list is a `422`
whose `errors` name the field and a reason code (`missing_required`, `wrong_type`,
`unplanned_property`, `invalid_email`, ...) and never echo a value. Removing a required
field with a patch is refused the same way. The key is also kept as the contact's first
identifier; changing the value of an identifier field (by `PUT`, `PATCH` or a bulk upsert)
replaces the identity: the old one leaves the person unless another source's record of the
person also lists it, so it is no longer found by search and can no longer merge people.
If the definition drops an identifier field and no remaining field produces that identifier
kind, a `PATCH` keeps its stored value and identity until a `PUT` or bulk upsert retracts the
field.

**Search.** `GET .../contacts/search?q=` matches a contact key by prefix and an email address
or phone number exactly, after normalisation. The value is then in the URL and the access
logs, so for addresses and numbers use `POST .../contacts/search` with
`{"identifier": {"kind": "email", "value": "..."}}` or `{"q": "..."}`; the web app does.

**Idempotency.** Send an `Idempotency-Key` header (1 to 255 characters) on a create, replace,
patch or delete to make a retry safe. The same key with the same method, path and body
replays the first status and changes nothing, and the response carries `Idempotent-Replayed:
true`; the body is the contact as it is stored now, or `404` if it has since been deleted or
erased, and a delete replays `204`. The same key with a different request is a `422` with the
code `idempotency_key_reused`. Keys are remembered for 24 hours and then forgotten by the
scheduler leader. The table that remembers them (`contact_idempotency`) holds the key, a
hash of the request and the status, and no field value, person id or contact key, so
deleting or erasing a person has nothing to remove there. Without the header an identical repeat of a
replace or patch still writes nothing, because a write that changes nothing is a no-op.

**Bulk upsert is asynchronous.** `POST .../contacts/bulk` takes
`{"contacts": [{"key": "...", "fields": {...}}]}`, at most `ingest.max_batch_items` of them. Every item
is validated first against the definition (or, in a workspace without one, the field registry), as a single write is; one invalid item makes the whole request a `422`
with `errors` by `index`, `field` and `reason`, and nothing is stored. Valid items are appended
to the ingestion inbox as kind `contact` and the answer is `202 {"accepted": n}`. The
`ingest.drain` job applies them in order, replacing the fields of an existing contact (a field the registry has since archived rejects the row), and
the result is the same state the single-contact writes make. A bulk request is limited like
ingestion: the same body size and the same per-key rate, which a signed-in session shares per
user. Use it for imports and use the single endpoints when you need the stored contact in the
answer.

**Permissions and masking.** Reads need the `read` scope and writes the `write` scope (or
any member of the workspace in the app; a session needs the CSRF token for writes). Values
of identifier fields and of fields flagged personal data are shown to the owner, workspace
admins and `admin` keys; members and `read` keys get `null` for them and the field names in
`masked_fields`. That applies to the response of a write too.

**Deleting a contact** retracts its `native` record. What stays of the person follows the
workspace's [orphan policy](#deletion-erasure-and-export): another source's data on the same
person stays, and under the `delete` policy a person left with nothing is deleted with the
contact. Erasing a person removes everything, including the idempotency rows.

## Deletion, erasure and export

Three different things happen to a person's data, and they are kept apart on purpose.

**Removing a contribution.** When a source retracts a record, only that source's contribution
goes. The record is tombstoned, the person's traits are recomputed from the other sources, and
each identity that only that record listed is deleted (together with the conflicts about it).
An identity first seen only through an event is removed too, and the next event with it creates
it again. Events are not tied to a ledger record yet, so removal leaves them. The person is
marked changed. What happens if nothing holds the person any more (no live record, no event and
no explicit consent) is the workspace **orphan policy**: `keep` (the default) or `delete`. Set it
under **Privacy** in the web UI or with `PUT /api/v1/workspaces/{ws}/privacy/settings`
(`{"orphan_policy": "delete"}`; workspace admins, CSRF, audited).

**Deleting a person.** `DELETE /api/v1/workspaces/{ws}/people/{id}` (workspace admins and `admin`
keys; CSRF; audited as `person.deleted` with the id only) deletes the person, the people merged
into them, and every row that refers to them: identities, source records, events in every
partition, dedupe rows, ingest rejects, list memberships and their history, consent and its
history, identity conflicts, merge provenance, pending exports and the person's decision
records. It adds no suppression and no ledger entry, so the same email can create a new person
the next time it is ingested.

**Erasing a person.** Use erasure for a data-subject request. `POST
/api/v1/workspaces/{ws}/privacy/erasures` takes `{"person_id": "..."}` or `{"identifier":
{"kind": "email", "value": "..."}}` (an identifier that matches nobody is a 404) and needs the
master key. It writes a row to the erasure ledger with the keyed hash of every identity of the
person and of the people merged into them (never a value) and enqueues `privacy.erase`. The job
runs in one transaction: it collects the person's group and identities as they are when it runs, adds
their hashes (and the group's ids) to the ledger row, suppresses every email and phone hash as
`erased`, deletes the workspace's undrained ingestion inbox rows with an identifier that hashes to any
of them, so a row drained later cannot create the person again (an `erased` suppression overrides any other reason and can never be removed); it deletes the group exactly as above; it completes the ledger row; and it audits `privacy.erased`
with the request id. A failure rolls everything back and the job is retried; a request whose job
ran out of attempts shows as `failed`. `GET .../privacy/erasures` lists the requests, newest
first, each with the erase call made to every destination that declares erase and handled the person
(`destination_calls`: outcome `pending`, `done` or `failed`, attempts and a fixed error code; admins
only). The `destination.erase` job makes those calls after the erasure commits, once per
destination, with an idempotency key that is stable across retries; a failing destination is retried
and then shown as `failed`, and never blocks the erasure. The ledger is the one place that keeps the
person id of an erased person, by design.

Erasure removes what was held or pending when it ran. Data that arrives while it runs, or after it,
creates a new person, who stays suppressed: the suppression keeps the address from being contacted.

**CSV imports.** The import tables hold no person id, so erasure and deletion do not reach them.
The stored file (`import_chunks`) is deleted 7 days after its import ends, and the rejected
rows' cells (`import_rejects.raw`) 30 days after they were written, so the cells of an erased
person can survive in either until then. Both age out within the one-month window of an erasure
request; see [CSV import](#csv-import).

**After a restore.** Backups keep erased data until they age out, so after restoring a backup run

```sh
zirv-flow admin replay-erasures [--workspace <slug>]
```

before starting the workers. It needs the master key and no password, and prints counts only. For
each completed ledger entry it re-adds the `erased` suppressions, erases every person id the erased group had (the survivor and the people merged
into them) that the restore brought back, including a backup from before a merge, and removes pending inbox rows that name an erased identifier. It is safe to
run again. Keeping a copy of the ledger outside the database is an open question in the
architecture.

**Exporting a person.** `POST .../privacy/exports` (same body as an erasure; workspace admins
only; CSRF; audited as `privacy.export_requested`) enqueues `privacy.export`, which builds one
gzip-compressed JSON document, `zirv-flow.person-export/v1`: `person` holds `profile` (traits
with their sources), `identities`, `source_records`, `events` (every partition, oldest first),
`consent`, `consent_history`, `lists`, `list_history`, `merges` and `identity_conflicts`, with the
data of merged people under the survivor. It is the one place values leave the system, so only
workspace admins can list exports or download one, and every download is audited as
`privacy.export_downloaded`. `GET .../privacy/exports/{id}/download` returns the file for 24 hours
after the export is ready and then answers 410; the scheduler leader drops expired content
hourly. Erasing the person deletes their exports.

Everything above is on the **Privacy** page of the web UI, visible to workspace admins and the
owner. Erasing asks you to type `ERASE` first. Erasure reaches only destinations that declare an
erase call and does not rewrite files outside the database.

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
transaction, in arrival order, and deletes them in that transaction. Rows are bucketed by
record key (the strongest identifier) when they are appended, and the job runs
`ingest.drain_shards` loops at once, one per share of the buckets, so a single workspace is
not bound by one transaction's round trips; the rows of one record key always land in one
loop and still apply in arrival order. A batch is up to `ingest.drain_batch_size` of the oldest rows of the
shard's buckets. Rows a pre-0.29 serve appends (during a rolling
upgrade) carry no bucket; each run buckets them first, so they join their key's queue by id.
Each loop holds one connection per batch, so the worker's pool holds `worker.concurrency *
(1 + ingest.drain_shards) + 1` connections (37 at the defaults). Concurrent workers never apply a row twice. The
scheduler leader enqueues a drain for any workspace whose oldest row has waited more than 2 seconds, which covers an append that lands as a drain finishes. The
inbox holds the raw item only until it is applied. Metrics: a `serve` process exports
`zirv_flow_ingest_requests_total{endpoint,status}` and `zirv_flow_ingest_items_total{kind}`;
a `worker` process exports `zirv_flow_ingest_applied_total{kind,outcome}` (`valid`, `flagged`,
`unplanned`, `rejected`, `duplicate`, `apply_failed`) and `zirv_flow_ingest_late_total`; the
scheduler leader exports `zirv_flow_ingest_inbox_age_seconds`, the age of the oldest row.

## People

Open **People** in the web UI to find a person and see everything the system knows and did
about them. Any member can look; an API key needs the `read` scope.

**Search.** Search by an identifier, matched exactly after the usual clean-up (an email is
trimmed and lowercased, a phone number put in international form), or by the value of one
field. A person that was merged into another is found as the person they were merged into.
The conditions on a field depend on its type: `eq` and `exists` for any type (on a
`string_list`, `eq` means the list holds the value), `prefix` for text, and `gt`, `gte`, `lt`
and `lte` for numbers, timestamps and dates. The API is `POST /api/v1/workspaces/{ws}/people/search`
with exactly one of `{"identifier": {"kind": "email", "value": "..."}}` and
`{"trait": {"key": "plan", "op": "prefix", "value": "pro"}}`, plus `limit` (1 to 200,
default 50) and `after` (the `next_after` of the previous page). It is a POST so the values
stay out of URLs and access logs. Results are `{id, display, updated_at}`, paged by person
id, where `display` is the person's strongest identifier. Searches run under a 5 second
limit; one that takes longer is answered `503` with the problem code `search_timeout`, so
narrow it rather than retry it unchanged. A caller who may not see values (see Masking) can search a PII-flagged field only with `eq` and `exists`; `prefix` and the range conditions on it are refused with `403` and the code `pii_trait_not_searchable`. Equality on a field uses an index; the other
conditions scan the workspace's people until the indexing question (OQ-1) is decided.

**Profile.** `GET /api/v1/workspaces/{ws}/people/{id}` returns the identities grouped by the
source whose live records list them (those no record lists, such as one first seen on an
event, come under `observed`), each field with its type, source and the time that source
observed it, the person's consent and lists (the same as their own endpoints), and the people
merged into this one. An id that was merged into another returns the survivor with
`merged_from` set to the id you asked for. The web UI shows the same on the profile page,
where a member can also record a consent choice for the person.

**Timeline.** `GET .../people/{id}/timeline?limit&before&kinds&all_decisions` returns the
person's events and the decisions the system made about them, newest first, as entries with
`"type": "event"` or `"type": "decision"`, paged with an opaque `before` cursor. Decisions
cover trait changes, merges, consent changes, list membership and identity conflicts. The
people merged into this one are included. `kinds` is `event`, `decision` or both separated by
a comma. The `event.accepted` decision repeats the event entry, so it is left out unless you
pass `all_decisions=true`. Every entry has a `trace_id` that opens in **Traces**
(`GET .../traces/{trace_id}`), where the decisions hold ids and reason codes only.

**Masking.** Identifier values, PII-flagged field values and the PII properties of events
are shown to the owner, workspace admins and `admin` keys. Members and `read` keys see
`masked: true` and no value (the web UI says "Hidden"). An event with no published schema
cannot say which of its properties are personal, so all of its values are hidden from them.
Search is not masked on the way in: a caller who may search can find a person by a value
they already know.

## CSV import

Load people from a CSV file in four steps: upload it, map its columns, check the mapping with
a dry run, then run it. Open **Imports** (under People) in the web UI, or use the API under
`/api/v1/workspaces/{ws}/imports`. Any member of the workspace can import; an API key needs
the `write` scope to change anything and `read` to look. Sessions send the CSRF token as for
every other write.

**The file.** UTF-8 text (a byte order mark is ignored), RFC 4180 quoting (quoted values may
hold the delimiter, quotes written twice and line breaks), a header row of unique, non-blank
names, and the delimiter detected from comma, semicolon and tab unless one is given. The
upload is `POST .../imports?name=...` with `Content-Type: text/csv` and the file as the raw body;
`file_name`, `source`, `stream` and `delimiter` (`comma`, `semicolon` or `tab`) are optional.
The body is streamed, never held whole: it goes into PostgreSQL in 1 MiB chunks while its
SHA-256 and row count are taken, so separate `serve` and `worker` processes need no shared
disk. A file over `imports.max_file_bytes` (512 MiB by default) is a 413, a header with a
blank or repeated name or a file that is not UTF-8 is a 422, and a refused or interrupted
upload keeps nothing: the import exists as `uploading` (hidden from the history) while the body arrives, each chunk is a statement of its own so no transaction stays open, and an upload that breaks off, is refused or sends no byte for 60 seconds (408) is deleted with its chunks. The
answer is the import with its header and the first 20 rows; the cells of those rows are null
and `samples_masked` is true for callers who may not see values (members and `read` keys).
Row numbers are the ones a spreadsheet shows: the header is row 1, every blank line counts as a row, and a quoted value over several lines is one row.

**The mapping.** (The same language serves the sync runs of sources; see the Mapping reference.) `PUT .../imports/{id}/mapping` saves it and moves the import to `mapped`:

```json
{
  "key_column": "customer_id",
  "columns": [
    {"column": "Email", "target": {"type": "identifier", "kind": "email"},
     "transforms": [{"op": "trim"}, {"op": "lower"}]},
    {"column": "Joined", "target": {"type": "trait", "key": "joined_at"},
     "transforms": [{"op": "timestamp", "format": "%d/%m/%Y"}]},
    {"column": "Newsletter", "target": {"type": "list", "list_id": "..."}}
  ]
}
```

A column becomes an `identifier` of a kind (`email`, `phone`, `anonymous_id` or
`external:<namespace>`), a `trait` by registry key (the registry supplies the type), a `list`
(a truthy cell such as `true`, `1`, `yes`, `y`, `t`, `on` or `x` adds the person; `false`, `0`,
`no`, `n`, `f`, `off` and an empty cell do not; anything else is `wrong_type`), or is left out.
Between the cell and the target run the transforms, in order: `trim`, `lower`, `upper`,
`cast` (to a trait type), `timestamp` (a strftime `format` such as `%d/%m/%Y %H:%M`, or `unix`
or `unix_ms`; a value without a time zone is read as UTC), `concat` (the value and other
columns, skipping empty ones, with a separator), `constant`, `value_map` (replaces the values
it lists) and `split` (into a list of trimmed, non-empty strings). That is all: there are no
expressions or scripts. An empty cell leaves its trait out. The mapping is rejected with a 422
that names the rule and the column unless it has at least one identifier target, every column
and list exists, every trait key exists and is not archived, no trait is filled by two columns
and every transform is well formed. It can be saved again until the first run starts; saving
clears an earlier dry run.

**Timestamps.** A value that a mapping reads as a timestamp (a `timestamp` trait target, a
`cast` to `timestamp`, an event time) is RFC 3339, or the SQL text form that PostgreSQL and MySQL
produce: `YYYY-MM-DD HH:MM:SS`, a space or a `T` between the date and the time, then optionally
`.fraction` (1 to 9 digits) and an offset (`+HH`, `-HH`, `+HH:MM`, `-HH:MM` or `+HHMM`). A value
without an offset is **UTC**, so a MySQL `DATETIME` or a `timestamp without time zone` must hold
UTC to be right. The date alone, a time without seconds and a value with trailing text are
`bad_timestamp`; use the `timestamp` transform with a `format` for those. CSV imports read
timestamps with the same leniency as sync runs.

**Dry run.** `POST .../imports/{id}/dry-run` queues the `import.dry_run` job. It reads the
whole file with the mapping and writes nothing but the rejected rows (phase `dry_run`) and the
counters: no person, identifier, field or list changes. Poll `GET .../imports/{id}` for
`status` (`dry_running`, then `dry_run_done`) and the counters `rows_total`, `rows_done`,
`rows_applied` (rows that would be imported) and `rows_rejected`.

**Run.** `POST .../imports/{id}/run` queues the `import.run` job; a dry run is not required.
The file is read in batches of 500 rows. Each batch is one transaction that applies the profile
records, adds the list members (source `import`), writes the batch's rejects (phase `run`)
and advances the `rows_done` checkpoint with the counters, so a crash or a lost lease resumes
from the checkpoint when the job is retried and neither skips nor repeats a row. Poll the
import for progress: `rows_done` of `rows_total`, `rows_applied`, `rows_unchanged` (the row
repeats what is stored, so nothing was written) and `rows_rejected`. `POST .../imports/{id}/cancel`
stops it at the next batch boundary; the batches already applied stay applied. One run per
import: starting a second is a 409. A run whose job ran out of attempts is marked `failed` with `job_failed` (rows already
applied stay applied); upload the file again to finish, which changes only what is missing.

**Idempotency.** Records are written under the import's `source` (default `csv`) and `stream`
(default the file name without its extension). A row's record key is the value of the
`key_column`, or, without one, the file's SHA-256 and the row number. Running the same file
again with the same source and stream therefore changes nothing: the keys are equal and the
content hashes make every write a no-op. A file that changes between runs needs a key column
to update the same records; without one every row of the new file is a new record. Records
are observed at the time the run was started, so with the default precedence a later import wins where fields conflict.
Keep the source and the stream stable for data you import repeatedly.

**Rejected rows and the threshold.** A row the mapping or the profile store refuses is
rejected, not applied, and the import goes on. The reasons are codes: `missing_identifier`,
`invalid_identifier`, `wrong_type`, `bad_timestamp`, `unknown_column`, `invalid_key` (a key
over 512 characters) and `apply_failed`. They name the column and never a value. Once more
than 1,000 rows are done and more than 5% of them were rejected, the run stops as `failed` with
`error_code` `reject_threshold`, so a broken mapping cannot flood a workspace. The batches
applied before it stopped stay applied; fix the mapping and run the file again, which changes
only what is still missing. Other failure codes: `list_not_found` (a mapped list was
deleted), `file_missing` (the file was already deleted), `file_unreadable`, `job_failed` and `abandoned`.

`GET .../imports/{id}/rejects?phase=&limit=&after=` pages through the rejects in row order
with `row_number` and the codes (`limit` 1 to 200). `GET .../imports/{id}/rejects.csv?phase=` downloads them as
`row_number,column,reason`, one line per problem. For the owner, workspace admins and `admin`
keys each row's own cells follow under the file's column names, in a file that may hold
personal data; cells that start like a spreadsheet formula get a leading `'`. Everyone else
who may read gets the codes alone. Each download is audited.

**Retention and personal data.** The cells of rejected rows are the one place the file's
values are kept beyond the people they created: they are admin-only and deleted 30 days after
they were written. The scheduler leader deletes the uploaded file 7 days after an import ends
(succeeded, failed or cancelled) and the rejects after 30 days. An import that is uploaded, mapped or dry-run and untouched for 7 days is cancelled as `abandoned` and loses its file at once, and an upload that never finished (a crash mid-body) is deleted after a day.
The import itself, its mapping and its counters are kept. The import
tables hold no person id, so an erasure does not reach the cells in a reject; they age out. The
audit log records `import.created`, `import.run_started`, `import.cancelled` and
`import.rejects_downloaded` with ids and counts only. No log line, metric label, decision or
audit entry holds a cell, an identifier or a trait value.

To time a large file, run the ignored test against a PostgreSQL instance with
`ZIRV_FLOW_BENCH_ROWS=1000000 cargo nextest run -p zirv-flow-jobs a_big_file --run-ignored all --no-capture`;
it prints the duration and the rows per second.

## AI assistant

Zirv Flow can use a language model to help draft its own artifacts. Everything the assistant
does goes through a provider layer with gates in front of it. The assistant is off until the operator lists a provider and an admin turns it on
for a workspace.

**Deployment configuration.** The operator allowlists the providers, models and base URLs a
workspace can choose from, with each model's context window and prices (in micro-units of your
currency per million tokens, so `3000000` is 3.00). With no providers the assistant is
unavailable and the UI says so.

```toml
[ai]
rate_limit_per_user_per_minute = 20  # default
max_repair_attempts = 2              # default; each repair is a separate, gated call

[[ai.providers]]
id = "openai"
kind = "openai_compatible"           # or "anthropic"
base_url = "https://api.openai.com/v1"
[[ai.providers.models]]
name = "a-model-name"
context_window_tokens = 128000
input_price_per_mtok = 2500000
output_price_per_mtok = 10000000
```

Provider ids and the model names within one provider must be unique, and a base URL is an
http(s) URL without credentials, a query or a fragment. A mistake fails startup and
`zirv-flow config check` with the key and the reason. The OpenAI-compatible adapter also serves
local model servers: point `base_url` at the server. **A local server on a private address is
blocked until its address is in `egress.allow`** (see Egress); the model client never follows
redirects and ignores proxy environment variables.

**Per workspace.** Open **AI** in the web UI (workspace admins and the owner). An admin chooses
the provider and model, stores the provider key, sets a monthly budget and turns the assistant
on. The key is stored encrypted like any other secret (see Secrets) and is never shown again;
this needs the master key. Turning the assistant on, or changing its provider, model or key while
it is on, runs a short check: the model must call a tool with valid arguments and report token
usage, and its configured context window must be at least 32,768 tokens. A model that fails is
refused with the missing capabilities and their reasons. The settings are
`GET` and `PUT /api/v1/workspaces/{ws}/ai/settings`; `GET .../ai/models` lists the allowlist and
`GET .../ai/usage` the month's spend. All three are admin-only, and a settings change is audited
as `ai.settings.updated` with the changed fields and their new values, never the key.

**Privacy.** By default a request carries only field names, types, descriptions and counts. A
request that holds the values of people is refused unless an admin turns on *Allow personal data
in requests*. Pasted documents and samples are fenced and labelled as data, never instructions.
No transcript of a prompt or an answer is stored by default. An admin can opt in to keeping them
for 1 to 30 days (7 is suggested); an hourly job, `maintenance.ai_transcripts`, deletes expired
ones, and erasing a person also deletes the transcripts that name them.

**Budget and rate limit.** Before any request the dispatch checks the assistant is enabled, the
user is under `ai.rate_limit_per_user_per_minute` and the workspace is under its monthly budget
(a calendar month in UTC; a budget of 0 allows no calls). A refused call sends nothing, records
nothing and answers `402` (budget), `429` with `Retry-After` (rate limit), `409` (not enabled) or
`422` (personal data without opt-in). Both counts are read from the call log in the database, so
they hold across instances. Calls in flight reserve the cost of the request's input-token upper
bound and full output cap. A provider that reports usage above either bound can overrun the
budget by the calls in flight; the reported usage is still recorded and counted as a provider
fault. The cost of a call is estimated from the reported tokens and the configured prices.

**The call log.** Every dispatched call writes a row with the workspace, user, feature,
provider, model, token counts, estimated cost, latency and outcome, and never a prompt, an
answer or an error body. The metrics are `zirv_flow_ai_calls_total{feature,outcome}`,
`zirv_flow_ai_refusals_total{reason}` and `zirv_flow_ai_provider_faults_total{feature,reason}`
(`usage_over_reservation` when reported usage exceeds the request's bounds). The tests never
call a real model: they use a fake provider and local mock servers.


### Mapping and manifest assistant

Open **Map** on a stream (in the setup wizard or later) or the manifest editor of an `http` source,
say what you want and, for a manifest, paste the API documentation. The panel is empty while the
assistant is off. The server asks the model to call one draft tool, checks each answer with the
same rules a save applies, and feeds the problems back at most `ai.max_repair_attempts` times. You
get either the problems of the last attempt (no draft, nothing to accept) or a validated draft with
its field-by-field difference from the saved version and a dry run: the mapping preview on five
records (or on your pasted sample, for a webhook stream), or a five-record sample of the
manifest's first stream. A manifest dry run needs a saved source whose base URL and `auth` the draft
keeps; otherwise the panel says why it did not run.

- **Privacy.** With the personal-data opt-in off, only field names, types, trait keys, list names
  and your prompt are sent, and the source is not read for the model. With it on, up to 5 sample
  records (8 KiB) are added. Field names, pasted documentation and records are sent as fenced data
  that the model is told never to follow. Do not paste personal data into the prompt.
- **Accept loads, Save publishes.** Accept checks the draft again and loads it into the editor as
  the ordinary unsaved draft. Saving it stays the existing Save, with its permission and audit. A
  draft expires after 7 days, is yours alone, and goes with its source.
- **Routes.** `POST /api/v1/workspaces/{ws}/ai/drafts` (`kind`, `source_id`, `stream`, `prompt`,
  `documentation`, `sample`) answers 201 with `status` `valid` or `invalid`. It needs an admin and a
  signed-in user (an API key is 403 `ai_user_required`). Problems: 409 `ai_disabled`, 402
  `ai_budget_exceeded`, 429 `ai_rate_limited`, 502 `ai_provider_error`, 422 naming the field
  (`sample_required`, `sample_not_supported`). `POST .../ai/drafts/{id}/accept` answers 200 with the
  content, 409 `ai_draft_invalid` or `ai_draft_outdated` (the saved version changed), 422 when it no
  longer passes a save's rules, 404 for another user's or an expired draft. Accept is audited as
  `ai.draft.accepted`.
- **Metrics.** `zirv_flow_ai_draft_runs_total{kind,outcome}` (`valid`, `invalid`, `failed`),
  `zirv_flow_ai_drafts_accepted_total{kind}`, and the existing `zirv_flow_ai_calls_total` under the
  features `mapping`, `manifest`, `segment`, `segment_explain` and `evaluation`.
- **Timeouts.** A run is one synchronous request of up to (1 + `ai.max_repair_attempts`) calls. A
  reverse proxy needs a read timeout of at least (1 + `ai.max_repair_attempts`) x 60 s + 30 s for
  `POST /api/v1/workspaces/{ws}/ai/drafts`.
- **Evaluation.** `zirv-flow admin evaluate-assistant --workspace <slug> [--case <name>]` runs the
  golden prompts against the workspace's model through the gated dispatcher (every prompt counts
  against the budget) and prints one JSON line per case with the properties that held, the ones
  that did not, attempts, tokens, cost and model. It exits 0 whatever the score. CI runs the same
  cases on a fake provider and never calls a model.

### Segment assistant

On a segment (new or saved), an admin of a workspace with the assistant on sees **Draft with the
assistant** under the builder and **Explain this segment** on the page. Neither is shown to a
member, and neither while the assistant is off.

- **Draft.** Say who should belong. The model calls `draft_segment` with a whole definition, which
  is checked with the rules a builder save applies (the parser, the validator against the
  workspace's catalog and the compiler). Problems go back to it as a code and a JSON pointer, never
  a value, at most `ai.max_repair_attempts` times. A valid draft is shown as a difference from the
  saved version with the live count of the draft. **Accept** loads it into the builder as the
  unsaved copy of that segment, after a dialog if you had unsaved edits to the conditions; **Save**
  publishes it, as always. A segment draft names the segment (or none, for a new one), has no
  source, and stores the SHA-256 of the saved version it was made from.
- **Explain.** `POST /api/v1/workspaces/{ws}/segments/{id}/ai/explain` (optional body
  `{"version": n}`, default the current version) answers `{version, summary, nodes: [{pointer,
  text}]}`. The model calls `explain_segment`; every pointer must be a node of that version, every
  condition must be cited exactly once and the texts are plain text (500 characters per node, 1,000
  for the summary). Nothing is stored. The page shows each condition in the builder's own words
  next to the sentence. 404 for an unknown segment or version, 422 `ai_explain_failed` (with issue
  codes) when the attempts run out.
- **Privacy.** The model is told trait keys, types and descriptions, event names with property
  names and types, list and segment names, consent topics and identifier kinds. It is never told a
  trait or event value, a count, a sample or a member. With the personal-data opt-in off, every
  string literal in the saved definition that is compared with a trait or event property that is
  not declared non-personal (or is not defined) is replaced by a placeholder such as `(kept:1)`
  and put back in the draft, so the model keeps it by leaving the placeholder where it is. A
  placeholder moved to another condition, or made up, is a problem it must fix. With the opt-in the
  definition is sent as it is, fenced as personal data.
- **Accept.** `POST .../ai/drafts/{id}/accept` locks the segments of the workspace the way a save
  does, answers 409 `ai_draft_outdated` when the current version is no longer the one the draft was
  made from, and 409 `ai_draft_invalid` with the issues when the definition no longer validates
  (for example, a field archived since). It writes no segment version and is audited as
  `ai.draft.accepted` with the kind and the segment.
- **Roles.** The same as the other assistants: an admin, signed in (an API key is 403
  `ai_user_required`), with the assistant on. A member who may edit segments does not see the
  panel; saving what was accepted needs write access, as any save does.

## Sources

A source is a configured external system that records are read from. Each source has a
type (a source type is one module plus one line in the source registry of
`zirv-flow-connectors`), a name that is unique in the workspace, its settings, an optional
cron schedule in UTC, and an enabled flag. It offers **streams**, such as a table or an
endpoint; a stream is read only when you turn it on, and you choose its sync mode (full
refresh, or incremental with a cursor field), its primary key and a staleness threshold.
A run reads a stream and applies it to people (see Sync runs below).

Open **Sources** in the web UI. The setup wizard walks through choosing a type, filling in its
settings, saving and testing the connection, and discovering and setting up the streams. The
form is rendered from the type's field specs, listed by `GET /api/v1/source-kinds`. Discovering
again refreshes what a source reports and never changes the settings you chose.

**Who can do what.** Any member or `read` key can list and read sources and streams. Creating,
changing, deleting, testing and discovering a source and setting up its streams need a
workspace admin, the owner, or an `admin` key. Every such change is audited (`source.created`,
`source.updated`, `source.deleted`, `source.tested`, `source.discovered`, `stream.configured`)
with ids, names and field names, never a value.

**Credentials.** A source uses a workspace credential (see Workspace credentials) by
`credential_id`, and shows the id and only the names of the fields the credential holds, never a
value. To use another credential, send a new `credential_id` with `PATCH .../sources/{id}`; to change
a secret, rotate the credential. Deleting a source deletes its streams and its account tokens, and
leaves the credential. A server started without a master key answers 503 where a credential has to
be sealed or opened. A source that calls out does so through the guarded HTTP client and the egress policy (see Egress).

**API.** Under `/api/v1/workspaces/{ws}/sources`: `GET` and `POST`; `GET`, `PATCH` and `DELETE`
on `/{id}`; `POST /{id}/test` (returns `{ok, error}`; a failed test is a 200); `POST
/{id}/discover`; `GET /{id}/streams`; and `PUT /{id}/streams/{name}` with `enabled`,
`sync_mode`, `cursor_field`, `primary_key`, `staleness_threshold_seconds` and `mapping`; and `POST /{id}/streams/{name}/preview` (see the Mapping reference). Settings are
checked against the type and, for streams, against what discovery reported; a refusal is a 422
that names each field, for example `incremental` without a `cursor_field`.

### The file source

The `file` source type reads a CSV, newline-delimited JSON or JSON file at an **https** address
and offers one stream, `records`. Its settings are `url` and `format` (`csv`; `json` for a
top-level array of objects; `ndjson` for one object per line). The address may not hold user
info or a fragment, and `http` is refused. Its credential fields are all optional and sealed: a
`token` (sent as a Bearer token), or a `username` with a `password` (Basic), and a `query`, a
query string appended to the address, so the signature of a presigned link stays out of the
plain settings. A signed link that redirects must be given as its final address: a redirect off
the source's origin is refused, not followed.

**How it reads.** The file is fetched in byte-range windows of 4 MiB through the guarded client
(so the egress policy, retries with `Retry-After` and the per-source rate limit all apply), and
a record is parsed as soon as it is complete, so memory stays bounded whatever the file's size.
The server must answer `Range` requests for a file over 10 MiB; a server that ignores `Range`
is read whole when the file fits the 10 MiB response cap, and otherwise the run fails with
`file_range_unsupported`. The checkpoint is the byte offset of the next record, so a run that
lost its lease resumes there and neither repeats nor skips a row. Every later window asks with
`If-Range`; if the file's `ETag` (or `Last-Modified`) or size changed between windows the run
fails with `file_changed`, keeping the units already committed. A record over 1 MiB fails with
`record_too_large`; text that is not valid CSV or JSON fails with `csv_invalid` or
`json_invalid`. A byte-range answer whose `Content-Range`, length or total contradicts itself or the
request fails with `file_range_invalid`, so a short body is never taken for the end of the file.
These codes never quote the file or its address.

**Fields and keys.** For CSV the fields are the header names (blank or repeated names are
refused), all text. For JSON they are the top-level keys of the first 100 records, typed from
the first value that is not null; nested values are reached with a dotted path in the mapping
(see the Mapping reference). Choose the stream's **primary key** in **Set up**: a record's key
is its value, or several values joined as a JSON array. Without a primary key the key is the
record's position in the file (`row:<n>`), which holds only while the file is not reordered.

**Full refresh only.** A file has no cursor, so every run reads it from the start; the content
hashes make an unchanged row cost nothing, and a row removed from the file is removed, with its
event, by the sweep at the end of the next run (see Sync runs).

### Mapping reference

One mapping language serves the CSV import and the sync runs. A mapping is a list of `columns`
rules, an optional `key_column` and, for sync runs only, an optional `event` and an optional
`delete_when` (see Sync runs); a CSV import refuses both.

- **Fields and paths.** A rule's `column` is a field name. For a JSON stream it may also be a
  dotted path: `address.city` selects a key, and a segment of digits indexes an array, as in
  `items.0.sku`. A top-level field whose name holds a dot wins over the path. A value that is a
  string is used as it is, a number or a boolean as its JSON text, and null, a missing step or
  an array or object as an empty cell. A path is accepted when its first segment is a field the
  stream reports; a path that leads nowhere in one record is an empty cell, not an error.
- **Targets.** `identifier` (`email`, `phone`, `anonymous_id`, `external:<namespace>`), `trait`
  by registry key, `list` (a truthy cell adds the person), or `ignore`. A list only adds: a cell
  that turns false later does not remove the member. In a sync run a list must exist when the
  mapping is saved and when each unit is applied (`list_missing` fails the run).
- **Transforms**, in order: `trim`, `lower`, `upper`, `cast`, `timestamp`, `concat`, `constant`,
  `value_map`, `split`. That is the whole set: there is no expression language and no script.
- **Event** (sync runs only; a CSV import refuses it with `event_not_supported`). One event per
  record: `{"name": "order_placed", "timestamp": {"column": "order.at", "transforms": [...]},
  "properties": [{"property": "total", "column": "order.total", "transforms": [...]}]}`. The
  name is a constant, the person is the record's identifiers, the time goes through its
  transforms and is read as a timestamp, RFC 3339 or SQL text, see Timestamps (without one the event happened when the run
  started), and a property is the chain's value (an empty one is left out; at most 50). The
  stream must have a primary key (`event_requires_primary_key`).

Example, a JSON file of customers with an order each:

```json
{
  "columns": [
    {"column": "profile.email", "target": {"type": "identifier", "kind": "email"},
     "transforms": [{"op": "trim"}, {"op": "lower"}]},
    {"column": "profile.name", "target": {"type": "trait", "key": "display_name"}},
    {"column": "flags.news", "target": {"type": "list", "list_id": "..."}}
  ],
  "event": {
    "name": "order_placed",
    "timestamp": {"column": "order.at", "transforms": [{"op": "timestamp", "format": "%Y-%m-%d"}]},
    "properties": [{"property": "total", "column": "order.total",
                    "transforms": [{"op": "cast", "to": "number"}]}]
  }
}
```

**Preview.** `POST /api/v1/workspaces/{ws}/sources/{id}/streams/{name}/preview` (admins only,
because the answer holds values) reads the first records of the stream from the start, up to
`limit` (1 to 20, default 10) or 30 seconds, and returns for each record its identifiers,
fields, lists and event, or the `{field, reason}` codes that refused it. The body may carry a
draft `mapping`, checked exactly as a save is; otherwise the saved one is used. It writes
nothing, is not audited, and runs the same `core::mapping` function as a sync run. In the web UI
open **Map** on a stream; the Map dialog also has an assistant that drafts a mapping for you (see Mapping and manifest assistant). A save or a preview against a stream whose source reported a blank or
repeated field name answers 422 with `code` `blank_header` or `duplicate_header`.

### Sync runs

A **run** reads one stream of a source from a checkpoint and applies the records to people.
Start one with **Run now** on the source page (for every enabled stream that has a mapping, or
for one), with `POST /api/v1/workspaces/{ws}/sources/{id}/runs` (`{}` or `{"stream": "name"}`,
admins only, 202), or let the source's cron schedule start them. A stream needs a **mapping**
before it can run: send it as `mapping` in `PUT .../streams/{name}` (the same language as a CSV
import's, see the Mapping reference, with the stream's field names or dotted paths into them as
columns; a trait must be in the registry and a list must exist), or use **Map** on the stream
in the web UI, which previews the mapping on the first records before it is saved.

**Streaming runs.** A stream in the `stream` sync mode (a Kafka topic) is not read to an end. Its
run goes on for up to a day, worked by a chain of `kafka.consume` jobs, and ends `cancelled` when
you cancel it or turn the stream off, and `failed` with a code on an error that retrying cannot
fix or on the reject threshold. Its counters are cumulative; its `attempt` counts only a lost
worker, not each slice. Reconciliation and the sweep do not apply to it, so `reconcile_schedule`
and a `sweep_limit` other than the default are refused for it. See Kafka sources.

**How a run goes.** The `sync.run` job reads the source in batches, maps each record, and
commits a unit of at least 500 records (a source batch is never split) in one transaction: the
profile changes, the retracted records, the rejects, one decision per record, the counters and
the checkpoint. The transaction is fenced twice. It locks the job's lease, so a worker whose
lease was taken over commits nothing, and it locks the run, so a cancelled run is not applied.
Records are kept under the source id and the stream name, with `last_seen_run` set to the run.
A record the source reports as deleted retracts its contribution.

**Deleting what the source dropped.** A **full refresh** ends with a **sweep**: once the whole
stream is read and committed, the stream's live records that the run did not see are removed
(the person's traits are recomputed, and the person follows the workspace's orphan policy), in
bounded transactions of 200 records, each fenced on the lease and on the run like a unit. A
record counts as seen if the run read it and it still exists in the source: applied, unchanged,
and also refused by the mapping or the store, so a broken mapping never cascades into
deletions. A run that fails, is cancelled, or stops on the reject threshold sweeps nothing, and
an incremental run never sweeps, since it cannot tell a dropped record from an unchanged one.
The sweep runs inside the run, before it is `succeeded`; a crash mid-sweep resumes on the next
attempt and removes each record once. Every removal appends a `sync.record` decision with
outcome `deleted` (reason `source`, `signal` or `swept`; it names the record, never a value)
next to the store's `contribution.removed`, and removing a record deletes the events that were
linked to it (`events.source_record_id`) in the same transaction. The event a mapping yields for a
record is linked to that record's ledger row, so a sweep, a delete signal, a connector delete or a
reconciliation removes it with the record; first-party, ingest and contact events carry no link
and stay. List membership added by a sync is not removed with the record (lists add only), so a
removed record's person stays on its lists until someone removes them.

**Deleted records.** Besides the source's own delete flag, a mapping can name a column that
marks a record as deleted: `"delete_when": {"column": "is_deleted", "rule": "truthy"}` in the
mapping of `PUT .../streams/{name}`. `truthy` reads a flag (`true`, `1`, `yes`, `x`, ...; a cell
that is not a flag is not a delete), `present` treats any non-empty cell as a delete (a
`deleted_at` time). The column must be a field of the stream, or a dotted path into one such as `meta.deleted` (422
when its first segment is not a field), the signal is read
before the mapping runs so a soft-deleted row is never refused, and a CSV import refuses a
mapping with one (422 `delete_signal_unsupported`), as it does an `event`. The web UI edits it under
**Map**, with the rest of the mapping; **Set up** keeps the mapping as it is.

**Reconciliation.** An incremental stream can also be reconciled: a run of kind `reconcile`
(`POST .../runs` with `{"stream": "name", "kind": "reconcile"}`, **Reconcile now** on the
page, or the stream's `reconcile_schedule`, a UTC cron set in `PUT .../streams/{name}`) reads the
stream from the beginning for its keys only, applies nothing, never moves the stream's cursor,
and then sweeps like a full refresh. A full-refresh stream is refused (422
`reconcile_needs_incremental`), and a firing skips a stream that has an active run.

**Sweep limit.** A sweep that would remove more than `sweep_limit` of the stream's live records that existed before the
run (default 0.5, set per stream in `PUT .../streams/{name}`, above 0 and at most 1; absent resets
it) removes nothing and fails the run with `sweep_limit`. This stops an upstream outage that
returns an empty list, a partial answer, or a changed primary key from deleting people whose
state a later read cannot bring back. A drop at exactly the limit sweeps. An admin authorizes a
genuinely large deletion by raising the stream's limit (1 turns the guard off) and running again;
the change is audited with the limit.

**Events and lists.** A mapping's list rules add the people of the unit to their lists (source
`sync`, with the usual history row and `list.membership` decision), and its event rule applies
each record's event through the event path: validation against the published schema or the
unplanned-name policy, redaction and reject handling as for an event sent over HTTP. Both are
in the unit's transaction, so profiles, events, members and the checkpoint commit together or
not at all. An event's idempotency key is derived from the source, stream, record key and event
name, so reading the same file again records nothing twice (the event counts as a duplicate),
and a record that changed does not rewrite its past events. The event's time is the run's start
unless the mapping reads one; an old file's events are accepted and flagged late. A refused
event is an event reject tied to the run (shown in its detail), and the record counts once under
its worst outcome: rejected, then applied, then unchanged; its profile change stands. A run
with an event fails before reading with `stream_not_keyed` (no primary key) or
`event_key_unavailable` (the worker has no redaction key), and with `mapping_invalid` when the
mapping no longer fits the descriptor (a field that disappeared).

**Checkpoints.** Each run keeps its own checkpoint, so a retried run resumes where it stopped
without reading or applying a committed unit again; the run shows its **attempt** number. An
incremental stream also keeps its cursor on the stream: the next run starts there, while a full
refresh always starts from the beginning. A run that fails on its rejects does not move the
stream's cursor, so after the mapping is fixed the next run reads those records again. Changing a
stream's sync mode, cursor field or primary key, or a source's settings, clears the stream's
cursor, since it belonged to the old ones. Such a change is refused with a 409 (`sync_run_active`)
while a run of the stream (for a source's settings, of any of its streams) is queued or running,
since that run would write the old cursor back: wait for it or cancel it first. Other changes,
such as the staleness threshold, enabled, the name, the schedule or the mapping, stay allowed.

**Cancel** with `POST .../runs/{run_id}/cancel` (admins only; 409 when the run has ended). The
run stops at its next unit: the unit in flight is not applied, and the checkpoint stays at the
last committed one.

**Rejects.** A record the mapping or the store refuses is kept in `ingest_rejects` with the run
id, reason codes, and a sample in which a value is shown only when the stream's mapping sends
that field to a trait the registry marks non-PII; every other value is null and listed as
masked. A run fails with `reject_threshold` when, after at least 100 records, more than 5% were
refused. The units applied before stay applied. Sync rejects age out after 30 days like the
others and hold no person id, so an erasure does not need to reach them.

**Settings are checked again.** Before reading, a run checks the stream's settings against the
descriptor discovery last reported, and fails with `stream_settings_invalid` when they no longer
fit (a cursor field that disappeared, say). A stream without a mapping fails with
`stream_not_mapped`; other codes are `source_error` and `job_failed` (the last attempt failed),
`mapping_invalid`, `list_missing`, `stream_not_keyed`, `event_key_unavailable`, `file_changed` and the other file codes (see the file source), `source_missing`, `stream_missing` and `source_kind_unavailable`.

**Schedules.** A source's cron schedule (UTC) is mirrored to a `schedules` row, kept in step in
the same transaction as the source: clearing the schedule, disabling the source or deleting it
stops the firing. When the scheduler leader fires it, the job queues a run for each enabled,
mapped stream that has no active run; a stream with an active run is skipped. One active run
per stream is enforced by the database. The leader also fails (`job_failed`) a run whose job was
buried by the job system or deleted, at most a minute later, so a lost job cannot keep a stream
blocked.

**Reading.** `GET .../runs?stream=&limit=&cursor=` lists runs newest first, and `GET
.../runs/{run_id}` returns one with its counters, state, attempt, error code, trace id and its
first 100 rejects. Any member or `read` key can read; starting and cancelling are audited as
`sync_run.started` and `sync_run.cancelled` with run ids only. Everything a run changes is under
one trace (`GET .../traces/{trace_id}`): each applied, refused or deleted record has a decision
with the `sync_run` cause and a durable id (the source record, the person, or the reject), never
a value. Metrics: `zirv_flow_sync_runs_total{outcome}`, `zirv_flow_sync_records_total{outcome}`
`zirv_flow_sync_events_total{outcome}` (`accepted`, `duplicate`, `rejected`) and `zirv_flow_sync_run_duration_seconds`, with no source, stream or workspace label.

### The HTTP manifest source

The `http` source type reads any HTTP API that a **manifest** describes: its address, how to
authenticate, how it pages, which field is the incremental cursor, which record marks a deletion and
(optionally) how its fields map to people. The manifest is JSON data stored as the source's
`config.manifest`. It is interpreted, never executed: there is no expression language and no
scripting. A manifest holds no secret; the credential fields below hold those.

Add one in the setup wizard like any source. The **Manifest** field is an editor that checks the text
as you type and shows every problem as a sentence with its place in the manifest (click it to jump
to the line), offers a starter manifest for each kind of authentication, and, once the source is
saved, a **Test with sample** button that reads a few raw records with the stored credential. An assistant panel drafts a manifest from pasted API documentation (see Mapping and manifest assistant).

**A manifest, annotated.**

```jsonc
{
  "version": "v1",                                  // the only version
  "base_url": "https://api.example.com/v1",         // https; no user info, query or fragment
  "auth": { "type": "oauth2_authorization_code",    // see Authentication
            "authorize_url": "https://auth.example.com/authorize",
            "token_url": "https://auth.example.com/token",
            "scopes": ["contacts.read"] },
  "rate_limit": { "requests": 100, "per_seconds": 60 },   // optional; 1-1000 per 1-3600 s
  "headers": { "Accept": "application/json" },      // optional; static; at most 10
  "streams": [{                                     // 1 to 20
    "name": "contacts",                             // unique, 1-256 characters
    "path": "/contacts",                            // starts with /, no //, .., ? or #
    "query": { "limit": 100 },                      // optional static parameters, at most 20
    "records_path": "/data",                        // JSON pointer to the array; "" for a top-level array
    "pagination": { "type": "cursor", "param": "cursor", "next": "/next_cursor" },
    "fields": [ { "name": "id", "type": "string" },        // 1 to 200
                { "name": "email", "type": "string" },
                { "name": "updated_at", "type": "timestamp" } ],
    "primary_key": ["id"],                          // fields of the stream; no positional fallback
    "incremental": { "cursor_field": "updated_at",  // optional
                     "request_param": "since", "cursor_type": "timestamp" },
    "delete_signal": { "path": "status", "equals": "deleted" },   // optional
    "mapping": { "key_column": "id", "columns": [ ... ] }         // optional; see Mapping reference
  }]
}
```

**Authentication.** `auth.type` is one of `none`; `api_key` (`in`: `header` or `query`, and `name`);
`basic`; `bearer`; `oauth2_client_credentials` (`token_url`, `scopes`, `client_auth`: `body` or
`basic`); and `oauth2_authorization_code` (`authorize_url`, `token_url`, `scopes`, `client_auth`).
The credential fields are fixed by the type: `api_key` reads `api_key`; `basic` reads `username` and
`password`; `bearer` reads `token`; both OAuth2 types read `client_id` and `client_secret` (optional
for authorization code, since a PKCE client may have none). A missing one fails the test or the run
naming `credential.<name>`. Auth headers cannot be set in `headers`: `Authorization`,
`Proxy-Authorization`, `Cookie`, `Host`, `Content-Length`, `Transfer-Encoding`, `Connection`,
`Range` and `If-*` are refused. An `api_key` header name follows the same rule, except that it may
be `Authorization`. Changing the `auth` section of a manifest disconnects an account
connected earlier, because a refresh token belongs to one client and scope set.

**Credential schema.** `auth.credential` is optional and declares the shape of the credential the API
needs, so a manifest source gets the same form, storage and rotation as a built-in one. It holds
either `{"kind": "<built-in kind>"}` (`api_key`, `basic`, `smtp`, `oauth2_client`,
`signing_secret`, `bearer_token` or `tls_client`) or `{"fields": [{"key", "label", "type", "required", "secret", "options"}]}` with
the field rules of Workspace credentials (1 to 32 fields, unique lowercase keys, `options` only on an
`enum`, and only a `string` can be `secret`), never both. The schema must contain every credential
field the auth type reads (above; `client_secret` of the authorization-code grant is optional), or
the manifest is refused with `credential_field_missing`. A field the auth type reads that holds a secret (`api_key`, `password`, `token`, `client_secret`) must be `secret: true`, or the manifest is refused with `credential_field_not_secret`, because a non-secret field is stored in plaintext. A manifest holds no value: the validate
response returns the schema as `credential_schema`, and the credential is created from it as kind
`custom`.

**Pagination.** `none`; `cursor` (`param`, `next`: a JSON pointer in the response); `offset`
(`param`, `limit_param`, `limit` 1-1000); `page` (`param`, `first`, and optionally `size_param` with
`size`); `link_header`. They are the runtime's drivers (see Connector runtime).

**Incremental reads and the checkpoint.** With `incremental`, a read after the first sends the stored
value as `request_param`. The checkpoint is `{"since": <text|null>}` and moves only at the end of a
stream: every batch but the last carries the checkpoint the run started with, and the last carries
the highest `cursor_field` value seen (compared as RFC 3339 instants or as integers per
`cursor_type`; a record with a missing or unparsable value is still read and does not move it).
An API may return records in any order, so a per-page high-water mark could skip records; the cost
is that a run that loses its lease re-reads from where it started, which costs requests and not
correctness, because applying a record twice changes nothing.

**Deletes and a suggested mapping.** A record whose value at `delete_signal.path` (a dotted path)
equals the literal is read as deleted, and the run removes it like any deleted record. A stream's
`mapping` is only a seed: when discovery first stores a stream, the seed is written as its mapping if
it passes the same checks as a saved one (traits that do not exist drop it, and the stream is left
unmapped); it never replaces a mapping and is not used on a later discovery. A manifest cannot add
people to a list.

**Validation.** `POST /api/v1/workspaces/{ws}/source-manifests/validate` with `{"manifest": ...}`
(an admin; a body up to 65 KiB) answers 200 with `{valid, issues: [{code, path}], streams,
credential_schema}` and
writes nothing; saving a source with the same manifest answers 422 with the same codes under
`config.manifest<path>`. An issue holds a code and a JSON pointer, never a value. The codes:
`required`, `invalid_type`, `invalid_value`, `out_of_range`, `invalid_length`, `invalid_url`,
`unknown_field`, `expression_not_supported` (a script-like key such as `script` or `eval`, or a string
holding `{{`, `${`, `{%` or `<%`), `unsupported_version`, `manifest_too_large` (64 KiB),
`too_many_streams`, `too_many_fields`, `too_deep` (12 levels), `too_many_headers`,
`forbidden_header`, `invalid_header`, `too_many_query`, `path_invalid`, `invalid_pointer`,
`duplicate_stream`, `duplicate_field`, `invalid_field_name`, `primary_key_empty`,
`primary_key_unknown_field`, `cursor_field_unknown`, `invalid_mapping`, `list_target_not_allowed`,
`invalid_credential_schema`, `credential_field_missing`, `credential_field_not_secret`,
and the mapping codes of the Mapping reference.

**Sample.** `POST .../sources/{id}/manifest/sample` with `{"stream": "...", "limit": 1-20,
"manifest": ...}` (an admin; the draft is optional and checked as a save is) reads from the start,
with the stored credential and account tokens, and returns the raw records. A draft must keep
the saved base URL and `auth` section (otherwise 409 `manifest_draft_auth_changed`, before anything
is opened): save those changes first. It writes nothing and is not audited.

**Connect account.** For an `oauth2_authorization_code` manifest, an admin opens the source and
chooses **Connect account**: the page asks the server for the provider's consent URL
(`POST .../sources/{id}/oauth/start`, a signed-in person, never an API key) and navigates to it. The
server stores a single-use state (hashed, ten minutes) with the PKCE verifier. Register
`<server.public_url>/api/v1/oauth/callback` as the redirect URI at the provider; without
`server.public_url` the start answers 409 `public_url_not_configured`. The callback exchanges the
code through the guarded client and stores the access and refresh token in one sealed secret, then
returns to the source page with a success or failure banner. On any failure (an unknown or reused
state, another person's session, a manifest whose authentication changed since the consent started, a provider error, a refused exchange, a response with no refresh
token) nothing is stored. Refresh tokens that rotate are stored by whoever refreshes (a sync run or a
test), serialized across processes so a single-use token is never used twice. Audit entries
(`source.oauth_started`, `source.oauth_connected`, `source.oauth_failed` with a reason,
`source.oauth_disconnected`) hold ids and reasons only, and the counter
`zirv_flow_source_oauth_total{outcome}` has no other label.

### Database sources

The `postgres` and `mysql` source types read the tables and queries of one schema (PostgreSQL) or
one database (MySQL). Supported versions: PostgreSQL 16 and 18 and MySQL 8.4, the ones CI runs
against; MariaDB is not supported. Add one in the setup wizard like any source: the form asks for
the host, the port (5432 or 3306 by default), the database, for PostgreSQL the schema (`public` by
default), TLS, an optional CA certificate, a statement timeout and the queries, and the credential
is a user and a password.

**Create a read-only role.** The role's grants are the real guarantee that nothing is written.
Every statement also runs in a read-only transaction, which the server refuses to write in, but do
not depend on that alone. Grant `SELECT` on the tables to read and nothing else:

```sql
-- PostgreSQL
CREATE ROLE zirv_flow_reader LOGIN PASSWORD 'choose-a-long-password';
ALTER ROLE zirv_flow_reader SET default_transaction_read_only = on;
GRANT CONNECT ON DATABASE shop TO zirv_flow_reader;
GRANT USAGE ON SCHEMA public TO zirv_flow_reader;
GRANT SELECT ON public.customers, public.orders TO zirv_flow_reader;

-- MySQL
CREATE USER 'zirv_flow_reader'@'%' IDENTIFIED BY 'choose-a-long-password' REQUIRE SSL;
GRANT SELECT ON shop.customers TO 'zirv_flow_reader'@'%';
GRANT SELECT ON shop.orders TO 'zirv_flow_reader'@'%';
```

Discovery lists the base tables the role can see in the schema (not views and not other schemas),
so the grants decide which streams exist. More than 1,000 tables is refused: grant the role fewer.

**Streams.** A table is a stream named after the table. A **query** is a stream named
`query:<name>`: enter queries in the **Queries** setting of the source (at most 20; a name of 1 to
64 characters of `a-z`, `0-9` and `_`). A query is one `SELECT` or `WITH` statement of at most
20,000 characters, with no semicolon at the end, whose column names are unique. It must return the
primary key columns and, for an incremental stream, the cursor column. No `ORDER BY` is needed: the
source orders and pages it. Discovery prepares each query (the server parses and plans it and does
not run it) to learn its columns. After discovery, set each stream's primary key and, for an
incremental one, its cursor field in the stream settings; a table's primary key is filled in for
you, and `updated_at` is offered as the cursor when the table has a timestamp column of that name.

**Egress.** A database on a private address needs its range in `egress.allow` (see Egress), for
example `allow = ["10.0.5.0/24"]`. The host is resolved and every address checked at each
connection (a test, a discovery and each run); one refused address refuses the host. The
connection goes to the checked address, and the host name you entered is the name the certificate
must match.

**TLS.** The default, `verify`, requires TLS and checks the certificate against the **CA
certificate** setting (a PEM, which is then the only CA trusted) or, when that is empty, the public
roots compiled into the server. The operating system's certificate store is not used. There is no
plaintext fallback. `off` is accepted only toward non-public addresses (private, loopback,
link-local) that the egress policy allows; a public address always needs TLS, even when it is
allowlisted in `egress.allow`.

**How a table is read.** A stream is read in pages of 500 rows, each in its own short read-only
transaction with the statement timeout set (`statement_timeout_seconds`, 60 by default, 1 to 600).
Rows are ordered by the cursor and then the primary key (incremental) or by the primary key (full
refresh), and the next page starts strictly after the last row of the previous one, so rows that
share a cursor value across a page boundary are neither skipped nor read twice. The checkpoint is
the key of the last row. Consequences to plan for:

- A stream must have a **primary key** (one or more columns): without one it cannot be resumed,
  and the run fails. A table without a primary key can be listed but not read.
- The cursor and primary key columns must be of a type that can be paged on. PostgreSQL: `int2`,
  `int4`, `int8`, `numeric`, `text`, `varchar`, `bpchar`, `uuid`, `date`, `timestamp`,
  `timestamptz`. MySQL: integer types (signed and unsigned), `decimal`, `char`, `varchar`, `text`,
  `date`, `datetime`, `timestamp`. Other types (floats, booleans, binary, JSON) are refused.
- Choose a cursor that only moves forward and is assigned when the row is committed, such as an
  `updated_at` maintained by a trigger, and index it together with the primary key. A transaction
  that commits a row with an older cursor value after the stream has passed it is picked up by the
  next full refresh, not before: that is inherent to reading by cursor.
- **Rows whose cursor is NULL are not read by an incremental stream**; a full refresh reads them.
- A NULL primary key value fails the run (it cannot be a record key).
- Sweeping what a full refresh no longer lists, and soft-delete signals, come later: these sources
  never report a record as deleted.

**Values.** Every selected column is read as text and arrives as a JSON string, or `null` for SQL
`NULL`; the mapping's cast transforms turn it into numbers, booleans and timestamps (the text
forms PostgreSQL and MySQL give are read as they are, see Timestamps under the mapping). Timestamps are
read in UTC. JSON columns arrive as JSON text. Binary columns (`bytea`, `BLOB`, `BINARY`) are not
text and arrive as `null`.

**Errors and privacy.** An error from the database can quote the value that broke a constraint, so
no server or driver message is ever shown, logged or stored: a failure is one of a few fixed
messages (the credential was refused; the database could not be reached or did not answer in time;
the connection could not be secured; the query failed; a write was refused), and the log line
holds the source type and the SQLSTATE or MySQL error number only. Metrics (no source, stream or
workspace label): `zirv_flow_source_db_batches_total{source_type}` and
`zirv_flow_source_db_errors_total{source_type,reason}` with `reason` one of `auth`, `unreachable`,
`egress`, `timeout`, `tls`, `read_only`, `config` or `query`.

### Kafka sources

The `kafka` source type reads JSON messages from Apache Kafka topics (or a Kafka-compatible
broker such as Redpanda). A **stream is a topic**, and its sync mode is **streaming**: instead of
reading to an end, a consumer follows the topic for as long as the stream is on. Add one in the
setup wizard like any source.

| Setting | Meaning |
| --- | --- |
| `brokers` | One to ten `host:port` pairs, comma-separated, without a scheme. IPv6 addresses are not supported. |
| `topics` | One to twenty topic names, comma-separated. Each is a stream. |
| `security` | `tls` (default) or `plaintext`. |
| `sasl` | `none` (default), `plain`, `scram_sha_256` or `scram_sha_512`. |
| `tls_ca` | A PEM bundle to trust instead of the system roots, for a private authority. |
| `max_lag` | How many messages, over all partitions, the stream may be behind and still count as fresh. Default 10,000. |

The credential fields are `username` and `password` (needed with `sasl`) and `tls_client_cert`
and `tls_client_key` (a client certificate and its key, both or neither). All four are secret:
sealed like any credential and never returned. OAUTHBEARER is not supported.

**Messages.** The **message key** is the record key, as UTF-8 text of 1 to 512 characters, and the
stream's primary key is the virtual field `_key`. The value must be a JSON object of at most 1 MiB;
its top-level fields are the stream's fields, plus two virtual ones: `_key` and `_timestamp` (the
message time, RFC 3339). Those two names win over fields of the same name in the value. Map them
like any field. Discovery reads up to 50 messages from the start of each topic to list the fields;
an empty topic reports only the two virtual ones, so discover again once producers write.

**Deleting.** A message with a **null value** (a tombstone) removes the record with that key and
its linked events, through the same path as a source that reports a deletion. A tombstone for a
key the stream never saw does nothing. A delete signal in the mapping works as for any stream.
There is no sweep and no reconciliation: they cannot tell a dropped record from a quiet one.

**Refused messages.** A message that is not a record is a **reject** of the run, with its place
(`partition 3 offset 1042`) and a code, and never its content: `kafka_key_missing`,
`kafka_key_not_text`, `kafka_value_not_json` (an empty value counts), `kafka_value_not_object`,
`kafka_message_too_large`. The partition goes on. A run over the reject threshold stops like any.

**Delivery.** Offsets are kept in the database, one row per partition, never in Kafka: the consumer
joins no consumer group and commits nothing to the broker. They are written in the same
transaction that applies the messages they cover, fenced on the job's lease and on the run, so
**each committed batch applies once and an uncommitted batch is read again** after a crash, a lost
lease or a restart. A partition with no stored offset starts at its earliest offset, and new
partitions are found at the start of every slice. Order is kept within a partition; a key that
appears in several partitions has no defined order. Two messages with one key are never merged:
each is its own unit of work, so each yields its own event, and a message replayed after a crash
yields the same event again, which is dropped as a duplicate. Offsets that hold no record
(transaction markers, compacted-away batches) are skipped, so compacted topics and transactional
producers do not stall a partition. Messages of aborted transactions are **delivered** (the client
cannot filter them), so a topic written by aborting transactional producers is not supported. If
retention removed the stored offset, the consumer jumps to the earliest one and counts it in
`zirv_flow_kafka_offset_resets_total`. If the stored offset is past the end (the topic was
deleted and created again) the run fails with `kafka_offset_out_of_range`; stored offsets cannot
be reset from the app yet.

**Starting and stopping.** Turning on a streaming stream that has a mapping starts its consumer.
**Run now** and the source's cron schedule start one when none is running, so the schedule is also
the restart path after a failure; `*/5 * * * *` is a sensible choice. Turning the stream or the
source off cancels the consumer. The run stays `running` for up to a day and is worked by a chain
of `kafka.consume` jobs of 60 seconds each; each slice enqueues the next, in the transaction that
completes it. A day after it started, the run ends `succeeded` and a new run takes over, so its
trace, counters and reject ratio stay small. A consumer holds one worker slot during its slice: with
more streaming streams than `worker.concurrency - 1`, other jobs starve. Watch
`zirv_flow_worker_busy`.

**Lag and freshness.** After each slice (and every 10 seconds within one) the consumer stores the
log's end for each partition. **Lag** is the end minus the next offset to read. `GET
.../sources/{id}/streams` carries `freshness.lag` with the `total` and each partition's `lag`
(counts, which members may read), and the streams table shows `Lag 1,204` next to the badge. Lag
within `max_lag` counts as a success and lag above it as a failure, so a stream that falls behind
turns **failing**, and a consumer that stops turns **stale** by the usual time rule, which needs a
staleness threshold on the stream or a schedule on the source. The lag of a transactional topic can
include its trailing commit marker, which is never delivered, so `max_lag` should not be 0 for one. The gauge
`zirv_flow_stream_lag_messages{workspace,stream}` follows it, and `zirv_flow_kafka_fetches_total{outcome}`
counts fetches.

**Network.** The client has no resolver hook, so every connection goes through a loopback SOCKS5
relay that resolves the host, checks every address against the egress policy (see Egress), dials
the checked address and leaves TLS to verify the host name. This covers the bootstrap hosts and
every broker the cluster names, on every reconnect. A broker whose address is refused fails the
read with `source_error`, counted in `zirv_flow_egress_denied_total`. While a read runs, another
process on the same machine could use the relay to reach any destination the policy allows. A broker
that is private needs an `egress.allow` entry like any private destination. Errors are static
codes (`kafka_connect_failed`, `kafka_tls_failed`, `kafka_auth_failed`, `kafka_topic_missing`,
`kafka_timeout`) and never a broker's own text. The client logs host names, never messages. The
SASL password lives in plain memory inside the client while it runs and cannot be wiped there.

**Limits.** JSON only (no Avro, Protobuf or schema registry), no consumer groups, no producing,
no topic patterns. Gzip, Snappy and LZ4 compression only: a zstd-compressed batch fails the read
until the Kafka client moves to a fixed zstd (#338). At most 1 MiB is fetched per partition per request. Messages are checked after
decompression only for the 1 MiB record limit.

### Source freshness

Marketers can see whether the data behind a segment is current. Every enabled stream of an
enabled source is **fresh**, **stale** or **failing**, and the state is worked out from facts on
each read, never stored, so a changed threshold or schedule takes effect at once.

- **Failing:** the last run of the stream failed (any error code, including `reject_threshold`
  and `job_failed`). This holds however recent the last success was. A cancelled run is not a
  verdict on the data and changes nothing. The next success clears it.
- **Stale:** the time since the last success is past the stream's threshold. Exactly at the
  threshold is still fresh.
- **Fresh:** anything else. A stream that has never run is fresh until the threshold has passed
  since it was enabled, then stale. Enabling a stream again restarts that grace period, and
  streams that were already enabled when the feature arrived start theirs at the upgrade.

**The threshold.** A stream's own `staleness_threshold_seconds` wins. Without one it is twice
the source schedule's interval (an hourly schedule is stale after 2 hours); the interval is the
longest gap among the next 8 slots, so a weekdays-only schedule allows for the weekend. A stream
with neither a threshold nor a schedule is **never stale**, though it can still be failing.
Admins set or clear the threshold with the stream's `PUT .../streams/{name}` or the stream
dialog; it is audited as `stream.configured` with the number (`staleness_threshold_seconds`).

**Where it shows.** `GET .../sources/{id}/streams` carries `freshness` on each stream: `state`,
`last_success_at`, `threshold_seconds`, `threshold_is_default` and `high_water_mark`. It is
`null` for a stream that is not read, or whose source is disabled. `GET .../sources` and `GET
.../sources/{id}` carry `freshness` as the worst state of the source's enabled streams (`null`
when there is none). The high-water mark is the stream's checkpoint, which can hold a value
taken from a record, so only workspace admins get it; everyone else gets `null`. The Sources
list shows a badge per source, and the streams table and the stream dialog show the badge, the
time since the last success and the threshold.

**Metrics.** The scheduler leader refreshes these at most every 30 seconds, with ids as the only
labels: `zirv_flow_stream_freshness_seconds{workspace,stream}` (age of the data),
`zirv_flow_stream_state{workspace,stream}` (0 fresh, 1 stale, 2 failing) and
`zirv_flow_streams{workspace,state}` (how many enabled streams are in each state). A stream that
is disabled or deleted is set to 0, and an instance that stops leading zeroes what it published.

**Other source types.** A pull run reports when it ends. A webhook source reports each accepted
payload through the same call, `record_stream_outcome` (see Inbound webhooks), and a stream consumer
reports through it too: a success at a time, or a failure, written to the same columns. The Kafka
consumer reports within its lag bound as a success and above it as a failure (see Kafka sources). A
webhook has no schedule, so its threshold is the one you set ("expect a payload at least every");
without one it is never stale.

**Not done:** stale or failing inputs do not pause sends. That is a later decision (OQ-14, M5);
for now freshness is shown and exported, nothing acts on it.

## Inbound webhooks

A webhook lets an external system (a form tool, a shop back end, an internal service) push one JSON
document at a time to Zirv Flow. It is a **push source**: a source of type `webhook` with one stream,
`payload`, so it has the same mapping editor, preview and freshness as any other stream. Its
payloads are not read by a run; each one is verified, queued in the durable inbox and mapped to a
person, an event and list memberships by `ingest.drain`.

**Set up.** Under Sources, add a source and choose Webhook (or `POST .../webhooks`, admins only).
You give it a name, a **dedupe field** (a dotted path into the payload whose value identifies it,
such as `id` or `data.order.id`) and, optionally, how often you expect a payload. The response
carries the **URL** (`/hooks/{id}` on your server's origin) and the **shared secret**
(`zfwh_` and 64 hex digits). The secret is shown once, in that response, and never again: it is
stored sealed (purpose `webhook_secret`), and no read, update, audit entry or log line holds it.
Rotate it with `POST .../webhooks/{id}/rotate-secret`; the old secret stops working at once. The
id in the URL only locates the hook, it is not a secret, and the signature authorizes.

Then map it: open the stream (Set up, then Map), paste a sample payload, and the mapping editor
shows what the mapping makes of it, with the same function the drain calls and without writing
anything (`POST .../streams/payload/preview` with `sample`: 1 to 20 objects, 64 KiB in all).
Columns name payload fields, a nested one with dots (`order.total`, `items.0.sku`); a field a
payload lacks is an empty cell. A webhook stream has no key column and no delete signal, and an
event needs no primary key: the event id comes from the dedupe field. Turn the stream on. Until it
is on and mapped the URL answers `404`.

**The signature.** Each request carries an HMAC-SHA256 of its body, keyed with the secret's UTF-8
bytes, as the prefix followed by 64 hex digits (either case). The defaults, all per hook:

| Setting | Default | Meaning |
|---|---|---|
| `signature_header` | `X-Zirv-Signature` | the header holding the signature |
| `signature_prefix` | `sha256=` | the text before the digest; may be empty |
| `timestamp_header` | `X-Zirv-Timestamp` | unix seconds as a decimal number; `null` selects the body-only scheme |
| `tolerance_seconds` | 300 | how far the timestamp may be from the server's clock, either way (1 to 86400) |

The **timestamped scheme** signs `"{timestamp}.{body}"` (the timestamp text exactly as sent, a dot,
the raw body bytes) and refuses a timestamp outside the tolerance. The **body-only scheme** signs the
raw body. It exists because many senders cannot sign a timestamp; its only replay defense is the
dedupe field. Other algorithms, base64 signatures and handshake protocols are not supported.

```sh
# The timestamped scheme (the default).
BODY='{"id":"order-1","email":"ada@example.com"}'
TS=$(date +%s)
SIG=$(printf '%s' "$TS.$BODY" | openssl dgst -sha256 -hmac 'zfwh_example' | sed 's/^.* //')
curl -X POST 'https://flow.example.com/hooks/3f2a9c1e-0b7d-4e55-9c1a-7d4e2b6a8f10' \
  -H 'Content-Type: application/json' \
  -H "X-Zirv-Timestamp: $TS" \
  -H "X-Zirv-Signature: sha256=$SIG" \
  -d "$BODY"

# The body-only scheme.
SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac 'zfwh_example' | sed 's/^.* //')
curl -X POST 'https://flow.example.com/hooks/3f2a9c1e-0b7d-4e55-9c1a-7d4e2b6a8f10' \
  -H 'Content-Type: application/json' -H "X-Zirv-Signature: sha256=$SIG" -d "$BODY"
```

With the secret `zfwh_example`, the timestamp `1700000000` and the body above, the signature is
`c8447361d430f2281aef6579aec897c4930f3d15af1920d3da1f83ff8e4f4d56`; a unit test signs that
example with the same recipe.

**What happens to a request.** In this order, and nothing is written before the signature checks
out: a declared body over `ingest.max_webhook_bytes` is `413`; an unknown id, or a hook that is not
receiving (source or stream off, stream not mapped), is `404`; a hook over `ingest.rate_per_hook`
is `429` with `Retry-After`; the body is read with the same cap (`413`); the signature and, for the
timestamped scheme, the timestamp are checked, the digest compared in constant time (`401`, code
`invalid_signature`, one answer for every cause but a stale timestamp, which says so); the body
must be one JSON object (`400`, `invalid_payload`; arrays and several records per request are not
supported); the dedupe field must hold a string or number of 1 to 128 characters (`422`,
`dedupe_missing`); then the payload is appended to the inbox and the answer is `202
{"accepted":1}`: durable, not yet applied. A database failure is `503` and stores nothing, so
retry; so is a server started without a master key. The rate limit is a token bucket per hook and
per `serve` process (`ingest.rate_per_hook`, `ingest.burst_per_hook`), charged only for ids that
belong to a hook, so a flood of made-up ids costs one indexed lookup each and no memory. Anyone
who holds the URL can use up a hook's budget with unsigned requests; limit senders by address at
your edge proxy.

**Dedupe and replay.** The dedupe value becomes the event's message id, namespaced by the hook
(`wh:{hook}:{value}`), so the same payload sent again, inside or outside the tolerance window, with
a new signature or the same, leaves one event and the same trait values. The profile record is
keyed by the person's strongest identifier and observed at the signed timestamp (at the arrival
time for the body-only scheme), so a replayed older payload never overwrites a trait that a newer
one set. With the body-only scheme a captured request can be replayed, but only to the same effect.

**Mapping failures.** A payload the mapping refuses (no identifier, a value that does not cast) is
an ingest reject for the stream, with field names, codes and a masked sample and never a value
outside non-PII traits, and the rest of the batch applies. A payload queued before the stream lost
its mapping is rejected as `mapping_unavailable`. Mapping runs in the drain, so an edit to the
mapping applies to payloads still queued. A webhook reject is not replayable.

**Freshness.** An accepted payload reports through `record_stream_outcome`, at most once a minute
per hook per process, so the stream shows **fresh**, **stale** (no payload within the stream's
threshold; set it with "expect a payload at least every" or `staleness_threshold_seconds`) or
**failing** like any other (see Source freshness). A rejected request writes nothing to the stream.
A webhook source is created, changed and deleted only through `/webhooks` (the generic source
routes answer `409`, code `managed_by_webhooks`) and never gets a run (`409`, `push_source`).

**Metrics.** `zirv_flow_webhook_requests_total{outcome}` with `accepted`, `not_found`,
`rate_limited`, `too_large`, `signature_missing`, `signature_malformed`, `signature_stale`,
`signature_mismatch`, `invalid_payload`, `dedupe_missing` or `unavailable`; the drain's
`zirv_flow_ingest_applied_total` carries `kind="webhook"` with the outcomes `applied`,
`unchanged`, `rejected`, `apply_failed`, `accepted` and `duplicate`. No hook id, workspace or
value is a label, and nothing logs a body, a header value, a signature or a secret.

## Connector runtime

Every HTTP source makes its calls through the connector runtime in `zirv-flow-connectors`
(`SourceContext::send`), which adds the pieces below on top of the guarded client, so no source
type implements them itself. A test connection, a discovery and a sync run of one source all use it.

**Auth.** API key (in a header or a query parameter), basic, bearer, and OAuth2 client
credentials and the OAuth2 authorization-code grant with PKCE and refresh (the client proves itself in the form body or with basic auth). The plaintext
credential is borrowed for the duration of a call. An OAuth2 token is fetched when there is none or
when less than 60 seconds of it remain; a `401` on a token that was already cached refreshes it
once and retries the call once, and a second `401` is a refused credential. A refreshed client-credentials token is
handed back to the caller (`take_refreshed_token`); the runtime never writes to the database. For
the authorization-code grant the refresh token is the source's long-lived credential: the
code is exchanged once at consent, and a refresh that returns a new refresh token (rotation)
replaces the old one. A caller that persists tokens gives the context a token store, and the
refresh then runs through it so that processes refreshing the same source take turns: the api and
the sync worker lock the source's token secret, reload it, call the provider only if nobody else
has, and save the result in the same transaction. Both tokens live in one sealed secret per source,
linked from the source and never returned. Auth is attached only to requests for
the source's base origin (scheme, host and port); a request for any other origin, such as a
pagination link that points elsewhere, is refused before it is sent. Redirects are followed only
within the same origin: a redirect to another origin, including `http` to `https` on the same host,
ends the call with the same error, so a header or query key never leaves the origin. Give such a
source its final base URL.

**Retries.** `GET` and `HEAD` (and a request the source marks idempotent) are retried on `429`,
`500`, `502`, `503`, `504` and on connection failures and timeouts, up to 5 attempts, with an
exponential backoff from 1 s to 60 s with jitter. On `429` and `503`, `Retry-After` (seconds or an
HTTP date) is used instead; a `Retry-After` over 300 s ends the call at once without waiting. Other
`4xx` codes and a `POST` are not retried. A call that runs out of attempts fails with the last
status.

**Rate limit.** A source type may declare a request rate; the runtime holds a token bucket per
source and waits for it before each request. The limit is **per process**: it is shared by the sync
runs, test connections and discoveries of that source in one process, but several workers each have
their own bucket and can exceed the limit together. Honouring `Retry-After` reduces that overshoot
and does not prevent it. A source type may declare the rate once for all its sources, and an `http`
source declares it in its manifest; editing it takes effect on the next call. Enforcing the limit across workers is decided with open question OQ-9 (M6).

**Pagination.** Four drivers read an API to its end as a stream of pages: cursor (the next cursor
at a JSON pointer, a missing, null or empty cursor ends it), offset (ends on a page shorter than
the limit), page number (ends on an empty page, or a short one when a page size is set) and the
`Link: rel="next"` header. A cursor or link that repeats ends with an error rather than looping,
and so does a response whose records are not an array, or a cursor or link longer than 8 KiB. An
offset or page stream whose API ignores the parameter and sends the same full-size page three times
in a row ends with the same error (two identical pages can be genuine, so they are read); with no
page size set, only the 100,000-page cap ends such a stream.

**Metrics and logs.** `zirv_flow_source_requests_total{source_type,outcome}`,
`zirv_flow_source_retries_total{source_type,reason}` and
`zirv_flow_source_rate_limit_wait_seconds_total{source_type}`. The labels are the source type and
fixed values only: never a URL, a host or a source id. No error or log line holds a secret, a URL,
a header or a response body.

## Tracking and unsubscribe links

Marketing email is sent with signed links on `server.public_url` (the send fails `render_error`
without it, and the secrets master key must be set). A transactional message is not tracked.

- Links are rewritten to `/t/c/{token}` and redirect only to the link recorded for that message;
  `/t/o/{token}` is a 1x1 GIF added at the end of the HTML.
- `{{ system.unsubscribe_url }}` is `/u/{token}`: a GET shows a confirmation and changes nothing, a
  POST unsubscribes. The `List-Unsubscribe` header uses it with `List-Unsubscribe-Post:
  List-Unsubscribe=One-Click` (RFC 8058), so the URL must be `https`.
  `{{ system.preferences_url }}` is `/u/{token}/preferences`, where the person picks topics and
  dimension values; a change to one vertical blocks only that vertical.
- Tokens hold the message id and a link index and a MAC, nothing else. The routes have no sign-in and
  no CSRF token: the token authorises the one message. A bad token is a small 404 page.
- Events: `email.opened` (with `machine` and `machine_reason`), `email.clicked` (with `link_index`) and
  `email.unsubscribed`. An open is a machine open when its user agent contains `proxy`, `prefetch`,
  `preview`, `scanner`, `crawler` or `bot`, or a fragment you list in `tracking.prefetch_user_agents`,
  or when it arrives within 10 seconds of delivery. `machine = false` means not detected as a
  machine, not a verified person. Neither the IP address nor the user agent is stored.

```toml
[tracking]
prefetch_user_agents = ["my-gateway-fetcher"]
```

or `ZIRV_FLOW_TRACKING__PREFETCH_USER_AGENTS='["my-gateway-fetcher"]'`.

### Automatic UTM parameters

A workspace admin can turn on UTM parameters for email links on the Sending screen (the "UTM parameters"
section), or with `PATCH /api/v1/workspaces/{ws}` and a `utm` object. It is off by default. When it is on,
every `http` or `https` link to a listed domain (or a subdomain of one) gets `utm_source`, `utm_medium`,
`utm_campaign` and `utm_content`, in that order, from patterns you set (defaults `{workspace}`, `email`,
`{flow}`, `{template}`). The patterns may only use `{workspace}`, `{flow}`, `{flow_version}`, `{step}`,
`{template}`, `{template_version}` and `{category}`; values are lower-cased with hyphens and URL-encoded, and
nothing about the person is used. Enabling needs at least one domain. Unsubscribe and preference links,
`mailto:`, `tel:` and anchors are never tagged, a `utm_*` parameter already on a link is kept, and
`data-zf-utm="off"` on a link skips it (the attribute is removed from every message, tagged or not, and the same URL in the plain-text part is left alone too). A template can opt out in its editor. The
tagging happens before click tracking, so the recorded link and the redirect carry the parameters, and the
preview and test sends show them too (without a flow, `{flow}` is empty and that parameter is left out).

## Template editor and delivery counts

The web app has a Templates page. A template is written as HTML, MJML or from blocks (chosen when it is made) in a split view:
the editor on the left, the preview on the right. The block editor has a palette, a canvas you can
reorder by drag or by keyboard, a settings panel and inline validation, and a one-way "Convert to
MJML" action turns a block template into MJML code. The draft is saved as you type and the server renders it again for the preview, shown in a
sandboxed iframe, so the app never renders template HTML itself. Admins can preview as a real person,
publish, and send tests. A flow's send step chooses a published template and a destination; a sync step
chooses a destination and the traits to keep in an external system.

`GET /api/v1/workspaces/{ws}/flows/{id}/versions/{n}/counts` also returns, per node, the delivery counts
read from the version's message rows when asked: `sent`, `send_failed` and `skipped`, a list of
`{reason, count}` sorted by reason. Test messages and messages still pending or sending are not counted.

## Template test sends

A test send sends one template through a real destination to one person who agreed to receive it: a
member of the workspace, at the address on their account, or an address an admin put on the allowlist.
The subject starts with `[TEST] `, links are not rewritten, there is no open pixel, and nothing is
written to the message log, the events or the caps. `{{ system.unsubscribe_url }}` and the
`List-Unsubscribe` header point to a `/u/{token}` link that answers "This was a test send. Nothing was
changed." and does nothing.

| Route | Needs | |
|-------|-------|-|
| `POST /templates/{id}/test-send` | write (admin with `person_id`) | body: `version` (absent: the draft), `destination_id`, `sender_identity_id`, `to` (`{"member": id}` or `{"address": "..."}`), `person_id` (optional; absent renders the sample person); answers 202 with the log row |
| `GET /test-sends` | read | the log, newest first; `template_id`, `limit`, `before` |
| `GET`, `POST /test-send-allowlist`, `DELETE /test-send-allowlist/{id}` | admin | `{"address"}` to add; the list shows only a masked hint (`q***@example.com`) |
| `GET /test-send-allowlist/options` | write | the same entries as `{id, name}`, `name` being the masked hint, for the test-send dialog |

Any other recipient is a 422 `recipient_not_allowed`. At most 10 test sends per member and 100 per
workspace start in any rolling hour; past that the answer is 429 with `Retry-After`. A suppressed
recipient is never sent to (`suppressed`), and an unverified sender ends `sender_unverified`. The job
checks the recipient again when it runs: a member must still be a member (and the account's current
address is used) and an allowlist entry must still exist, or the row ends `failed` with
`recipient_unavailable` and nothing is sent. The log keeps 13 months. The log stores no address; an
allowlist entry holds the hash, the masked hint and the normalised address sealed with the master key
like a credential, and no job payload ever holds an address. Erasing a person deletes the log rows and
the allowlist entries sent to or holding their addresses, and purges their queued test-send jobs. The
leader ends a `queued` row whose job is dead or gone as `failed: abandoned`.

## Sending domains

An email template publishes only with a sender identity on a verified sending domain (a publish
with any other sender is a 422 `sender_unverified`). Workspace admins manage both:

- `POST /api/v1/workspaces/{ws}/sending-domains` with `{"domain": "example.com"}` (optional
  `selector`, default `zirv`, and `spf_mechanism`, such as `include:spf.relay.example`) generates
  an RSA-2048 DKIM key, seals the private half (it is never returned) and answers with the three
  DNS TXT records to publish: DKIM at `<selector>._domainkey.<domain>`, SPF at the domain and DMARC
  at `_dmarc.<domain>`. The SPF value is a recommendation: a name holds one `v=spf1` record, so
  merge it with the one you have.
- A background job reads those records. The domain is `verified` only while all three are `ok`:
  DKIM when the published `p=` key matches, SPF when exactly one `v=spf1` record contains the
  configured mechanism, DMARC when any `v=DMARC1` record has a valid `p=`. It falls back to
  `unverified` when a check finds one missing or wrong; a name server that does not answer changes
  nothing and the check is retried. `POST .../sending-domains/{id}/verify` queues a check now;
  otherwise the scheduler re-checks every `sending.recheck_hours` (default 6).
- `POST /api/v1/workspaces/{ws}/sender-identities` binds a from name and an address on the domain.
- The pickers read `GET .../sender-identities/options`, `GET .../destinations/options` and
  `GET .../test-send-allowlist/options`. They need the write role (a `write` key, or any member). Each returns
  only ids, names and state (archived ones are listed and marked): never a setting, a credential or an
  address. A destination option and every destination body carry `capabilities`
  `{send, sync, erase}`, derived from the destination type and, for a provider, its manifest; the flow
  editor lists only destinations that can send in an `action.send` step and only those that can sync
  in an `action.sync` step. A read-only member still sees the chosen id.
- `GET` and `PUT /api/v1/workspaces/{ws}/sending/settings` read (any reader) and replace (admins) the
  workspace's frequency caps and quiet hours. A cap is `{channel, window, limit, mode}`: at most
  `limit` messages to one person in any `window` (an ISO 8601 duration in whole hours or days, up to
  `P31D`), and a message over it is `skip`ped (`frequency_cap`) or, in `defer` mode, held until a slot
  frees up. Quiet hours are `{start, end, days}` in the recipient's time zone (their `timezone` trait,
  else the workspace's): a message inside one waits for its end. Transactional mail is exempt from both.

## Email provider destinations

An email provider with an HTTP API is a destination of type `provider`, described by a manifest
instead of code. The same destination type can also keep people's profiles in an external system
(see Profile sync below). Its `config` is `{"manifest": {...}}`: the manifest format of the HTTP source
(`version`, `base_url`, `auth`, `headers`) with a `destination` section, and no `streams`. Auth is
`none`, `api_key`, `basic` or `bearer`; the manifest's `auth.credential` declares the credential
fields, including the secret that signs the provider's feedback, and the destination links a
workspace credential that holds them. The manifest holds no secret. Two fixtures are in
`crates/zirv-flow-connectors/tests/fixtures/providers/`; no real provider's manifest ships yet.

The `destination` section is data only (unknown keys, scripts and `{{ }}` markers are refused, each
issue named by its JSON pointer):

- `send`: `method`, `path`, `idempotency_header` (the message id is sent in it, so a retried
  delivery cannot duplicate at a provider that honours it) and `body`, whose `fields` place `from`,
  `from_name`, `reply_to`, `to`, `subject`, `html`, `text`, `headers` and `message_id` at JSON
  pointers, beside `static` values.
- `rate_limit`: `requests` per `per_seconds`; the default rate of a destination created without one.
- `errors`: rules (`status` or `"500-503"`, an optional `body` pointer and `values`, and an `action`:
  `retry`, `terminal` or `recipient_failure`); the first match wins. By default 408, 429 (which
  honours `Retry-After`) and 5xx retry, any other status is terminal, and a recipient failure is a
  hard bounce.
- `domain_status` (optional): the request that asks the provider about a sending domain, and the
  mapping of its answer to `verified`, `pending` and `failed`.
- `feedback`: how to read the provider's event type, your message id, the bounce kind and an optional
  event id, and `verification` (required): the signature header and prefix, an optional timestamp
  header with `tolerance_seconds`, and the credential field with the signing secret (HMAC-SHA256).

Give the provider `https://<host>/api/v1/destinations/{id}/feedback` as its webhook URL. The request
is public and the signature authorises it: it is verified with the destination's secret before the
body is read, and a missing or wrong signature or a timestamp outside the tolerance is a `401` that
writes nothing. A verified delivery, bounce or complaint is recorded as `email.delivered`,
`email.bounced` (`kind` is `hard` or `soft`) or `email.complained`, once per provider event (a retry
writes nothing). A hard bounce suppresses the address in every workspace of the organization and a
complaint in the destination's workspace; a soft bounce suppresses nobody. The address is the keyed
hash stored on the message when it was sent, so a message sent before this was recorded suppresses
nobody. Events for an unknown message, or one sent through another destination, get a `202` and are
counted in `zirv_flow_provider_feedback_total{outcome}`.

`GET /api/v1/workspaces/{ws}/destinations/{id}/domain-status?domain_id=` (admins only) shows the
provider's status of each DNS record next to ours. It only reads: it never marks a domain verified.

**Profile sync.** The `destination` section may also declare `upsert_profile` and `remove_profile`:
one request each, with a `path` that may hold the placeholder `{key}` once (the external key,
percent-encoded as one path segment), an `idempotency_header`, `key_from` (`{"identity": kind}` or
`{"trait": key}`), a `body` that places the key, the listed traits (`traits` as one object, or
`fields` one by one) and `static` values at JSON pointers, and the same `rate_limit` and `errors` as
`send`. `send` and `feedback` are then optional, as a pair: a manifest declares the email pair, a
profile operation, or both. The `action.sync` flow node (`destination`, `operation` of `upsert` or
`remove`, `trait_keys`, at most 50 and none for a remove) runs the operation through the
`delivery.sync` job, with the rate bucket, retries and verdicts of the other deliveries, and ends in
a `sync.succeeded` or `sync.failed` person event and a decision that hold no key and no trait value.
Keeping a segment in an external system is two flows, entry and exit: `docs/profile-sync.md` has the
manifest, the worked flows and the details.

**Erase call.** The `destination` section may declare `erase_profile`, with the shape of
`remove_profile` (`key` and `static` body placements only). When a person is erased, every
destination of that manifest that handled them (a message, a forwarded event or a sync) gets one
call, keyed by the `key_from` identity or trait, under an idempotency key that is stable across
retries. A destination without it, including `webhook` and SMTP, gets none.

## Event forwarding

A forwarding rule sends the events of one name to a destination, mapped to the payload you choose.
Only a destination type that carries events accepts a rule (`webhook` does; an email provider does
not: `422 destination_unsupported`). A workspace has at most 20 active rules (`422 too_many_rules`).

A rule has an exact `event_name`, an optional `filter` and a `mapping`:

- `filter` is the segment `where` list (`[{"prop": "total", "op": "gt", "value": 10}]`), all of which
  must hold. It supports `eq`, `neq`, `in`, `not_in`, `contains`, `starts_with`, `exists`,
  `not_exists`, `gt`, `gte`, `lt`, `lte` and `between`, and is checked by the segment validator, so
  a property needs a published event schema. A comparison needs the property to be present and of
  the value's type.
- `mapping` is an object of output key to source: `{"from": "event.id"}`, `event.name`,
  `event.time`, `event.received_at`, `person.id`, `{"from": "event.properties", "pointer":
  "/order/total"}` (a missing property is `null`), or `{"value": "shop"}` (a string, number or
  boolean). There are no expressions and no person traits.

The matching events are copied into an outbox in the transaction that stores them, so a rolled-back
write forwards nothing and a late-committing one is never skipped. This covers client events, synced
events, `webhook.*` and `email.*` events and `segment.entered` and `segment.exited`; a replay adds
none. With no active rule the cost is one indexed lookup per batch. An `event.forward` job per
destination delivers the rows with the webhook's signing, the guarded client and the destination's
rate limit, under the idempotency key `sha256(event id:destination id)`, which every retry and
re-drive reuses (the webhook sends it as `X-Zirv-Idempotency-Key`), so delivery is at least once.
A retryable answer backs off (5 s doubling to an hour, or the `Retry-After`); a terminal answer, or
the eighth attempt, makes the row a dead letter. Each retry and each dead letter appends a decision
(`event.forward`) with ids and a fixed code. Delivered rows lose their payload at once and are
removed after seven days.

All of these are for workspace admins and are audited:

- `GET`, `POST /api/v1/workspaces/{ws}/forwarding/rules` and `GET`, `PATCH`, `DELETE
  /api/v1/workspaces/{ws}/forwarding/rules/{id}`.
- `GET /api/v1/workspaces/{ws}/forwarding/dead-letters?destination_id=&limit=&before=` lists dead
  rows newest first with the error code, attempts and times; never the payload.
- `GET /api/v1/workspaces/{ws}/forwarding/status` gives each destination's queue `depth`, the
  `oldest_lag_seconds` of its oldest waiting row and its `dead` count.
- `POST /api/v1/workspaces/{ws}/forwarding/re-drive` with `{"id": ...}` (one row), `{"destination_id":
  ...}` or `{}` (every dead row) puts rows back in the queue with fresh attempts. It writes no
  event and starts no flow.

The metrics `zirv_flow_destination_forwarding_depth` and
`zirv_flow_destination_forwarding_lag_seconds` carry the same numbers, labelled by workspace and
destination id; attempts and outcomes are in `zirv_flow_destination_attempts_total` and
`zirv_flow_destination_outcomes_total`. Erasing a person deletes their outbox rows (the call that
erases them at the destination is separate), and a merge moves them to the survivor.

## Render host

Templates render in child processes, never inside the API or a worker, so a template
that allocates without bound fails its own render and nothing else. `serve` and `worker`
(and so `all`) start `render.pool_size` children at boot, each a copy of this executable
running the hidden `render-host` command, which an operator never runs by hand. A render
takes a free child, and a child that overruns `render.deadline_ms`, dies, or runs out of
memory is killed and replaced; only that render fails, with one of the reasons
`memory_limit`, `deadline`, `child_exited` or `protocol`.

On Linux each child limits its own address space to `render.memory_limit_mib` before it
reads its first request. Raise it if a legitimate render fails with `memory_limit`; the
loaded binary counts against it as well as the heap, so values under 64 are refused.
Other operating systems have no such limit: the child logs one warning at startup and
runs unlimited, which is for development only. Memory a child uses is outside the
worker's heap, so size the container for `render.pool_size` extra processes per role.

Metrics: `zirv_flow_render_duration_seconds{channel}`, `zirv_flow_render_wait_seconds`,
`zirv_flow_render_restarts_total` and `zirv_flow_render_failures_total{reason}`.

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

The ingestion and trait-storage benchmarks (architecture section 19 and OQ-1) live in
[docs/benchmarks/m2-ingestion.md](docs/benchmarks/m2-ingestion.md): method, environment, results against each
target, and the capacity ceilings at three resource sizes. The load generator is `tools/loadgen` and the
runbook is [scripts/bench/README.md](scripts/bench/README.md). Results are from a development machine, not
reference hardware.

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
- [Disclaimer](DISCLAIMER.md) -- operator responsibilities for a self-hosted tool
- [MIT License](LICENSE)
