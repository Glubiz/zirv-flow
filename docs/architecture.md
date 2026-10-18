# Zirv Flow architecture

Living document: the intended system and the decisions behind it. Change it
through pull requests. Decisions (section 20) are never rewritten in place; a
changed decision gets a new superseding entry, linked both ways. Anything not
yet decided is an open question (section 21). Delivery order: [roadmap.md](roadmap.md).

## 1. Purpose and scope

Zirv Flow (`Glubiz/zirv-flow`, MIT) is a self-hosted marketing automation tool:
one Rust binary, `zirv-flow`, plus PostgreSQL. Marketers connect internal and
external data sources, segment the people in their business from that data,
and run automation flows for everyone or for a segment. It targets the problems
marketers hit before any message is sent: data about one person spread across
systems that disagree; segments nobody can explain or date; integrations that
drop, duplicate or never delete records; consent and erasure handled elsewhere.

### 1.1 Goals for v1.0

| ID | Goal |
|----|------|
| G1 | Connect data: built-in database sources, declarative HTTP sources, CSV import, identify/track API, inbound webhooks. Syncs are incremental, idempotent and deletion-aware, with per-source freshness visible. |
| G2 | One profile per person: deterministic identity resolution, typed traits with per-trait source precedence, events, lists, consent, suppression. |
| G3 | Segments marketers can trust: visual builder, live counts, per-person explanation, visible data age, entry/exit events. |
| G4 | Durable, versioned, testable flows triggered by segment changes, events, schedules, manual starts or the API. |
| G5 | Compliant delivery: email and webhooks first; consent, suppression, frequency caps, quiet hours and rate limits enforced in core; delivery and engagement fed back as events. |
| G6 | Operable by a small team: binary + Postgres, compose quickstart, metrics, health checks, documented backup/restore/upgrade. |
| G7 | Message content without code: a channel-aware template builder (email blocks rendered to responsive HTML and text, a webhook payload editor) with brand theme, snippets, merge tags from the field registry and per-block conditions; previews come from the same renderer as sends. |
| G8 | Govern and investigate events: versioned schemas, visible validation and rejects, redaction, live debugging, exploration and a unified person timeline. Every derived action is traceable to its typed cause, including skips and failures. |
| G9 | Optional, safe authoring assistance: configured LLMs draft and explain Zirv Flow's typed artifacts through the same validators and RBAC as the API; a person always reviews and accepts a draft. |

### 1.2 Non-goals for v1

- More than one organization per deployment; active-active multi-region.
- Querying sources live without copying them (federated mode is a later milestone, ADR-001).
- Running a mail transfer agent or managing IP reputation (mail goes to SMTP relays and provider APIs).
- SMS, push, in-app and ad-audience channels and their builders (their content types are reserved in the template model, section 11.1); probabilistic identity matching; predictive scoring.
- Analytics, attribution and campaign reporting beyond delivery and per-node flow counters.
- AMP email; hosted landing pages or forms.
- Autonomous assistant actions; model hosting, training or fine-tuning by Zirv Flow.
- Third-party code plugins (ADR-004); sub-second membership for time-window segment conditions (scheduled recompute, section 8.4).

## 2. Principles

1. **The binary and Postgres are the whole product.** More infrastructure needs measured evidence (ADR-003).
2. **Every number is explainable.** Counts, memberships and skipped sends link to the data and rule behind them.
3. **Freshness is a value.** Every segment result and send decision carries the age of its inputs.
4. **Compliance is data, enforced at one boundary.** Consent, suppression and erasure live in the profile store and are checked in the send pipeline, not left to flow design.
5. **Deterministic core, effects at the edge.** Segment compiler, flow interpreter and template renderer are pure functions of their input and an injected clock; I/O lives in job handlers. The segment compiler takes an explicit `now` and reads the clock at the edges (8.2); the flow engine adopts the same seam when it is built (M5).
6. **At-least-once execution with idempotency keys.** Exactly-once is never claimed for a system that cannot honour a key.
7. **The workspace is the isolation unit.** Every row carries `workspace_id`; every query and job is scoped by it.
8. **API first.** The UI uses the public `/api/v1`; every milestone ships API and UI for its feature together.
9. **Fail visibly.** Rejected records, failed steps and skipped sends are stored with a reason and shown in the UI.
10. **Causality is data.** Every derived action carries a trace and typed cause across queues and external effects; operational logs are not the system of record for product decisions.
11. **Assistance proposes, people decide.** LLM output is untrusted, workspace-scoped and draft-only; deterministic validators and explicit acceptance remain authoritative.
12. **Module boundaries are executable rules.** Crate dependencies, feature-module APIs and frontend imports follow the allowlists in section 5, and CI rejects violations.

## 3. System context

```mermaid
flowchart LR
  subgraph deploy["Zirv Flow deployment"]
    bin["zirv-flow binary: API + embedded UI, workers, scheduler"]
    pg[("PostgreSQL: profiles, events, decisions, jobs, config")]
  end
  marketer["Marketers (browser)"] -->|"HTTPS: UI and /api/v1"| bin
  operator["Operator (config, CLI, backups)"] -->|"config, env, CLI"| bin
  apps["Websites, apps, forms"] -->|"HTTPS: identify/track, inbound webhooks"| bin
  bin -->|"pull: incremental sync"| sources["Data sources: internal databases, HTTP APIs, CSV"]
  bin <-->|SQL| pg
  bin -->|"deliver, sync"| dest["Destinations: email (SMTP, provider APIs), webhooks, CRMs"]
  bin -->|"optional, allowlisted HTTPS"| llm["Configured LLM providers or local model servers"]
  dest -->|"delivery webhooks"| bin
  dest --> recipients["Recipients (inbox, links, unsubscribe)"]
  recipients -->|"opens, clicks, unsubscribes, image fetches"| bin
  obs["Prometheus and log pipeline"] -->|"scrape /metrics"| bin
```

## 4. Runtime and process view

### 4.1 Roles

One binary, several roles, each a long-running loop. `all` runs every role in
one process and is the default (bare `zirv-flow` and the container image).

| Command | Instances | Responsibilities |
|---------|-----------|------------------|
| `serve` | any, stateless | `/api/v1`, ingestion, tracking/unsubscribe, inbound webhooks, template preview, image assets, embedded UI, `/healthz`, `/readyz`, `/metrics` |
| `worker` | any, stateless | Claim and execute jobs; `--kinds` dedicates a pool (e.g. `delivery`) |
| `scheduler` | any, one active leader | Due schedules to jobs, lease reaping, partition maintenance, retention, stale-source detection |
| `all` | one per host | All three roles in one process (default) |
| `migrate`, `admin ...`, `config check` | one-off | Apply migrations; create owner, rotate master key, replay erasures; validate config (secrets redacted) |

### 4.2 Cooperation through Postgres

- Roles never call each other; all coordination is rows in Postgres.
- **Transactional enqueue:** a state change and the jobs it causes commit in one transaction, so a committed change always has its follow-up job and a rolled-back one never does. No dual write, no outbox relay.
- Jobs are claimed in a short transaction and executed outside it (section 14).
- `LISTEN/NOTIFY` wakes workers as a latency hint; fixed-interval polling is the correctness path.
- The scheduler leader holds a session-level `pg_try_advisory_lock`; if its connection drops another scheduler takes over. Schedule slots carry unique keys, so a brief two-leader overlap cannot fire a slot twice.
- Due-time comparisons use the database's `now()`, so host clock skew cannot fire timers early.

### 4.3 From event to email

```mermaid
sequenceDiagram
  participant App as Website or app
  participant S as serve
  participant PG as PostgreSQL
  participant W as worker
  participant D as Email destination
  App->>S: POST /api/v1/ingest/track (order_placed)
  S->>PG: append to ingest_inbox
  S-->>App: 202 Accepted
  W->>PG: claim inbox batch, one tx - resolve identity, insert event, mark dirty, enqueue segment.reevaluate
  W->>PG: claim segment.reevaluate, one tx - membership diff, segment.entered event, enqueue flow.enroll
  W->>PG: claim flow.enroll, one tx - enrollment pinned to version, engine step, enqueue delivery.send
  W->>PG: claim delivery.send, check consent, suppression, cap, quiet hours, rate
  W->>D: send with idempotency key
  W->>PG: one tx - message sent, email.sent event
```

### 4.4 Shutdown and failure

On `SIGTERM`, `serve` drains requests, workers stop claiming and finish
in-flight jobs within a grace period, and the scheduler releases its lock. Jobs
left unfinished, by shutdown or crash, are retried when their lease expires;
idempotency keys make that safe (section 14). If Postgres is down, `/readyz`
fails, loops back off and ingestion returns `503`; v1 does not buffer locally.

## 5. Module and crate layout

M0 is a single crate. The Cargo workspace arrives in M1: `crates/<name>` for
each crate below (`zirv-flow-ai` joins in M3, `zirv-flow-identity` in M1 with OIDC sign-in and `zirv-flow-templates` in M6), plus `web/` (TypeScript +
React SPA, Vite, React Flow canvas).

| Crate | Contents | Independent reason to exist |
|-------|----------|-----------------------------|
| `zirv-flow` (bin) | CLI, config, role startup, embedded web assets | The single deliverable; thin composition root |
| `zirv-flow-core` | Domain types, ids, validation, segment AST, flow graph, `Clock` trait (8.2) | Shared vocabulary; dependency set kept pure (no sqlx, HTTP client or tokio networking) |
| `zirv-flow-store` | sqlx pool, migrations, repositories, Postgres job queue primitives | Sole owner of the profile store: one migration stream, one sqlx offline cache. Queue SQL lives here because enqueue must join the caller's transaction. |
| `zirv-flow-segments` | AST to parameterized SQL compiler, dependency analysis, reference evaluator | Most correctness-critical code, own golden corpus. Pure: emits SQL + params that `store` executes. Used by `api` (preview, explain) and `jobs` (recompute). |
| `zirv-flow-engine` | Deterministic flow interpreter, simulation harness | Pure, so simulation needs no I/O. Used by `api` (test mode) and `jobs`. |
| `zirv-flow-connectors` | `Source`/`Destination` traits, connector runtime, built-ins, HTTP manifests, email transport | Heavy, fast-moving external dependencies (HTTP, SMTP, source DB drivers). Depends on `core` only: yields records and checkpoints, never writes the profile store. |
| `zirv-flow-identity` | OIDC discovery, JWKS and the authorization-code exchange with ID-token verification (`openidconnect`, rustls) | Every outbound call to the operator-configured identity provider lives here, so `api` stays inbound-only. Depends on `core` only. The issuer is operator-configured, so the M3 egress policy does not apply to it. |
| `zirv-flow-ai` | Provider trait and OpenAI-compatible and Anthropic adapters; structured tool-call orchestration, bounded repair, budgets, audit inputs and deterministic fake provider | Optional outbound protocols and HTTP dependencies change independently from the deterministic domain crates. The crate coordinates existing typed tools and validators but neither defines nor bypasses them. |
| `zirv-flow-templates` | Template document types and schema migrations, MiniJinja sandbox, rich-text sanitizer, MJML generation and rendering via `mrml`, text-part generation, webhook payload rendering, publish validation | Pure renderer used by `api` (preview) and `jobs` (send), so preview and send share one code path. Own golden corpus (per-block renders, migrations). Its dependencies (`mrml`, MiniJinja, sanitizer) are pure but too heavy for `core`, and `connectors` cannot be held pure by the `cargo tree` check. Starts in M6 with code templates; M7 adds block documents. |
| `zirv-flow-api` | axum router, auth, OpenAPI, ingestion, tracking, static UI serving, assistant tools (`AiTool` implementations and their registry) | HTTP surface; neither `api` nor `jobs` depends on the other |
| `zirv-flow-jobs` | Worker and scheduler loops, job registry, job handlers | Composition point for background work: handlers combine `store`, `segments`, `engine`, `templates`, `connectors`, which no other crate needs together |

```mermaid
flowchart LR
  bin[zirv-flow] --> api & jobs & store
  api --> segments & engine & templates & connectors & ai & identity & store & core
  jobs --> segments & engine & templates & connectors & ai & store & core
  segments & engine & templates & connectors & ai & identity & store --> core
```

### 5.1 Allowed crate dependencies

The table is an allowlist of direct workspace-crate dependencies. A crate may
depend on fewer crates than listed; any workspace edge not listed fails CI.

| Crate | May depend directly on |
|-------|-------------------------|
| `zirv-flow` (bin) | `zirv-flow-api`, `zirv-flow-jobs`, `zirv-flow-store` |
| `zirv-flow-core` | none |
| `zirv-flow-store` | `zirv-flow-core` |
| `zirv-flow-segments` | `zirv-flow-core` |
| `zirv-flow-engine` | `zirv-flow-core` |
| `zirv-flow-connectors` | `zirv-flow-core` |
| `zirv-flow-ai` | `zirv-flow-core` |
| `zirv-flow-identity` | `zirv-flow-core` |
| `zirv-flow-templates` | `zirv-flow-core` |
| `zirv-flow-loadgen` (tools/loadgen) | none |
| `zirv-flow-api` | `zirv-flow-core`, `zirv-flow-store`, `zirv-flow-segments`, `zirv-flow-engine`, `zirv-flow-connectors`, `zirv-flow-ai`, `zirv-flow-identity`, `zirv-flow-templates` |
| `zirv-flow-jobs` | `zirv-flow-core`, `zirv-flow-store`, `zirv-flow-segments`, `zirv-flow-engine`, `zirv-flow-connectors`, `zirv-flow-ai`, `zirv-flow-templates` |

`api` and `jobs` never depend on each other, and no crate depends on the binary.
The pure crates (`core`, `segments`, `engine`, `templates`) never depend on an
I/O crate, sqlx, an HTTP client or tokio networking. `store` owns sqlx;
`connectors`, `ai` and `identity` are the outbound-I/O crates.

Two cross-cutting policies add no crate and no edge. The egress policy (16.1) is
pure code in `core::egress` (address classification, allowlist matching, the
decision type); `connectors` and `ai` each enforce it at their own connect point.
Envelope crypto (16.2) is pure code in `core::secrets` next to `MasterKey`;
`store` holds the ciphertext, and `api` and `jobs` open a secret and pass the
typed plaintext credential into `connectors` or `ai`, which never decrypt.

### 5.2 Feature modules inside crates

Each crate is split into vertical feature modules for the domain areas it owns:
`sources`, `people`, `events`, `segments`, `flows`, `templates`, `delivery` and
`assistant`. A crate creates only the modules it needs. Shared code stays small
and earns a domain-neutral name; feature code does not move into a generic
`utils` module.

```text
crates/zirv-flow-api/src/
  lib.rs
  sources/mod.rs
  people/mod.rs
  events/mod.rs
  segments/mod.rs
  flows/mod.rs
  templates/mod.rs
  delivery/mod.rs
  assistant/mod.rs
```

Items are `pub(crate)` by default. A feature's supported surface is the set of
functions and types re-exported by its `mod.rs`; a crate's external surface is
re-exported by `lib.rs`. Code never reaches into another feature's internal
submodules. Cross-feature calls use the owning feature's public functions and
types. File and module names use domain terms, not transport or implementation
terms, unless the module owns that transport.

### 5.3 Extension points

These are the only implementation registries. Adding an implementation means
one module that implements the owning trait and one line in its registry; only
the registry enumerates implementations.

| Extension | Trait | Owning crate | A new implementation adds |
|-----------|-------|--------------|---------------------------|
| Connectors | `Source` | `zirv-flow-connectors` | One source module and one source-registry line |
| Destinations and channels | `Destination` | `zirv-flow-connectors` | One destination module and one destination-registry line |
| Template content types | `TemplateContent` | `zirv-flow-templates` | One content-type module and one content-registry line |
| Job kinds | `JobHandler` | `zirv-flow-jobs` | One handler module and one job-registry line |
| AI tools | `AiTool` (defined in `zirv-flow-ai`) | `zirv-flow-api` (`assistant` feature module) | One tool module and one tool-registry line; tools live in `api` because they call validators and store reads that `ai` may not depend on (5.1) |

No other registry is introduced. Domain catalogs such as the field and event
schema registries store workspace data; they are not implementation registries.

### 5.4 Frontend modules

`web/src/features/` contains feature folders named for the same domain areas.
Each feature exposes its supported surface through `index.ts`; another feature
imports only that surface, never an internal file. Shared presentation
components live in `web/src/ui/`, and the generated OpenAPI client has its own
top-level folder. Features may use both, but never import from `web/src/app/`;
the app shell composes features, not the reverse.

### 5.5 Enforcement

- From M1, CI reads `cargo metadata` and fails on every workspace dependency
  edge absent from section 5.1, naming the disallowed edge.
- The existing `cargo tree` purity check rejects sqlx, HTTP clients and tokio
  networking in `core`, `segments`, `engine` and `templates`; sqlx remains a
  direct dependency of `store` only.
- Frontend lint rejects deep imports across features and imports by a feature
  from the app shell.
- Rust review treats `pub(crate)` as the default and rejects access to another
  feature's internal modules or an implementation listed outside its registry.
- Reviews never loosen an allowlist, purity rule or import boundary merely to
  make a change pass. A boundary change requires an architecture decision.

The engine evaluates branch conditions through an injected
`ConditionEvaluator` (production: `segments` + `store`; simulation: the
reference evaluator). The renderer never evaluates conditions itself: it lists
a template's block conditions, the handler evaluates them in one
`ConditionEvaluator` call, and the results go into the render as plain inputs
(section 11.4). CI builds `web/` before the release build; `build.rs` embeds `web/dist`, or a
placeholder page in the api crate when it is absent, so `cargo build` works without Node. The OpenAPI document
(utoipa) is committed, drift-checked and feeds the SPA client.

## 6. Domain model

One organization per deployment (implicit). **Workspaces** (brands,
environments) are the isolation unit. **Users** are deployment-wide and join
workspaces through **memberships** with a role; **API keys** belong to one
workspace and carry scopes. Every domain table leads its key with
`workspace_id`; ids are UUIDv7 (time-ordered, index-friendly).

```mermaid
erDiagram
  WORKSPACE ||--o{ SOURCE : has
  SOURCE ||--o{ STREAM : exposes
  STREAM ||--o{ SYNC_RUN : "synced by"
  STREAM ||--o{ SOURCE_RECORD : "ledger of"
  SOURCE_RECORD }o--o| PERSON : "contributes to"
  WORKSPACE ||--o{ PERSON : contains
  PERSON ||--o{ IDENTITY : "identified by"
  WORKSPACE ||--o{ TRAIT_DEFINITION : registers
  PERSON ||--o{ EVENT : performed
  WORKSPACE ||--o{ EVENT_SCHEMA : governs
  EVENT_SCHEMA ||--o{ EVENT_SCHEMA_VERSION : versions
  EVENT_SCHEMA_VERSION ||--o{ EVENT : validates
  WORKSPACE ||--o{ INGEST_REJECT : records
  WORKSPACE ||--o{ DECISION_LOG : traces
  PERSON ||--o{ CONSENT : holds
  WORKSPACE ||--o{ SUPPRESSION : maintains
  LIST ||--o{ LIST_MEMBER : contains
  PERSON ||--o{ LIST_MEMBER : "member of"
  SEGMENT ||--o{ SEGMENT_MEMBERSHIP : tracks
  PERSON ||--o{ SEGMENT_MEMBERSHIP : has
  FLOW ||--o{ FLOW_VERSION : "published as"
  FLOW_VERSION ||--o{ ENROLLMENT : pins
  PERSON ||--o{ ENROLLMENT : "enrolled in"
  ENROLLMENT ||--o{ STEP_EXECUTION : records
  STEP_EXECUTION ||--o| MESSAGE : produces
  DESTINATION ||--o{ MESSAGE : delivers
  WORKSPACE ||--o{ TEMPLATE : owns
  TEMPLATE ||--o{ TEMPLATE_VERSION : "published as"
  TEMPLATE_VERSION ||--o{ MESSAGE : renders
  TEMPLATE_VERSION }o--o{ SNIPPET : references
  SNIPPET ||--o{ SNIPPET_VERSION : "published as"
  TEMPLATE_VERSION }o--o{ ASSET : embeds
  WORKSPACE ||--|| BRAND_THEME : styles
  BRAND_THEME ||--o{ BRAND_THEME_VERSION : "published as"
  BRAND_THEME_VERSION ||--o{ MESSAGE : styles
  BRAND_THEME_VERSION }o--o| ASSET : "logo"
  SNIPPET_VERSION }o--o{ ASSET : embeds
  WORKSPACE ||--o| AI_SETTINGS : configures
  WORKSPACE ||--o{ AI_CALL_LOG : audits
```

| Entity | Purpose and key rules |
|--------|-----------------------|
| Person | Resolved individual. `traits jsonb` is materialized from contributions; `merged_into` is set on the losing side of a merge. For traits marked "indexed for segments" (OQ-1, 21.1), `people` also carries a fixed set of generic typed slot columns per promotable scalar type: 8 string, 8 number, 6 timestamp, 4 date and 4 boolean per workspace, each with a partial btree index on `(workspace_id, slot)` where the slot is not null. `string_list` traits stay in `jsonb`. | Slots are live: columns `trait_s1..s8`, `trait_n1..n8`, `trait_t1..t6`, `trait_d1..d4` (UTC midnight) and `trait_b1..b4`; a definition holds one in state `backfilling`, `ready` or `retiring`, and every writer of `people.traits` takes `pg_advisory_xact_lock_shared(7000, hashtext(workspace))` in its own statement before reading the assignments while every assignment change takes the exclusive form.
| Identity | `(kind, normalized value)` per person: email, phone, anonymous_id, external id namespaced per source. Unique on `(workspace_id, kind, value)`. |
| Trait definition | Field registry: key, type (string, number, boolean, timestamp, date, string list), precedence policy, PII flag, channel-address flag. Values are validated on write, so compiled reads cast safely. |
| Source record | Ledger row per `(source, stream, record key)`: mapped payload, content hash, person, last seen run, deletion mark. These are the per-source contributions traits are computed from. |
| Event schema, version | Registry entry per event name with immutable versions: typed properties, required fields, enums, descriptions and a PII flag per property; validation mode `allow`, `warn` or `block`. The workspace policy creates a draft schema for an unplanned name or rejects it. |
| Event | Append-only `(name, schema_version, occurred_at, received_at, properties, source, event_id, message_id, trace_id, validation_state)`. Range-partitioned by month on `occurred_at`; ingest redaction runs before storage. A client `messageId` deduplicates retries for effectively-once storage. |
| Ingest reject | Rejected customer event or sync record with reason, masked sample, source and trace. It is visible, retained separately and replayable after its schema or mapping is fixed. |
| Decision log | Append-only, time-partitioned typed records for derived actions and their causes. It stores durable ids and reasons but no personal-data values; `trace_id` links the tree across jobs, events, messages and forwarding. |
| Consent | Recorded consent: state per `(person, channel, topic)`: `subscribed`, `unsubscribed`, `pending_confirmation`, `unknown`; plus append-only history with source and evidence. It is the input to the workspace consent policy (10.1), which decides effective consent. |
| Suppression | Identifier-level block (hard bounce, complaint, manual, erased), stored as an HMAC of the normalized identifier so it outlives erasure. |
| List; segment, version, membership | Static set of people (manual, CSV, API, flow action). Segment definition AST; membership rows with `entered_at`/`exited_at`, current members `exited_at IS NULL` (partial unique index). Erasure and person deletion remove a person's membership rows; a merge closes the merged person's rows, and the survivor's `merge` change class (7.1) re-evaluates it. |
| Flow, version, enrollment; message | Versions are immutable once published and enrollments are pinned to one. One message row per send attempt; `messages.idempotency_key` is unique. The message records the exact template, snippet and theme versions it was rendered from. |
| Template, template version | Channel (`email`, `webhook`; `sms`, `push`, `in_app` reserved), category, mode (`blocks` or `code`) and a versioned content document (section 11). Versions are immutable once published. |
| Snippet, snippet version | Reusable block fragment (e.g. legal footer). Templates reference the snippet, and rendering uses its current published version. |
| Brand theme, theme version | One per workspace: colours, fonts, logo asset, default footer snippet. Published versions are immutable; rendering uses the current one. |
| Asset | Uploaded raster image (bytes, sniffed type, dimensions, content hash), served publicly from `server.public_url`. Retained while any published template, snippet or theme version references it. |
| AI settings | Per-workspace enablement, provider/model choice, budget, the opt-in transcript window (off by default) and PII opt-in. Deployment configuration declares the permitted providers and models. |
| AI call log | Audit record for each assistant call: workspace, user, feature, provider, model, tokens, estimated cost, latency, outcome and resulting draft id; transcript content is kept only inside the workspace's opt-in window (13.4). |

As built (lists): `lists` holds `member_count`, kept exact under a row lock on the list;
`list_members` is keyed `(list_id, person_id)` and `list_member_changes` is the history.
`store::lists` adds and removes in one transaction that locks the people (resolved through
`merged_into`, re-resolved if a merge wins the race) before the list, the same order a merge
uses, and writes one history row and one `list.membership` decision (ids only) per real
change. `merge_people` calls `lists::move_on_merge`, which gives the survivor the union of
both memberships with the earlier `added_at`, deletes the loser's rows, corrects the counts
and re-points the loser's history. Reads need `read`; writes need `write` (any member for a
session) and CSRF. Identifier values on the member page use the same masking as conflict
review. The CSV import resolves identifiers to existing people and never creates one; its
dry run runs the real transaction and rolls it back, so its counts are exact.

As built (consent, topics and suppressions):

- **Rules.** `core::consent` holds the pure rules: `Channel` (only `email`), `ConsentStatus`,
  `TopicPolicy`, `allowed(channel_wide, topic, policy)` for the M5 send gate and `merge` for
  two people's states. `core::people::identifier_hash` is the `Identifier`-purpose HMAC of
  `<kind>:<normalized value>`; the workspace is left out so organization-wide suppression
  (OQ-19, still open) and the erasure ledger can reuse a hash.
- **Tables** (migration `0008_consent.sql`). `topics` (unique `(workspace_id, key)`),
  `consent` (one row per person, channel and topic, `topic_id` null for the whole channel,
  unique with `NULLS NOT DISTINCT`), `consent_changes` (the history: from and to status,
  source, evidence up to 4 KiB, actor, trace) and `suppressions` (primary key
  `(workspace_id, identifier_hash)`). A trigger refuses any update of `consent_changes` other
  than moving `person_id` to the person its owner was merged into, which a merge needs;
  deletes stay possible for erasure. `suppressions` carries an opaque `id` for paging, so a
  cursor never holds a hash.
  It has no `person_id` and survives erasure by design; the other three are
  covered by the isolation tests and the erasure scan.
- **Writes.** `store::set_consent` locks the person row, follows `merged_into`, treats a write of
  the status already held as a no-op (no history row, no decision), and otherwise upserts the
  state, appends the history row and a `consent.change` decision (reason is the source code,
  subjects are the person and the change). `store::consent::move_on_merge` runs inside the merge
  transaction: per channel and topic of the loser it applies `core::consent::merge` against the
  survivor, records a `merge` history row when the status changes, deletes the loser's states
  and re-points its history.
- **Suppressions.** `add_suppression` keeps the first reason, `erased` overwrites; `erased`
  rows are never deleted by `remove_suppression`. Import and export hash and stream in the API
  crate; the store sees hashes only.

## 7. Data sources and ingestion

### 7.1 Paths and the common write path

| Path | Mechanism | Idempotency | Freshness signal |
|------|-----------|-------------|------------------|
| Sync (pull) | `sync.run` job per stream, resuming from a checkpoint | Ledger content hash; checkpoint committed with the data | Last successful run, cursor high-water mark |
| Identify / track (push) | `serve` durably appends to `ingest_inbox` before returning `202`; workers drain it in batches | Client `messageId`, else server-assigned | Oldest unprocessed inbox row |
| CSV import | Upload, map, dry run, then run like a sync | Key column, else file hash + row number | Import completion |
| Inbound webhook | `/hooks/{id}`, signature check, then a durable append to `ingest_inbox` like identify; mapping runs in the drain | Configured dedupe field | Last received |
| Contacts API (push) | Create, update and delete contacts by key, singly or in bulk, through the inbox like identify (7.6) | Idempotency key, else contact key plus payload hash | Oldest unprocessed inbox row |
| Kafka stream | A long-running consumer job per stream, not a `sync.run` batch (7.6, 14); the partition offsets are the stream checkpoint, committed in the apply-batch transaction (7.6) | Topic, partition and offset | Consumer lag per partition |

Every path ends in **apply-batch**, one transaction per batch (default 500
records): normalize identifiers; resolve or create the person (merging if
needed); upsert ledger contributions and recompute affected traits; insert
events with dedupe; write coalesced dirty marks (`person_changes`); enqueue
follow-ups (segment re-evaluation, event triggers, event-wait wakeups); commit
the stream checkpoint. Customer-event schema validation, redaction, reject
handling, lateness and trace propagation are defined once in section 12 and
used by every ingestion path.

The dirty set is `person_changes`, one row per `(workspace_id, person_id)`. Each row carries a
`smallint` change-class bitmask (traits, events, lists, consent, identifiers, merge), OR-ed
on conflict, so repeated marks coalesce and keep every class that changed. A mark made after
`segment.reevaluate` claimed a row re-inserts it (8.4). `segment.*` events never mark a person
dirty: a membership change reaches dependent segments through topological evaluation, not
through the dirty set. A mark carries the trace and cause of the latest change, which the entries and exits it causes continue; the writer's transaction enqueues `segment.reevaluate` once, only to cut latency.

As built (identify / track ingestion): `POST /api/v1/ingest/{identify,track,batch}` validate
structure, charge the key's in-process token bucket (items, per `serve` process), and append
one row per item to `ingest_inbox` with the request's trace id in a single transaction that
also enqueues `ingest.drain` for the workspace (`unique_key = ingest.drain:<workspace>`). The
inbox row's `message_id` is the client's or a server-assigned UUID v7. Each row also
stores `affinity`, the first byte of the SHA-256 of its record key (the strongest identifier)
modulo 64, fixed forever; rows appended before the column existed, or by a pre-0.29 serve
during a rolling upgrade, hold NULL. A drain job first buckets the workspace's NULL rows (a
blocking `FOR UPDATE` in a short transaction of its own, so it waits out an erasure and cannot
deadlock; the bucket comes from the same function as the append's), then runs
`ingest.drain_shards` (default 8) loops concurrently, each owning the buckets with `affinity % shards =
index`, each a tokio task of its own owned by the job (so the shards use several runtime threads,
and a lost lease aborts them all). A batch is up to `ingest.drain_batch_size` of the
oldest rows of the shard's buckets, picked through `(workspace_id, affinity, id)`, so a scan
never walks another shard's backlog. A key lives in one bucket, so its
rows still apply in id order; two shards may share a person under different keys, and a
deadlock between them is the retryable `40P01`. The worker pool holds `concurrency * (1 +
shards) + 1` connections. Each loop is `drain_batch`: in one transaction it picks the
batch's ids without locking, locks those rows with `FOR UPDATE SKIP LOCKED` (a batch never
applies past a row it could not lock: the rest of that bucket's picked rows wait for the next
batch, and a batch that takes nothing ends the loop), applies them in id order through the
transaction-taking profile and event paths of apply-batch, deletes them and commits, so a crash leaves them in place and concurrent
drainers never apply a row twice. A retryable error retries the batch with jittered backoff;
any other error applies the batch's rows one at a time, each in a savepoint, and a row that
still fails becomes an `apply_failed` ingest reject. The job never re-enqueues itself, since it
holds its unique key; the scheduler leader enqueues a drain for each workspace whose oldest
row (one `(workspace_id, id)` index probe per workspace) is over 2 seconds old. Row ids come
from a sequence, so two requests that commit out of order drain in id order, not commit order.

As built (M3, webhooks): a hook is a source of kind `webhook` (capability `push`, no config, no
credential, `read` yields nothing) with one stream `payload`, plus a `webhooks` row that holds
the random v4 id of the URL, the signature scheme (`core::signature`: HMAC-SHA256 over
`"{timestamp}.{body}"`, or over the raw body when `timestamp_header` is NULL), the dedupe field
and the id of the sealed shared secret (`encrypted_secrets`, purpose `webhook_secret`). The
source, stream, secret and row are created and removed together, and only through
`/api/v1/workspaces/{ws}/webhooks`. `POST /hooks/{id}` is public and ordered cheapest first: a
declared length over `ingest.max_webhook_bytes`, the id, one query for the hook (it must be
receiving), the per-hook token bucket (charged only for ids that resolved, so the limiter holds
at most one bucket per stored hook), the body read with the same cap, the secret opened into a
zeroizing buffer, the signature and timestamp verified with a constant-time compare, then the JSON
object and its dedupe value. Only then does one transaction append an `ingest_inbox` row of kind
`webhook` (`webhook_id` set, `api_key_id` NULL, `ON DELETE CASCADE` from the hook; the row's
`item` is `{"payload", "signed_at"}`; `affinity` is the bucket of the hook id, so one hook
drains in id order in one shard), enqueue `ingest.drain` and, at most once a minute per hook per
process, report the stream through `record_stream_outcome`. Mapping runs in the drain, not at the
endpoint: it loads each batch's hooks and the trait catalog once, maps a payload with
`core::mapping::map_json` (the declared fields are `Mapping::root_fields`, so an absent field is
an empty cell), and applies the profile record (observed at the signed time, so an older replay
cannot overwrite a newer trait; the ledger keeps one row per record key, so the drain drops the
traits a later-observed payload already set), the event (message id `wh:{hook}:{dedupe value}`)
and the list memberships through the existing apply paths. A refused mapping becomes an
`ingest_rejects` row through the writer the sync batch uses (`insert_record_rejects`, with no run
id), and a row that cannot be applied is isolated as `apply_failed` like any inbox row.

As built (CSV import): `POST /imports` streams the file into `import_chunks` (1 MiB each, written
with the `imports` row in one transaction, so a refused upload leaves nothing) while the SHA-256,
the row count, the header and the first rows are taken with the pure incremental reader (over `csv-core`) in
`core::csv_file`. The mapping is the pure `core::mapping` (column rules, the bounded transform set,
save-time validation, row errors as codes), which the M3 sync runs will reuse. `import.dry_run`
and `import.run` are job kinds that read the chunks in batches of 500 rows; each batch of a run is
one transaction on a connection beside the job's, which applies the profile records through the
transaction-taking profile path, adds list members, writes the rejects and advances `rows_done`,
fenced on the import still being `running` at that checkpoint (a cancel or a second attempt makes
the batch a no-op). The record key is the key column's value, else `<file sha256>:<row number>`,
under the import's own source and stream. A run fails with `reject_threshold` once more than 1,000
rows are done and the rejected share is over the import's threshold (5%); applied batches stay.
The worker's lease heartbeat covers the long run, so there is no segmenting. The scheduler
leader's hourly upkeep deletes the chunks 7 days after an import ends and the rejects (whose
`raw` cells are admin-only personal data) 30 days after they were written.

### 7.2 Sources, streams and sync runs

A **source** is a connector instance (type, config, encrypted credentials,
schedule) exposing **streams** (table, query, API resource). Each stream maps to
`people` (identifiers, traits), `events` (name, timestamp, person identifier,
properties) or `list_membership`. Custom object types are post-1.0 (OQ-4): in v1
order- or account-like data arrives as events with typed properties or as
traits, and the store model is unchanged.

| Sync mode | Use | Deletion detection |
|-----------|-----|--------------------|
| Incremental (cursor field) | Default where an `updated_at`-like field exists | Delete signal in the mapping (soft-delete field, deleted-records endpoint), or a periodic key-only reconciliation run |
| Full refresh | Small data, no cursor | Mark and sweep: not seen in a successful complete run means tombstoned |
| Push | API, webhooks | Explicit delete calls |
| Stream | Kafka topics | A tombstone (null value) for the record key, or a mapped delete field |

```mermaid
stateDiagram-v2
  [*] --> queued
  queued --> running: worker claims
  running --> running: batch and checkpoint committed together
  running --> succeeded: stream exhausted, sweep applied
  running --> failed: retries exhausted or reject threshold exceeded
  running --> cancelled: operator cancels
  running --> running: lease expired, job re-delivered, resume from checkpoint
```

A checkpoint is opaque connector state (cursor, page token, LSN) written in the
same transaction as the batch it covers, so a resumed run neither skips nor
re-applies data. Long runs heartbeat their lease. One run per stream at a time
(job unique key). The sweep runs only after a run **succeeded**: a failed or
cancelled run never deletes. A run also fails, without sweeping, when its reject
rate exceeds the stream threshold (default 5%), so a broken mapping cannot
cascade into deletions. Rejects (type mismatch, missing identifier, bad
timestamp) are kept in `ingest_rejects`, the table identify, track and import
rejects already use, with reason and raw values and a `sync_run_id` that ties
each to its run. A Kafka stream has no `sync.run`: its consumer job (7.6, 14)
applies the same reject rule per message.

As built (M3): `sync_runs` holds the state, trigger, counters, per-run checkpoint, attempt,
trace id and error code; a partial unique index allows one `queued` or `running` run per
stream. A unit of at least 500 records commits in one transaction that first locks the job's
lease (`store::lock_lease`) and then the run row, so a lost lease or a cancel writes nothing.
An expired lease does not return the run to `queued`: the row stays `running`, the job system
re-delivers the job, `attempt` counts it, and the next attempt resumes from the run's
checkpoint; only the last failed attempt marks the run `failed` (`source_error` for a read error,
else `job_failed`); the scheduler leader fails, with `job_failed`, a run whose job is dead or
gone. A lost lease also cannot end a run: the end-of-run writes lock the lease like a batch. An incremental stream also keeps `streams.checkpoint`, written in the same
transaction except for a batch that fails the run on its rejects. The reject check applies from
100 records read (`SYNC_REJECT_CHECK_MIN_RECORDS`) at 5%, stored per run. A source's cron is
mirrored to a `schedules` row (`schedules.source_id`, cascading); the leader's firing queues
one run per enabled, mapped stream without an active run, each with its own job
(`sync:run:<run_id>`) under the run's trace. The sweep is the last phase of the same `sync.run`
job for a full refresh and for a reconciliation (`sync_runs.run_kind`, `sync` or `reconcile`; the
payload, queueing, the one-active-run index, fencing and metrics are the same): after the stream
is exhausted and the last unit committed, and before `finish_run`, `store::sync_runs::sweep`
removes the live ledger rows of the stream whose `last_seen_run` is not the run, 200 per fenced
transaction (lease locked, run locked `running`), through `remove_contribution_in`, with a
`deleted` decision per removal. A record read but not applied (refused by the mapping or the
store, or only listed by a reconciliation) is marked seen, so it is never swept. Before the first
unit, one statement compares the unseen share of the live rows with `streams.sweep_limit`
(default 0.5) and a run over it ends `failed` with `sweep_limit` and removes nothing. A reconcile
run reads from the beginning, maps records only to split deleted from seen keys, and never writes
`streams.checkpoint`; its cron is a `schedules` row keyed by `stream_id`.

As built (M3, database sources): `postgres` and `mysql` are two modules and two registry lines in
`zirv-flow-connectors`, with the settings, TLS roots, keyset checkpoint, error mapping and metrics they
share in a private `sources::sql` helper (not a registry). A table or a configured `query:<name>`
is read in pages of 500 rows ordered by `(cursor, primary key)` or `(primary key)`, each page in its
own read-only transaction with a server-side statement timeout, strictly after the checkpoint
(`{"v":1,"key":[...]}`, the last row's key as the database printed it), so rows that share a cursor
value across a page boundary are neither skipped nor repeated. A stream needs a primary key and a
pageable key type, and an incremental stream does not read rows whose cursor is NULL. Every column
is read as text. Driver and server messages are dropped: failures are fixed messages, with the
SQLSTATE or error number logged and `zirv_flow_source_db_errors_total{source_type,reason}` counted.
The sources set `emits_deletes: false`; sweeping and delete signals for them come later.

**Mapping** is declarative: field path to identifier kind, trait, event field or
list, with a bounded transform set (trim, case, cast, timestamp parse,
concatenate, constant, value map, split) and nested JSON path selection for
field sources, the set already in `core::mapping`. There is no expression
language or scripting in v1 (OQ-5); a request the set cannot express becomes an
issue. The UI previews a mapping on sample records before saving.

As built (M3, HTTP manifest): the `http` source (`zirv-flow-connectors::sources::http`, one registry
line) runs any valid **manifest v1** on the connector runtime. The manifest is JSON stored as
`sources.config.manifest` (JSON because the config is `jsonb`, the editor edits text and the
assistant's typed tool schema is JSON; a YAML parser would be a second surface for an input an AI
drafts from untrusted documentation). It names the base URL, the auth type (never a secret: the
credential fields have fixed names), a rate limit, static headers and 1 to 20 streams, each with a
path, static query, records pointer, one of the runtime's pagination styles, typed fields, a primary
key, an optional incremental cursor, an optional delete signal and an optional mapping seed. There is
no expression language: unknown keys are refused, a closed list of script-like keys and any string
holding a template marker get their own code, and the only dynamic value in a request is the
incremental parameter. `zirv_flow_connectors::manifest::validate(&Value)` is the pure validator
(a hand-written walk that copies no value into an issue; `{code, path}` only, at most 50 issues,
limits of 64 KiB, 20 streams, 200 fields and 12 levels checked before the walk). The API's validate
endpoint, the editor, the source's own `read` and `check`, and the assistant tool (13.2) call it; it
depends on `core` only (5.1), and the spec-level `config_check` makes `validate_config` run it on
create and update. The checkpoint is `{"since": ...}` and moves only with the last batch of a stream
(to the highest cursor value seen), because an API may return records in any order; a lost lease
re-reads from the run's start, which the idempotent apply makes harmless (OQ-2 keeps ascending-order
resume open). A delete signal sets `Record.deleted` and nothing else: the batch path and the sweep
are unchanged. Discovery makes no request; a stream's mapping in the manifest is a seed written to
`streams.mapping` once, after the checks a saved mapping gets. `Source::rate_limit(config)` lets a
source type take its rate from its settings.

As built (M3, file sources): the `file` source (`zirv-flow-connectors::sources::file`, one registry
line) reads a CSV, NDJSON or top-level JSON array file at an https address through
`SourceContext::send` in byte-range windows of 4 MiB. A window begins at a record boundary and
is parsed by a fresh parser; the part after its last complete record is dropped and fetched
again with the next window, so a record (at most 1 MiB) always fits. The checkpoint is the byte
offset with the row count, the validator (`ETag`, else `Last-Modified`), the size and, for CSV,
the header and delimiter; a later window sends `If-Range`, and a changed validator or size
fails the read as `file_changed`. Only full refresh is offered. `core::mapping` gained the dotted
path (resolved by `JsonRecord::for_mapping`), one optional `event` rule per mapping and the
shared `map_json` the run and the preview both call. `commit_sync_batch` applies each record's
event through the event path of section 12.1 and its list memberships through `add_members`
(source `sync`) in the batch transaction, after the profile and before the checkpoint. The
event's `message_id` is `sync:` and the SHA-256 of source, stream, record key and event name, so
a re-read never repeats an event, and a mapping with an event needs a stream primary key.
`POST .../streams/{name}/preview` maps the first records of the source and writes nothing.

### 7.3 Identity resolution and precedence

Deterministic matching only, with rules per identifier kind:

| Rule | Meaning | Default |
|------|---------|---------|
| Normalization | email: trim + lowercase; phone: E.164; external ids exact, namespaced per source | as stated |
| `unique_per_person` | At most one value of this kind per person | on for external ids |
| `max_per_person` | Cap per kind, preventing chain merges through shared addresses | email 10, phone 10, anonymous id 100 |
| Merge policy | Whether a match may merge two existing people | allowed unless a uniqueness rule would break |

- **Blocked merge:** the record attaches to the person matched by the strongest identifier, and an **identity conflict** is recorded for review in the UI (shared family or role addresses are the common case). A review ends in one of two outcomes: keep the people separate, or move the identifier to one of them.
- **Merge:** the older person survives. One transaction moves identities, contributions, list memberships and consent, recomputes traits and sets `merged_into`. Consent takes the most recent explicit decision per channel/topic (`unsubscribed` wins ties). A `person.merge.rewrite` job then re-points events, enrollments and messages in batches (segment membership rows are closed instead, 6); meanwhile reads follow `merged_into` (one hop, chains compressed). Duplicate active enrollments in one flow keep the older and exit the other with reason `merged`. Each merge records its provenance (the moved ids in a merge record, plus a decision record). There is no unmerge in v1 (OQ-3).
- **Races:** concurrent creation of the same person is serialized by the unique identity index; the loser retries resolution.
- **Precedence:** each trait value is computed from all sources' contributions under the registry policy `latest` (by source observation time, default), `source_priority` (ordered sources) or `first`. Flow actions and the API write as sources (`flow:<id>`, `api`). The profile shows which source supplied each value.

As built (people, identities and the field registry):

- **Identifier kinds.** `email`, `phone`, `anonymous_id` and `external:<namespace>`
  (namespace `[a-z0-9_.-]{1,64}`). Email reuses the `Email` rules; phone is international
  format only (`+` and 8 to 15 digits after removing spaces, `-`, `.`, `(`, `)`, and a leading
  `00` read as `+`), with no national-format guessing; anonymous and external ids are exact,
  non-empty, at most 256 characters, with no control characters. Strength for an attach:
  external, then email, phone, anonymous id. An invalid identifier rejects the record with a
  typed reason; a record needs at least one valid identifier.
- **Inference.** An unregistered key is registered on first sight with `inferred = true`,
  `latest` precedence and `pii = false`: a string becomes `string`, a number `number`, a
  boolean `boolean`, an array of strings `string_list`. Timestamp and date are never
  inferred, and an object or a mixed array is dropped with a reason. Concurrent inference is
  `ON CONFLICT DO NOTHING` followed by a re-read. A type is fixed at creation.
- **Changing a rule.** Changing a trait's `precedence`, `source_order` or `archived` flag
  enqueues a `traits.recompute` job (payload `{workspace_id, key}`) in the same transaction
  as the registry change. The job recomputes that key for every person with a live
  contribution, 500 people per run and transaction (each run enqueues the next with an
  `after` person-id cursor), and appends a `trait.change` decision (ids only) and a dirty
  mark where a value changed. An archived key is not materialized: its values leave
  `people.traits` and `trait_sources` while the ledger keeps them, so unarchiving restores
  them. Retry: `StoreError::is_retryable()` is true for deadlocks, serialization failures and
  an identity race that outlasts three attempts; the caller retries the whole batch.

As built (merges and conflict review):

- **Rules.** A const table in `core::people` holds the defaults per kind, which are not
  configurable yet: `external:*` is `unique_per_person` with a maximum of 1, `email` and
  `phone` 10, `anonymous_id` 100. `merge_allowed` checks the union of the people's
  identities plus the record's new identifiers and answers `unique_kind` or `max_per_person`.
  The check is all or nothing for the people a record touches.
- **Resolution.** The people a record matches are locked `FOR UPDATE` in id order, so
  concurrent resolutions and merges cannot deadlock on each other; a person found already
  merged away re-runs the resolution (three attempts, then a retryable error). Allowed: the
  oldest person (`created_at`, then id) survives, and in the record's transaction each loser's
  identities and source records move, its traits are cleared, `merged_into` is set and chains
  are compressed, open conflicts among the merged people close as `merged`, a `person_merges`
  row, a `person.merge` decision and the `person.merge.rewrite` job are written, and the
  survivor's traits are recomputed. Blocked: the `identity_conflicts` row carries the reason
  and the shared `identity_id`, and an `identity.conflict` decision is appended.
- **Rewrite job.** `person.merge.rewrite` (payload `{workspace_id, merged_person_id}`)
  re-points `identity_conflicts.person_id` and `other_person_id` in batches of 1,000 rows per
  statement to the person the merged one now points at, then appends a
  `person.merge.rewrite` decision. A re-run moves nothing. Other person-scoped tables (events)
  join the target list in `store::people::merge` when they exist.
- **Review.** Conflicts are listed newest first by keyset (`status=open|resolved`).
  Resolving needs the admin role (or an `admin` key) and CSRF. `keep_separate` closes the
  conflict. `move_identifier` moves the conflict's identity to one of its two people unless
  the rules forbid it for that person (409), and recomputes nothing because identities carry
  no traits. Both are audited with ids only and appended as `identity.conflict.resolved`.
  Identifier values are returned to the owner, workspace admins and `admin` keys, and masked
  for members and `read` keys.

### 7.4 Deletion propagation

Deletion is in the pipeline from M2. A source-side delete tombstones the ledger
record, removes that record's identities, contributions and event rows, and
recomputes traits from what remains. If nothing remains (no contributions,
first-party events or consent), the workspace orphan policy applies (`keep`
default, or `delete`). The person is marked dirty, so memberships exit, flows
react as configured and destination syncs remove the person downstream (10.2).
Deletion removes one source's contribution; **erasure** (16.3) removes the
person and every derived row.

As built (M2): `remove_contribution` tombstones the record, recomputes the traits, deletes the
identities no other live record of the person lists (and the conflicts about them), marks the
person dirty, appends `contribution.removed` and then applies `workspaces.orphan_policy`
(`keep` or `delete`; orphan means no live record, no event and no explicit consent).
`store::privacy::delete_person` is the one function that owns the list of person-scoped tables;
it deletes the person and the people merged into them, and every milestone that adds a table
extends it. It is exposed as `DELETE /people/{id}`, as a step of erasure and as the orphan
policy's action, and it adds no suppression. As built (M3): `events.source_record_id` (nullable,
no foreign key, a partial index on the partitioned parent) names the ledger row an event came from,
and `remove_contribution_in` deletes those events, with their `event_dedupe` and `debugger_feed`
rows, in the removal's transaction, before the orphan check, so an event-only person left with
nothing follows the policy. A merge moves a record's person, not its link. No sync path writes
record-linked events until the events target of the mapping (#87); first-party events carry no
link and stay. As built (M3, file sources and deletion together): a sync batch marks its records
seen first, then hands each record's ledger id through `EventRecord.source_record_id` to the
event path, so the event a mapping yields is deleted with its record by a sweep, a delete signal, a
connector delete or a reconciliation. Lists only add: a removed record's list memberships stay.

### 7.5 Freshness and connector concerns

| Object | Freshness | Shown as |
|--------|-----------|----------|
| Pull stream | Time since last successful run; cursor high-water mark | fresh / stale (past threshold, default 2x schedule interval) / failing |
| Push source | Time since last record; inbox backlog age | same |
| Segment | `computed_at` plus the oldest input freshness of sources feeding its dependencies | "Data as of ..." on counts and membership |
| Send decision | Segment and trait freshness at decision time | recorded on the message |

Stale and failing sources are metrics and Sources-page badges (auto-pausing
sends on stale inputs is OQ-14). The connector runtime, not each connector,
owns (ADR-004): auth (API key, basic, bearer, OAuth2 client credentials and
authorization code with refresh), retries honouring `Retry-After`, rate limits,
pagination (cursor, offset, page, link header), checkpoints, the egress policy
(16.1) and delivery capabilities (idempotency keys, batching, erase).

As built (M3): `zirv_flow_connectors::runtime` implements auth, retries, the rate limit and the
pagination drivers once, behind `SourceContext::send` and `paginate`. `SourceContext` now carries
the source type (the metrics label), the source id (the rate-limit key) and a shared `RateLimits`
registry that `AppState` holds. The limit is one token bucket per source per worker process
(ruling 691b13cf), shared by sync runs, test connection and discovery; concurrent workers can
exceed it in aggregate, `Retry-After` handling mitigates but does not prevent that, and
distributed enforcement is revisited with OQ-9 at M6. Auth is attached only to the source's base
origin, and any other origin is refused before sending. A refreshed OAuth2 token is handed back
through the context's token slot (`take_refreshed_token`); `connectors` never writes to `store`
(5.1), so the caller stores it through `encrypted_secrets`; a client-credentials token is short-lived and is fetched anew
by the next call. Errors carry a variant, a status and an attempt count, never a URL, header or body.

As built (M3, HTTP manifest): the runtime now speaks the OAuth2 authorization-code grant. The
consent is `POST .../sources/{id}/oauth/start` (a signed-in admin) and `GET /api/v1/oauth/callback`:
the OIDC pattern again, with a hash-keyed single-use server-side state (`source_oauth_states`, ten
minutes) holding the PKCE verifier, the source and the user, and the existing session binding the
browser (the callback takes the state first, then requires the same signed-in admin). The code is
exchanged through the guarded client (`exchange_code`); a response without a refresh token is
refused. Both tokens are one `oauth_token` secret linked from `sources.oauth_secret_id`, apart from
the credential row so that a refreshing job and an admin replacing the credential never overwrite
each other. A refresh that rotates the refresh token is serialized across processes by the
`TokenStore` trait the runtime defines and `api` and `jobs` implement over
`store::sources::oauth::refresh_serialized`: one transaction locks the token row, reloads it, calls
the provider only if the stored refresh token is still the one the caller held, and saves the result.
A token is used only with the token URL, client id and scopes it was issued for.

As built (M3, freshness): the state is derived, never stored. `streams` gains `enabled_at`,
`last_success_at` and `last_run_failed`; the high-water mark is the existing `checkpoint`.
`core::freshness::freshness` is the pure function (a failed last run is failing; no threshold is
never stale; else stale once `now - max(last_success_at, enabled_at)` is past the threshold, and
equal is fresh), and `effective_threshold` gives the explicit value or twice the longest gap
among the next 8 slots of the source cron. `store::stream_freshness` joins `streams` with
`sources`, calls them and is the one read both the API and the leader use, so neither depends on
the other (5.1). `store::record_stream_outcome` is the only writer of the two columns and is the
shape every source type reports through: a pull run calls it in the transaction that ends the run
(finish, fail, the reject threshold, the orphan sweep; a cancel records nothing), a webhook
source calls it with the time of the last record received, and a stream consumer with success or
failure by its lag bound. Only the pull path exists as of M3. Only enabled streams of enabled
sources are reported. The high-water mark is shown to workspace admins only, since an incremental
cursor can be a record value.

### 7.6 Native contacts and streaming updates

Some companies have no external customer store. For them Zirv Flow is the
system of record for its **contacts** (customers), not only a copy of someone
else's data.

- **Contact definition.** Each workspace defines its contact shape on top of
  the field registry: typed fields, which fields are required, and which are
  identifiers (contact key, email, phone, external ids). Validation uses the
  same rules as event schemas (section 12.1). A contact is a person whose
  contributions come from the built-in `native` source, so identity resolution,
  precedence, consent, suppression, segments and erasure work unchanged.
- **Writes over HTTPS.** `/api/v1/workspaces/{ws}/contacts` offers create,
  read, list and search, full replace, partial update and delete by contact
  key, plus a bulk upsert. A single-contact write is **synchronous**: it
  validates against the definition and applies in the request, through the
  shared apply-batch store function in one transaction, and answers `201`,
  `200`, `204` or `422` with the stored contact. A bulk upsert is
  **asynchronous**: it is validated at the edge, appended to the inbox and
  answered `202`, and `ingest.drain` applies it like `identify`. Single writes
  are idempotent through an `Idempotency-Key` header, stored without field
  values; an identical repeat without the header is a no-op through the
  content hash. The UI edits contacts through the same API. A delete removes the
  `native` contributions under the orphan policy (7.4); erasure stays separate.
- **Writes over Kafka.** A Kafka source consumes one or more topics with
  `rskafka` (pure Rust, rustls TLS, SASL PLAIN and SCRAM) and without consumer
  groups. It runs as a long-running consumer job with a time budget that
  re-enqueues itself (14), not as a `sync.run` batch. The worker holding the
  stream's job lease discovers the topic's partitions and fetches each one from
  its next offset, stored in our database. Each message is mapped like any other
  stream to contact upserts, events or list membership; a tombstone deletes the
  contact. The per-partition next offsets are the stream checkpoint, written in
  the apply-batch transaction with the lease fenced inside it, so a crash
  replays from the last committed batch with no double application and a
  worker that lost its lease cannot advance an offset. A
  message that fails mapping or validation becomes an ingest reject and the
  partition moves on. Freshness is consumer lag: the partition's end offset
  minus the stored next offset. Broker credentials (SASL, TLS client
  certificates) use secrets encryption (16.2), are opened by `jobs`, and
  connections, including every broker address learned from metadata, pass the
  egress policy (16.1). Values are JSON only in v1; Avro and Protobuf with a
  schema registry come post-1.0 (OQ-31). The `rskafka` TLS and compression
  features are validated on both musl release architectures (amd64, arm64).

As built (M3, Kafka): `kafka` is one module and one registry line in `zirv-flow-connectors`; a stream
is a topic, in the new `stream` sync mode, and the record key is the message key (virtual field
`_key`, with `_timestamp`). `read` yields batches whose checkpoint is the typed `kafka::Positions`
(`next` and the high watermark per partition); the job stores them and `sync.run` never looks
inside. `stream_offsets` (migration 0027) holds `next_offset`, `high_watermark` and `observed_at`
per stream and partition; `commit_sync_batch` writes them after the lease fence and before the
counters, so a batch that is not applied moves none. A message with a null value is a tombstone
and deletes through `remove_contribution_in`; a malformed one is a reject whose reasons are the
place (`partition p offset o`) and a static code, so no payload byte is stored. The event
idempotency key hashes the log position (`Record.occurrence`) and not the record key, so a later
message with the same key is a new event while a replayed one is a duplicate; the consumer
commits a unit before a second message of a key it already holds, so no message replaces another
in a unit. One run is `running` across many `kafka.consume` slices of 60 seconds; each enqueues
its successor, under the unique key `kafka:consume:<run>:<generation>` (the running job holds its
own key), in the transaction that completes it, and a run older than a day rotates to a fresh one.
`fail_orphaned_runs` finds the job by either key form. After a slice, `report_stream_lag` stores
the watermarks and records `Succeeded` within `max_lag` and `Failed` above it; `lag` joins
`StreamFreshness` and the leader sets `zirv_flow_stream_lag_messages`. rskafka has no resolver hook,
so its only network path is a loopback SOCKS5 relay (`sources/kafka/relay.rs`) that resolves
through `SourceContext::resolve_checked`, dials the checked address and leaves TLS to verify the
name; the client is built with a retry deadline, and an empty fetch below the high watermark twice
in a row triggers a probe at `next + 1` so control batches and compacted batches do not stall a
partition. Its limits (aborted transactions are delivered, the SASL secret is copied inside the
client, the last release was in 2025) are in the README and the pull request.

As built (native contacts, issue 43): a contact is a live `source_records` row with source
`native`, stream `contacts` and `record_key` equal to the contact key, which is also the
identity `external:contact_key` (the keys `search` and `bulk` are reserved because they name
endpoints). `contact_definitions` holds one row per workspace, a list of `{key, required,
identifier}` over registry keys; `core::contacts` validates a write against it with the event
rules (`check_properties`), and the fields of an identifier field are kept as traits of the
native record as well as becoming identities, so a read returns exactly what was written.
A workspace without a saved definition needs only the key; with one, a contact holds only
its fields. `store::contacts` applies a single write through `apply_profile_record` in a
savepoint of the caller's transaction, so the API commits the data and its audit entry
together, and a registry rejection (a dropped trait) rolls the write back as a `422`. A
write takes two advisory locks, the idempotency key and then the contact, so concurrent
requests with either run in turn, and the drain takes the contact lock of every contact in its batch (sorted, after claiming the inbox rows and before the source records and people), so a bulk upsert and a single write of one contact do not overwrite each other. `contact_idempotency (workspace_id, key, request_hash,
status, created_at)` holds no field value, no person id and no contact key (which may be an
email address): a replay takes the key from its own request, whose hash covers the path and
body, and answers the stored status with the contact as it is now (`404` once it is gone, `204` for a delete),
a different request under the key is `422 idempotency_key_reused`, and the leader's
hourly upkeep deletes rows older than 24 hours. Because the table names no person,
`delete_person` and erasure do not touch it. The bulk path appends inbox kind
`contact` items (the migration extends the kind check) and the drain maps them to native
records that replace the stored fields, tallying them as `contact` in
`zirv_flow_ingest_applied_total`. The endpoints are in the README's Contacts section.

## 8. Segmentation engine

### 8.1 Definition AST

JSON with a schema version. "Nordic customers with two or more orders of at
least 50 in 90 days, not VIP, subscribed to the newsletter":

```json
{ "v": 1, "all": [
  { "trait": "country", "op": "in", "value": ["DK", "SE", "NO"] },
  { "event": "order_placed", "where": [{ "prop": "total", "op": "gte", "value": 50 }],
    "agg": { "fn": "count", "op": "gte", "value": 2 }, "window": { "last": "P90D" } },
  { "not": { "segment": "seg_0192f3c0" } },
  { "consent": { "channel": "email", "topic": "newsletter", "status": "subscribed" } } ] }
```

| Node | Semantics |
|------|-----------|
| `all`, `any`, `not` | Boolean composition |
| `trait` | Operators by registry type: equality, comparison, `in`, contains, exists, relative dates ("more than 7 days ago") |
| `event` | Property filters, then `count`/`sum`/`min`/`max`/`avg`/first or last time over a window (`last` duration, `between`, since a timestamp trait); "did not" and "never" |
| `list`, `segment` | Membership; segment references must stay acyclic |
| `consent`, `identifier` | Channel/topic state; presence of an identifier kind |

**Semantics.** Every leaf is two-valued: a missing trait, or a missing or null property, makes every operator false except `not_exists`, so `neq` and `not_in` on a missing value are false. `not` is plain boolean negation of its child, so "not (country eq DK)" includes people with no country while "country neq DK" does not. Over zero matching events `count` is 0, and `sum`, `min`, `max`, `avg` and first or last time are missing, so every comparison on them is false. Date values compare at UTC midnight; the workspace time zone is out of scope in v1, a recorded limitation (21.1). The compiled SQL reproduces this with `COALESCE(…, false)`, and the property tests enforce it (ruling 887b9eef).

**Numbers (ruling e6659f67).** Numbers compare as `f64` in the evaluator and as `float8` in SQL, for traits, properties and aggregates alike; integer properties take the same path. A stored number whose magnitude is past `1e300` counts as missing, so a cast never overflows and a `sum` over up to 10^8 values cannot. v1 limits, recorded here: integers above 2^53 lose precision, and equality on an aggregate can depend on summation order.

### 8.2 Compilation

`parse -> validate -> normalize -> lower`. Validation checks types, operators,
references and window limits against the field registry and event catalog.
Normalization flattens, pushes `not` inward through `all` and `any` (never rewriting a leaf operator under `not`) and folds constants. Lowering emits
SQL with an `EXISTS` subquery per event condition without an aggregate, and for an aggregate over a `since_trait` window, whose bound reads the person's own trait. Any other event aggregate is one grouped set over `events` (`GROUP BY person_id`): a semi-join (`p.id IN (... HAVING ...)`) when the condition is a positive conjunct (the root, or reached through `all` only) that an empty aggregate does not satisfy, judged by the evaluator's own comparison, and a `LEFT JOIN` of the grouped set otherwise (under `any` or `not`, or when zero events satisfy it). A batch query also restricts every grouped set to its batch. Every value is a bind
parameter, column and JSON keys come only from the registry, and relative
windows bind `$now` from the explicit `now` the caller passes, so output is deterministic. Result:
`CompiledSegment { sql, params, deps, time_sensitive }`, where `deps` lists the
traits, events, lists, segments and consent channels read.

**Clock.** Pure code takes an explicit `now`: the compiler, the reference evaluator and
refresh planning never read time. A `Clock` at the edges (worker and scheduler context, `api`
state) supplies it. Reads are async and fallible, and `core` gains no I/O. Production reads the
database clock; tests advance a manual clock. A refresh captures one timestamp and passes it
through every compile, plan and evaluation. In M4 only segment paths adopt the clock.

Verification: golden tests (AST fixture to expected SQL, reviewed on change);
an in-memory **reference evaluator** of the same AST; property tests running
random ASTs and data through both and comparing. The AST generators and the golden SQL
corpus live in `zirv-flow-segments`: `proptest` is a dev-dependency, the generators sit behind
a `test-support` feature, and golden files are blessed with `ZIRV_FLOW_BLESS`. The PostgreSQL
comparison tests live in `zirv-flow-jobs`; CI runs them with fixed seeds, and a scheduled
nightly workflow runs random seeds without filing issues (18). v1 evaluates event aggregates
on raw events with partition pruning and `(workspace_id, name, occurred_at)`
indexes; a per-person daily rollup is the first optimization if benchmarks miss
targets (OQ-2).

### 8.3 Preview, explain, membership

- **Preview:** count and sample under a `statement_timeout` fixed at 20 s (no setting; the timeout is an execution ceiling above the section 19 targets, which measure performance), always with data age. A read replica (`database.read_url`) is deferred: in M4 preview and explain use the primary under `statement_timeout`, and the replica comes only if the M4 benchmark (#127) shows preview load harming ingestion.
- **Planning:** the compiled preview, refresh and re-evaluation statements run unnamed (`persistent(false)`), so each execution is planned with its bound values and never switches to a generic plan. A trait slot that reaches `ready` is followed by an `ANALYZE` of its column, in a job that runs after the `ready` commit; a failure only logs.
- **Explain:** evaluates each AST node for one person and returns a pass/fail tree with observed values ("2 orders in window, needs 2 or more"); it answers "why is this person in or out" on the profile. The store loads one bounded **snapshot** of the person in one read-only, consistent transaction: traits; events, limited to the referenced names and the widest window and capped; lists, consent and identifier kinds; and current memberships. The reference evaluator (8.2) runs over the snapshot at the same evaluation time the SQL would use. Leaves are two-valued, so a node is unknown only when it hit a cap: it reports "unable to explain", never a false pass or fail, and unknown propagates unless the known children decide the outcome.
- **Membership refresh:** compute the new set, diff against current members, insert entries, close exits, write `segment.entered`/`segment.exited` events (properties: segment id, version, cause `data_change`, `time`, `definition_change`, `manual` or `initial`), and enqueue batched flow triggers, all in one transaction. The full refresh derives the cause and trace from state: a segment never computed is `initial` and one computed from an older version is `definition_change` (both on the trace of the request that saved the version); otherwise the trigger decides (`schedule` and `save` give `time`, `manual` gives `manual`, a referenced segment's change gives `data_change`), on the refresh job's own trace.

### 8.4 Recompute and versioning

| Mode | When | Scope |
|------|------|-------|
| Incremental | `segment.reevaluate` drains coalesced dirty marks (7.1) | Segments whose `deps` classes intersect the batch's combined change classes, plus their transitive segment dependents, with `person_id = ANY($batch)` |
| Scheduled full | Per-segment cadence, default hourly, minimum 5 min | Whole segment; **required** for `time_sensitive` segments, which change as time passes with no new data ("no order in 30 days") |
| Manual | "Refresh now" | Whole segment |

Segment-of-segment dependencies are evaluated in topological order; one refresh
per segment at a time (job unique key); the UI states the cadence ("re-evaluated
every hour"). Editing a definition creates a new version and applies the diff
with cause `definition_change`; events and decisions are recorded, but flow
segment-entry and segment-exit triggers ignore `definition_change` changes (OQ-6, 21.1).
A flow with a segment-entry trigger chooses "enroll current members" at publish (M5) when
current members should enroll rather than only new entries.

**Claim and pass.** `segment.reevaluate` claims a batch with
`DELETE ... WHERE (workspace_id, person_id) IN (SELECT ... FOR UPDATE SKIP LOCKED LIMIT n) RETURNING`,
so concurrent jobs take disjoint people. A mark made later re-inserts a row, and a failed pass
rolls the claim back. In that one transaction it evaluates every segment whose dependency
classes intersect the batch's combined classes, plus their transitive segment dependents, in
topological order, with `person_id = ANY(batch)`. Name-level filtering (event names, trait
keys) is an optimization, made only if the M4 benchmark misses.
**Safety net and pending segments.** The scheduler leader enqueues `segment.reevaluate` for any workspace whose oldest mark is more than 2 s old, so correctness never depends on a writer's enqueue, and it publishes the dirty backlog and age. A pass skips a segment whose `computed_version` is null or differs from `current_version`: its pending full refresh covers everyone.

## 9. Flow engine

### 9.1 Model

A **flow** has drafts and immutable published **versions**: directed acyclic
graphs of typed nodes with stable node ids. Publish validation rejects cycles,
unreachable nodes and untyped edges. Lifecycle: `draft -> published -> paused ->
archived`; enrollments stay on the version they entered, new ones use the latest
(migrating in-flight enrollments is OQ-7). Triggers: segment entered or exited
(optionally enrolling current members at publish); named event with property
filters; schedule (cron + time zone, audience = segment or everyone); manual
start for an audience; API (`POST .../flows/{id}/enrollments`).

| Node | Behaviour |
|------|-----------|
| `wait.duration`, `wait.until_time` | Resume after a duration, or at a local time/date (person `timezone` trait, else workspace zone) |
| `wait.until_event` | Resume on a matching event, or take the timeout edge |
| `branch` | Ordered segment-AST conditions evaluated for this person now, plus else |
| `split.random` | Percentage split by stable hash of `(enrollment_id, node_id)` (reproducible in simulation) |
| `action.send` | Message via destination + template, through the send pipeline (10.1) |
| `action.webhook`, `action.sync` | Signed HTTP call; upsert/remove the person in a destination |
| `action.update_trait`, `action.list` | Set/increment/unset a trait (as source `flow:<id>`); add/remove list membership |
| `goal`, `exit` | Record conversion (optionally exit); end the enrollment |

### 9.2 Execution model

```rust
pub fn step(
    graph: &FlowGraph,
    state: &EnrollmentState,
    input: StepInput,      // Start | TimerDue | EventArrived(..) | Resume
    ctx: &dyn StepContext, // injected Clock + ConditionEvaluator
) -> Result<Transition, EngineError>; // Transition { state, effects, wake, outcome }
```

One call advances through consecutive non-blocking nodes and stops at a wait,
an exit or the end of the graph. **Internal effects** (trait update, list,
goal) are applied in the same transaction as the state change, so later
branches see them. **External effects** (send, webhook, sync) are enqueued as
jobs in that transaction and the enrollment moves on; their outcomes return as
events (`message.sent`, `message.skipped`, ...) that later nodes can wait for.

### 9.3 Durability, idempotency, timers

- Enrollment state (node, context, `wake_at`, `state_version`) lives in Postgres. Each `flow.step` job carries the `state_version` it expects; stale jobs are no-ops. So when an awaited event races its timeout, the first commit wins.
- Step idempotency key: `(enrollment_id, node_id)`, sufficient because graphs are acyclic and re-entry creates a new enrollment. It is stored on `step_executions` and passed to effect jobs and destinations.
- A wait sets `wake_at` and enqueues `flow.step` with `run_at = wake_at`, unique key `wake:{enrollment_id}:{state_version}`. `wait.until_event` also writes `enrollment_waits(person_id, event_name, enrollment_id, timeout_at)`, which apply-batch checks on event insert.
- Failed steps retry with backoff; then the enrollment is `failed` with the error visible, and an operator can retry or skip.
- Enrollment states: `active`, `waiting` (timer or event), `completed` (exit node or end of graph), `exited` (goal exit, exit criteria, merge, flow archived), `failed` (back to `active` on operator retry or skip).

### 9.4 Enrollment rules and test mode

| Rule | Behaviour |
|------|-----------|
| Fan-out | `flow.enroll` jobs of up to 1,000 people; idempotent on `(flow_id, person_id, trigger_occurrence)` (event id or schedule slot) |
| Re-entry | `once`, `once_at_a_time` (after the previous ends), or `always` with a minimum interval; partial unique index on active enrollments + history check |
| Exit criteria | Leaves trigger segment, event occurs, or goal reached; checked from dirty marks and events for people with active enrollments |
| Pause | No new enrollments (skips are counted); due timers held and re-enqueued on resume |

The injected clock and evaluator let one engine run three ways. **Test mode**
(UI and API) walks a real or synthetic person through a version on a virtual
clock: conditions read the profile store, effects are captured not executed,
events can be injected to satisfy waits, and the UI shows the path.
**Simulation tests** run thousands of enrollments over virtual months with the
reference evaluator and injected crashes (section 18). **Production** uses wall
time, with due-time comparisons in SQL.

## 10. Channels, destinations and delivery tracking

### 10.1 Destinations and the send pipeline

A destination implements `Destination` and declares capabilities: channels
(email, webhook, profile sync), idempotency-key support, batching, rate limits,
erase. v1: SMTP email, email-provider HTTP APIs (built in or manifest), generic
webhook, profile sync via manifests. Every `action.send` passes these gates **at
send time** (consent can change while a person waits):

| # | Gate | On failure |
|---|------|------------|
| 1 | Enrollment active, flow not paused | dropped |
| 2 | Channel address present and valid (registry channel-address trait) | `skipped: no_address` |
| 3 | Effective consent under the workspace consent policy (below); the default is the topic's policy (explicit opt-in, or allowed unless opted out) | `skipped: no_consent` |
| 4 | Suppression check on the address hash | `skipped: suppressed` |
| 5 | Frequency cap per person, channel, window; check-and-record under a per-person transaction advisory lock | `skipped: frequency_cap` or deferred (configurable) |
| 6 | Quiet hours in the recipient's time zone | deferred to the next allowed time |
| 7 | Template render (section 11; sandboxed, strict undefined) | `failed: render_error` |
| 8 | Destination rate limit | deferred until a token is available |
| 9 | Deliver with idempotency key | `sent`, retried, or `failed` |

Each outcome updates `messages` (template version, destination, data
freshness, gate results) and appends a person event, so the profile answers
"why did this person not get the email". Zirv Flow enforces the consent policy
the operator configures; it does not decide which legal regime applies.

**Consent policies (M6).** What a person accepted is not always the whole answer.
A company may also have to weigh its own data, for example a cooldown after a
complaint, an account state such as self-excluded or closed, or a product the
person no longer holds. Consent is also more than a channel: some regimes split
it further, for example by product vertical. Zirv Flow therefore separates:

- **Consent dimensions.** Each workspace defines the dimensions consent is
  recorded on: the channel, plus its own dimensions such as topic, product
  vertical or purpose. A consent record is keyed by one value per dimension. A
  dimension can be left out, meaning "all values" (for example an unsubscribe
  from every vertical on email). Topics (M2) are the first such dimension.
- **Effective consent.** A workspace consent policy is built and versioned in
  the UI. Its rules are conditions over the recorded consent, traits, events,
  list membership and message history (cooldowns). They are expressed in the M4
  segment condition AST, with no scripting, and published like a segment. The
  send gate evaluates the policy for the message's channel and dimension values.
  The gate result records which rule decided, so "why was this not sent" names
  that rule, and a policy change appends a `definition_change` decision. The
  default policy is the topic policy above.

OQ-32 settles the dimension model and how the policy reuses the AST.

### 10.2 Email, tracking and feedback

- **Templates:** content model, builder, rendering, validation and preview are in section 11. Marketing email must contain an unsubscribe link (publish fails otherwise); messages carry `List-Unsubscribe` and `List-Unsubscribe-Post` (RFC 8058 one-click).
- **Categories:** `marketing` or `transactional`. Transactional skips the marketing-consent gate, never suppression; a dedicated transactional API is OQ-11. DKIM and signing domains are the relay's or provider's job (documented).
- **Tracking:** open pixel `/t/o/{token}`, click redirect `/t/c/{token}`, unsubscribe page and one-click endpoint `/u/{token}` on `server.public_url`. Tokens are HMAC-signed (message id, link index, no personal data); redirects only go to links recorded for that message.
- **Feedback:** provider webhooks map to `email.delivered`, `email.bounced` (`hard`/`soft`), `email.complained`; hard bounce and complaint add a suppression. SMTP-only bounce handling is OQ-12. Engagement events (`email.opened`, `email.clicked`, `email.unsubscribed`) are ordinary person events usable in segments and waits; opens are low-confidence because some clients prefetch images (OQ-10).
- **Webhooks and sync:** outbound webhooks are HMAC-signed with a timestamp and send `Idempotency-Key`, with timeout and retries. Keeping a segment in an external system is a flow (entry: `action.sync` upsert; exit: remove), not a second mechanism. Deletion and erasure call the destination's remove/erase capability where declared.

## 11. Templates and the builder

A template is Zirv Flow's own channel-aware content document, stored as
versioned JSON, edited in the React builder and rendered server-side in Rust per
channel (ADR-006). Preview and send call the same renderer
(`zirv-flow-templates`). v1.0 builds email and webhook content. SMS, push and
in-app content types are defined in the schema from day one, and their builders
ship with those channels after 1.0. M6 ships code templates (MiniJinja HTML and
text); M7 adds the block builder on the same renderer.

### 11.1 Content model

```json
{ "schema": 1, "channel": "email", "mode": "blocks",
  "email": { "subject": "{{ person.first_name | default('Hi') }}, your order shipped",
             "preheader": "Tracking inside", "sender": "snd_0192f4a1", "lang": "en",
             "body": [ { "type": "section", "columns": [[ { "type": "heading", "level": 1, "text": "On its way" } ]] },
                       { "type": "snippet", "ref": "snp_0192f4b7" } ] } }
```

| Channel | Content type | v1.0 editor |
|---------|--------------|-------------|
| `email` | Subject, preheader, sender identity, reply-to, `lang`, body as blocks or as code (HTML + text) | Block builder, code mode |
| `webhook` | Headers and a JSON body template; the destination supplies URL and signing | Payload editor (11.7) |
| `sms` | Text and sender id, with encoding and segment-length rules | Reserved: validated, not publishable |
| `push` | Title, body, image, deep link, data | Reserved |
| `in_app` | Title, body, image, call to action | Reserved |

- Every document carries an integer `schema`. The server reads older schemas through migrations (11.8) and refuses newer ones.
- `mode` is `blocks` or `code`. Code mode (raw HTML + text with MiniJinja) is the escape hatch. Conversion is one-way: blocks to code produces HTML and text with merge tags kept; code to blocks is never offered.
- Reserved channel types define typed fields and length rules now, so content and validation keep their shape when those builders ship.

### 11.2 Email block set v1

| Block | Content | Notes |
|-------|---------|-------|
| `section` | Row of 1 to 3 columns, each a list of blocks; background, padding | Columns stack on narrow screens; sections do not nest |
| `heading` | Level 1 to 3, text | Theme font and colour |
| `text` | Rich text: paragraphs, bold, italic, underline, links, lists, line breaks | Sanitized against an allowlist on save and on render |
| `image` | Asset, alt text, optional link, width | From the asset library (11.3) |
| `button` | Label, URL, alignment | Theme colours |
| `divider`, `spacer` | Style; height | |
| `raw_html` | The marketer's own HTML | Passed through as `mj-raw`; never rendered unsandboxed in the app (16.1) |
| `snippet` | Reference to a snippet | Resolved at render time to the snippet's current published version |

Any block may carry a visibility condition (11.4). Styling is limited to theme
tokens and a small set of spacing and alignment values. The set grows only
under the block-set growth policy (OQ-23).

### 11.3 Brand theme, snippets, assets

- **Brand theme:** one per workspace, versioned: colours (primary, text, background, link), font stacks (web-safe, optional web font URL), logo asset, default footer snippet (postal address, unsubscribe link). Blocks reference theme tokens, so a theme change restyles templates at their next render without republishing.
- **Snippets:** versioned block lists (legal footer, signature). Snippets cannot contain snippet references, so there are no cycles. Templates render a snippet's current published version; publishing a snippet lists the templates and active flows that use it.
- **Assets:** raster images only (PNG, JPEG, GIF, WebP; no SVG), with the type sniffed from the bytes rather than taken from the client, and per-file and per-workspace limits (OQ-21). The baseline (ADR-003) stores them in Postgres and serves them content-addressed from `server.public_url` under `/a/` with immutable cache headers and `nosniff`. An asset referenced by any published version cannot be deleted, because sent emails keep pointing at it. S3-compatible object storage is an optional later add-on; large sends should put a CDN or caching proxy in front of `/a/`.

### 11.4 Personalization and conditions

- **Merge tags:** a picker fed by the field registry (traits by type), the event catalog (trigger event properties), the brand theme and system values (unsubscribe and preference URLs) inserts MiniJinja expressions such as `{{ person.first_name | default("there") }}`. The sandbox is unchanged: no filesystem or network, fuel, recursion and output limits, strict undefined, HTML autoescape in HTML fields.
- **Visibility conditions:** a block may carry a segment-AST condition ("in segment VIP", "country is DK"). The renderer lists a template's conditions; the send or preview handler evaluates them for the person in one call through the existing `ConditionEvaluator` and passes the results in. Preview shows which blocks are hidden and why, using explain (8.3).

### 11.5 Rendering pipeline

1. Load the template version and the current published theme and snippet versions.
2. Migrate the document to the current schema in memory (11.8).
3. Resolve snippets, apply theme tokens, drop blocks whose condition is false.
4. Render every text field (subject, preheader, text, labels, URLs, alt text) through MiniJinja with the person and trigger context.
5. Emit MJML and render it to responsive HTML with the `mrml` crate.
6. Generate the text part from the block tree (headings, paragraphs, links as `label: url`), not by scraping the HTML.
7. Post-render: rewrite links for click tracking, bind the required `marketing` unsubscribe footer to the message's signed URLs, add the `List-Unsubscribe` headers. Preview runs this step with inert tokens.

Code mode runs steps 4 (on the whole HTML and text) and 7; webhook content
renders as in 11.7. Output is deterministic: no clock reads, random ids or
map-order dependence, so equal inputs give equal bytes. Golden tests and the
preview-equals-send test rely on this.

### 11.6 Validation

- **Blocks publish:** a merge field unknown to the registry or event catalog; MiniJinja syntax errors; a `marketing` email without an unsubscribe link (own or from the theme footer); a missing sender identity; a broken reference (snippet, asset, theme, segment in a condition); a document failing its schema or the sanitizer; raw HTML that `mrml` cannot parse; a webhook body that is not valid JSON after a sample render.
- **Warns:** images without alt text; HTML size near the clipping threshold of common mail clients (about 100 KB); missing `lang`; subject or preheader beyond typical display length; links not using `https`.

### 11.7 Versions, preview, test sends, webhook payloads

- **Versions:** drafts are editable; publishing creates an immutable version. `action.send` references the template, not a version, and sends its current published version at send time, so a typo fix needs no flow republish. Publishing shows the active flows that use the template. Version history and where-used are available for templates and snippets.
- **Preview:** rendered server-side through the same pipeline, as a real person (requires `view_pii`; until M8 introduces that grant, owners and admins only) or a sample person built from registry types, at desktop and mobile widths. The SPA shows it in a sandboxed iframe (no `allow-scripts`, no same-origin) with the text part, subject, preheader, hidden blocks and validation results.
- **Test sends:** a `template.test_send` job sends through the real destination, but only to workspace members' verified addresses or a workspace allowlist. Test sends are rate-limited, marked `test`, excluded from frequency caps and engagement stats, and kept in a test-send log under message retention.
- **Webhook payloads:** headers and a JSON body template. MiniJinja is allowed only inside string values, so the structure always stays valid JSON; a value that is exactly one merge tag may take the field's registry type (number, boolean). Sample-payload preview with a real or sample person; body size limit.

### 11.8 Document schema migrations

A migration is a pure function from schema `N` to `N + 1`, and reads chain them.
Published versions are stored as saved and never rewritten; drafts are upgraded
when saved. Each migration ships with golden tests: a schema-`N` fixture
migrates to the expected document and renders byte-identically. The builder
refuses a document whose schema is newer than the server's.

## 12. Event logging and traceability

Customer events describe what people did; the decision log describes what
Zirv Flow derived and why. Neither replaces security audit records or
operational logs. The four layers stay separate because they have different
writers, readers, retention and privacy constraints:

| Layer | Records | Authority and links |
|-------|---------|---------------------|
| Customer events | Typed business facts used by segments and flows | Event store; `event_id`, `message_id`, `trace_id`, source id |
| Audit records | Who changed configuration or accessed sensitive operations | Audit log; actor id, object id, `request_id`, `trace_id` where applicable |
| Decision log | What the system derived, attempted, skipped or rejected, and why | Append-only decision store; `trace_id`, typed cause and domain ids, never personal-data values |
| Operational logs and spans | Runtime diagnosis for requests, jobs, dependencies and failures | Structured `tracing` output; `request_id`, `trace_id`, job and domain ids; exportable through OTLP |

Durable ids, not copied payloads, link the layers. A request receives a
`request_id`; ingestion or a domain operation receives a `trace_id`; a customer
event receives an `event_id`; a client retry may supply `message_id`; a send
receives a message id. These ids cross every job payload and span.

### 12.1 Event schemas, validation and redaction

The event catalog is a versioned schema registry keyed by event name. Each
immutable version defines properties, types, required fields, enums,
descriptions and a PII flag per property. A registry entry selects one mode:

| Mode | Behaviour |
|------|-----------|
| `allow` | Accept and record validation results without flagging the event |
| `warn` | Accept, set `validation_state = flagged`, and expose every reason |
| `block` | Do not create the customer event; write an ingest reject with reason and a masked sample |

A workspace policy handles unplanned event names by creating a draft schema
from the observed shape or rejecting them. Schema inference never publishes a
schema. Before accepted properties are stored, ingest redaction drops or
keyed-hashes configured properties and truncates IP addresses; validation uses
the pre-redaction types but rejects and debugger samples apply the same masking
rules. Property values flagged as PII are masked without `view_pii`; until M8
adds that grant, only owners and admins can view them. Per-event-name retention
may shorten or extend the deployment default within operator-set bounds.

**As built.** The store ships the registry, validation and storage of this section, and 7.1
describes the ingestion API; the debugger, explorer and replay are described under 12.2 and 12.3. Versions are immutable
(a trigger refuses an update), an unplanned name drafts a schema only when it has no entry yet,
and a reject sample keeps a value only when the published schema marks the property as
non-PII and not an `object`. Only an accepted event claims its `message_id`, so a rejected
message can be resent; a second reject for the same open `message_id` is not written.
Events are partitioned by month on `occurred_at` like the decision log, with the leader
creating this and next month and the `events.retention` job dropping old ones. Merging people
does not yet rewrite events.

`segment.entered` and `segment.exited` are reserved system event names: a client cannot send
any `segment.` name, on any ingestion path. The store writes them with source `segments`.
Their properties are the segment id, version and cause, and their trace and cause are those of
the change that caused them.

As built (M3, sync events): an event a sync run yields goes through the same validation,
redaction and reject handling as one sent over HTTP. A refused event is an `ingest_rejects` row
of kind `event` that names the run, and the record counts once, as rejected, while its profile
change stands. Duplicates are counted by `zirv_flow_sync_events_total{outcome}`.

### 12.2 Delivery, time and replay semantics

- The ingestion API returns `202` only after the inbox append is durable. A
  client `messageId` has a unique dedupe record, so retries give
  effectively-once customer-event storage; duplicates are counted and linked
  to the original event rather than applied again.
- `occurred_at` and `received_at` are distinct. Timestamps beyond the configured
  future-skew limit are clamped to `received_at` and flagged. Events older than
  the workspace lateness window are accepted and marked late; affected
  time-window segments are recomputed for that person before downstream
  triggers are evaluated.
- An operator may replay rejects after publishing a schema or fixing a mapping,
  or re-drive a failed forwarding destination. A replay records its original
  id and a typed `manual` or `definition_change` cause. It never re-triggers
  flows unless the operator explicitly selects that separately previewed
  action; forwarding re-drives cannot trigger flows.

As built (reject replay): `POST /ingest-rejects/replay` (admin, CSRF) takes `ids` or a
`filter` (at most 10,000 rejects), a typed `cause` (`manual` or `definition_change`),
`emit_triggers` (default false) and `dry_run`. A dry run judges each reject's stored,
redacted payload against the current schema and policy and predicts `accept`, `flag`,
`still_rejected` (with reasons), `duplicate` or `already_replayed`, and names properties that
the policy hashes although the schema types them as something other than text
(`redacted_type_changed`); it writes nothing. A real run audits `ingest_rejects.replayed`
with counts and enqueues one `ingest.replay` job on a trace whose cause is the typed one.
The job replays each reject in its own transaction: it stores the event once under the
reject's original `message_id` and person (the payload keeps no identifiers, so a reject that
named no person is `still_rejected` as `no_person`), applies the redaction rules the payload
has not had yet (a reject records the rules it was stored under in `ingest_rejects.redaction`;
`drop` and `truncate_ip` are applied in full, `hash` only for paths not recorded, a legacy
reject gets the full current rules; the dry run reports `redaction_changed`), writes the
debugger feed row when the workspace is watched, sets `replayed_at` and `replay_event_id`, flags the event `replayed` and
appends an `event.replayed` decision (subjects `reject`, `event`, `person`). A second replay
finds `replayed_at` and does nothing; a `message_id` that is already stored resolves the
reject as a duplicate. With `emit_triggers` false the event is also flagged `no_triggers`.
**The M5 event-trigger path must skip an event flagged `no_triggers`**; no flow reads events
before M5, so nothing consumes the flag yet (`EventFlag::NoTriggers` documents the rule).

### 12.3 Debugger, explorer and person timeline

- **Live debugger:** an authenticated SSE stream scoped to one workspace and
  optionally one source. It shows accepted, flagged, rejected and duplicate
  events with validation results; filters by event name, source and result;
  samples apply PII masking and ingest redaction. Disconnecting a viewer never
  affects ingestion.
- **Event explorer:** filters by event name, typed properties, time and person;
  displays counts over time and property breakdowns; exports the filtered set
  as CSV. Interactive and export queries have separate statement timeouts and
  row limits, always use workspace and partition predicates, and report a
  timeout rather than running without a bound.
- **Person timeline:** interleaves customer events with relevant decision
  records, including merges, consent and trait changes, segment changes,
  enrollments, steps, sends and skips. Each system record links to its trace
  tree; values remain subject to the caller's PII access.

As built (people search, profile and person timeline):

- **The person key.** Every decision about a person carries `subject_ids.person`, the person
  whose timeline it belongs on. The merge decision (`person.merge`), the merge rewrite and the
  list-membership move add it next to `survivor` and `merged`; the trait, consent, identity,
  list and event decisions already had it. Migration `0012_people_timeline.sql` indexes
  `decision_records (workspace_id, (subject_ids ->> 'person'), recorded_at DESC)` for rows that
  have the key, on the partitioned parent so every partition gets it, and adds a GIN
  `jsonb_path_ops` index on `people.traits` for equality search until trait promotion (OQ-1, 21.1)
  replaces it. Decisions written before the migration are not backfilled.
- **Search** (`store::people::search_people`). An identifier search is an exact match on the
  normalized value that follows `merged_into` to the survivor. A trait search compares one
  registered field, with the operator checked against the field's type (`eq`, `exists`;
  `prefix` for `string`; `gt`, `gte`, `lt`, `lte` for `number`, `timestamp`, `date`), keyset
  paged by person id, and skips merged people. Values are bound, never formatted into SQL, and
  errors name the field and the rule, never the value.
- **Profile** (`store::people::load_profile`). Identities are grouped by the `source` of the
  live source records that list them; the rest are `observed`. Each field comes from
  `people.traits` and `trait_sources`, with the observation time of the latest record of the
  winning source that carries the value.
- **Timeline** (`store::people::load_timeline`). Two keyset streams, events by `occurred_at`
  (across partitions) and decisions by `recorded_at`, for the person and everyone merged into
  it, are merged newest first under the cursor `(at, kind, id)`; at equal times a decision
  sorts before an event, then ids descend. `event.accepted` is left out unless asked for.
  Each event carries the published schema version it was validated against, so the API can
  mask PII properties; an event with no schema is masked entirely for callers without value
  access.
- **Bounds.** Search, profile and timeline reads run in a transaction with
  `SET LOCAL statement_timeout = 5000`. PostgreSQL's cancellation (SQLSTATE 57014) becomes
  `StoreError::is_timeout`, which the API answers as `503` with the problem code
  `search_timeout` (the problem document gained an optional `code`).
- **API and UI.** `POST /people/search`, `GET /people/{id}` and `GET /people/{id}/timeline`
  need `read`; masking uses the same rule as identity conflicts and lists. The web UI's
  People page is the search, `people/:id` the profile with the consent panel (owned by the
  consent feature) and the lists panel (owned by the lists feature).

As built (debugger, explorer):

- **Feed.** Migration `0016_investigation.sql` adds `debugger_watch` and an UNLOGGED
  `debugger_feed`. A `serve` process with a viewer for a workspace upserts
  `debugger_watch.until = now() + 30 s` every 10 s and on connect. While `until` is in the
  future, `apply_event_record` writes one feed row per outcome (`accepted`, `flagged`,
  `unplanned`, `rejected`, `duplicate`) in the deciding transaction and the batch ends with
  one `pg_notify('zf_debugger', workspace_id)`; otherwise it writes nothing extra. Rows hold
  the redacted properties only, the reject code, the reasons and the published schema version
  for later masking. The leader deletes rows older than 15 minutes every minute. The feed
  does not survive a crash, which also starts `seq` over. `seq` is taken when a row is
  inserted, not when its transaction commits, so a row can become visible behind a newer one.
- **Stream.** `GET /debugger/stream` (`read`) uses one shared `PgListener` per `serve`
  process; a separate timer renews the watch, so a failing listener cannot let it expire, and
  while the listener is down the viewers poll every 5 s and send `event: status` with
  `degraded`. A notification wakes the viewers of that workspace. Each viewer keeps a low
  watermark (every `seq` up to it is settled) and the rows it sent above it, reads what it has
  not sent, and waits 10 s for a missing `seq` before giving up on it (its writer rolled
  back), so a late commit is never lost. A frame's `id` is the watermark after it, so
  `Last-Event-ID` resumes without skipping and may repeat rows (the `seq` in the data tells);
  a position beyond the feed is clamped to its end with a `gap` frame whose `reset` is true.
  At most 20 streams per workspace per process (`429` with `Retry-After`), and every stream
  ends when the server shuts down. Rows are masked per viewer with `mask_event` (the person timeline's rule, now in core; rejected events use the stricter
  sample rule), and sends `item` frames through a bounded channel. A full channel makes the
  viewer skip to the head and send `event: gap` with the number skipped. `Last-Event-ID`
  resumes as described; without it the stream starts at the newest row older than 10 s, so a batch still committing at that moment is not missed (live, plus those seconds). `reveal=true`
  shows stored values only to owners, admins and `admin` keys. Comments keep idle
  connections open every 15 s.
- **Explorer.** `POST /events/explore/{count,breakdown,list,export}` (`read`). Every query
  carries `workspace_id` and `[from, to)` (at most 400 days), so partitions are pruned, and
  runs in one `REPEATABLE READ READ ONLY` snapshot under `SET LOCAL statement_timeout`
  through the shared bounded-read helper, so a breakdown and its totals agree; a timeout
  is `503` with the code `query_timeout`. Filter operators follow the published schema type of
  the chosen name (undeclared properties compare as text); callers without value access may
  use only `eq` and `exists` on PII or undeclared properties and cannot break them down
  (`403 pii_property_not_explorable`). Count, breakdown, list and export share one `WHERE`.
  The export counts first (so a timeout is an error response and the audit entry exists),
  audits `events.exported` with operators and counts only, then streams keyset pages and
  ends with a `#` comment when the row limit cut it short. The `[explorer]` settings hold the
  timeouts and row limits.

### 12.4 End-to-end causality

Every derived action appends a typed decision record in the same transaction
as its domain state change. Covered actions are sync records applied, rejected
or deleted; identity merges; consent and trait changes; segment entry and exit;
enrollment and flow step outcomes; sends or skips with the gate reason;
delivery feedback; and forwarding attempts. Each record has `trace_id`, time,
action type, outcome, durable domain ids and one typed cause: `event`,
`trait_change`, `schedule`, `manual`, `api`, `definition_change` or `sync_run`, with the
cause id. It never stores identifier, trait or event-property values.
A sync writes one decision per record, appended with one multi-row insert per
batch in the apply-batch transaction.

As built (M3): the `sync_run` cause carries the run id, and the run's trace is
the trace of its jobs and of every decision it writes. An applied record is
`sync.record`/`applied` with the `person` and `source_record` ids; a rejected record is
`sync.record`/`rejected` with the reason code and the `ingest_reject` id, and also the `source_record` ledger row when it has one (#114; a record refused on first sight is named by its reject row alone); a deleted record has
the `contribution.removed` decision that retracting it writes. Unchanged records write none.
Both `cause_kind` CHECKs (`jobs`, and the partitioned `decision_records`) list the seventh kind.

The `trace_id` and cause cross transactional job enqueue, retries and external
effects. Retries add attempts under the same logical record rather than hiding
failures. `GET /api/v1/workspaces/{ws}/traces/{trace_id}` and the trace UI return
the resulting tree, including skipped work and exhausted retries:

```mermaid
flowchart LR
  event[customer event] --> segment[segment entered]
  segment --> enroll[flow enrolled]
  enroll --> step[step executed]
  step --> send[message sent]
  send --> feedback[delivery feedback]
  step --> skip[message skipped: gate reason]
```

Decision records are append-only and time-partitioned by record time. Their
retention and volume policy is OQ-28; erasure removes records scoped to the
person while preserving aggregate operational metrics.

As built (M1 skeleton):

- **Trace id and cause.** `TraceId` is a v4 UUID shown as 32 lowercase hex
  characters (the W3C trace-id shape, so OTLP can carry it later). `Cause` is one
  of the six kinds above plus an id, built only through typed constructors. The
  HTTP layer mints a fresh trace per request with cause `api(<request id>)` and
  ignores any incoming trace header, so a client cannot graft its requests onto
  another trace. The scheduler mints one per fired slot with cause
  `schedule(<schedule id>)`.
- **Jobs.** `jobs.trace_id`, `cause_kind` and `cause_id` are required; handlers read
  the trace from `JobContext::trace()` and pass it to follow-up jobs they enqueue.
- **`decision_records`.** Columns: `id`, `workspace_id`, `trace_id`, `recorded_at`,
  `action`, `outcome`, `reason`, `cause_kind`, `cause_id`, `subject_ids` (name to
  uuid, durable ids only), `job_id` and `attempt`. The table is partitioned by
  month on `recorded_at` with primary key `(recorded_at, id)`, an index on
  `(workspace_id, trace_id, recorded_at)` and a DEFAULT partition, so an insert
  never fails for a missing month. The scheduler leader creates this month's and
  next month's partitions on taking leadership and at most hourly; if the DEFAULT
  partition already holds rows for a month, that month's partition cannot be
  created, a warning is logged, and the rows stay readable. The store API takes
  static strings and ids only, and offers no update or delete.
- **`job.attempt` records.** For workspace jobs, a failed attempt appends
  `job.attempt` with outcome `failed` (a retry follows) or `exhausted` (dead) and
  the fixed failure reason, in the same statement as the fenced update. The lease
  reaper appends `lease_expired`, or `exhausted` when the expiry kills the job.
  Successful completion appends nothing; domain handlers record their own
  decisions through `JobContext::record_decision`, in their own transaction.
- **`GET /api/v1/workspaces/{ws}/traces/{trace_id}`** needs a workspace member or an
  API key with `read` scope. It returns `{trace_id, records, truncated}`, oldest
  first and at most 1000 records, each with `id`, `recorded_at`, `action`,
  `outcome`, `reason`, `cause {kind, id}`, `subject_ids`, `job_id` and `attempt`.
  A malformed id is 400 and a trace with no records in the workspace is 404,
  never saying whether it exists elsewhere.

### 12.5 Event forwarding

M6 adds event-forwarding destinations after core tracking and causality are in
place. A destination selects accepted events with typed filters and maps them
to its payload. Delivery is at-least-once with a stable idempotency key derived
from event and destination; retries with backoff, attempts and dead letters
are visible, re-drivable and represented in the decision log. Metrics expose
per-destination queue depth, oldest-item lag, attempts and outcomes. Erasure
removes pending and dead forwarding payloads and calls destination erase where
the destination declares that capability.

## 13. LLM assistance

Assistance is optional and operator-configured. It turns natural language into
reviewable drafts of Zirv Flow's own typed artifacts; it is not a second
authoring model and has no privileged path around validation (ADR-008).

### 13.1 Providers and configuration

`zirv-flow-ai` defines a Rust provider trait with OpenAI-compatible and
Anthropic API adapters. The compatible adapter accepts a base URL, so local
model servers use the same path. Deployment configuration allowlists providers,
models and base URLs. Per-workspace settings choose enablement, provider,
model, monthly budget and whether personal data may be sent. Provider keys use
envelope encryption (16.2); `api` or `jobs` opens the key right before a call and
passes it to `ai`, which never decrypts. Every call uses the egress policy
(16.1); a local server on a private network still needs an explicit allowlist
entry.

Minimum model capabilities (OQ-27, decided): structured tool calls with a JSON
Schema subset (object, string, number, integer, boolean, array, enum,
required), token usage reporting, and a context window of at least 32k tokens.
Streaming is optional. A capability probe on save checks tool calls and usage;
the window comes from provider metadata or operator configuration. Enabling a
model is refused while a requirement is missing. Provider-specific responses
are reduced to one internal stream of text, usage and structured tool calls, so
product code does not branch on provider.

As built (M3, AI provider layer): `zirv-flow-ai` depends on `core` only. Its `Provider` trait is
`complete(http, endpoint, key, request) -> ChatResponse`, and a response is reduced to `Text` and
`ToolCall` parts plus `Usage`. An answer from an OpenAI-compatible server without a `usage` object,
or a tool call whose arguments are not JSON, is a `Capability` error; the probe turns these into
missing capabilities. `AiHttp` is the crate's one HTTP client: a resolver hook calls
`EgressPolicy::decide_all` and hands reqwest only the checked addresses, IP literals are decided
before sending, redirects are never followed, environment proxies are ignored, the total timeout is
60 s and a response body is capped at 4 MiB; a source-scan test allows exactly one client builder.
It duplicates the small guard of `zirv-flow-connectors` because 5.1 forbids the edge; both delegate
every decision to `core::egress`. Tool parameter schemas are a validated subset (`JsonSchema`), so no
provider sees a keyword it may reject. Generating tool schemas from the API types is #93's.

The deployment allowlist is `[ai]` in the configuration (`ai.providers` with models, context
windows and prices in micro-units per million tokens, `ai.rate_limit_per_user_per_minute`,
`ai.max_repair_attempts`), validated at startup and by `config check`. Workspace settings are
`ai_settings`; the provider key is an `encrypted_secrets` row of purpose `ai_provider_key`, written
in the same transaction as the settings. `PUT ai/settings` runs the probe before it stores anything,
checks the configured window against 32,768 tokens without calling the model, and stores only a
passing probe. Streaming is not built; #93 adds it if its UI needs it.

### 13.2 Typed tools and the draft loop

Tool JSON schemas are generated from the same Rust types that produce the API
schema. The registry contains workspace-scoped reads and draft creation only:

| Artifact | Assistant work | Deterministic authority and review |
|----------|----------------|------------------------------------|
| Segment AST | Draft or explain a segment | Segment parser/compiler; diff plus preview count and sample |
| Flow graph | Draft or explain a flow | Flow publish validation; graph diff plus simulation summary |
| Template document | Draft copy, subject variants, layout or translation | Template validation; rendered diff at desktop/mobile widths |
| Source field mapping | Draft mappings from schema and samples | Mapping schema and sample dry run; field-by-field diff |
| HTTP connector manifest | Draft a manifest from pasted API documentation | Manifest schema and mock/test-connection preview |
| Trace | Explain a trace tree in plain language | Read-only trace API; cited decision-record ids |

Model output is parsed as a tool call and passed to the existing validator.
Validation errors may be returned to the model for a configured bounded number
of repair attempts; exhaustion returns the errors to the user. The UI shows
the validated diff and applicable counts, renders or dry-run results. Accepting
copies it into an ordinary product draft; publishing remains a separate human
action under its existing permission.

As built (M3, #93): `zirv-flow-ai` defines the `AiTool` trait (`ToolEffect` has two variants,
`Read` and `Draft`, so a publishing, sending, deleting or consent tool cannot be added without
touching every exhaustive match) and the async three-way `repair` (`Verdict`: valid, invalid
with typed issues, or abort). `api::assistant::tools` holds the registry, one module and one line
per tool: `read_source`, `draft_mapping` and `draft_manifest`. Their parameter schemas are lowered
from the API types (`MappingBody`, the manifest validate request) into the provider subset by
`assistant::schema`. `POST .../ai/drafts` runs one synchronous draft: the server reads the context
through `read_source`, offers the model only the kind's draft tool, checks each attempt with the
validator a save runs, repairs at most `ai.max_repair_attempts` times, and answers with the draft,
its field-by-field diff against the saved artifact and a dry run (the mapping preview, or the
manifest sample). Only a validated draft keeps content, in `ai_drafts` (seven days; an invalid
run stores a row with no content). `POST .../ai/drafts/{id}/accept` re-checks the stored draft,
audits `ai.draft.accepted` and returns it; the editor holds it as its unsaved draft, and saving
stays `PUT .../streams/{name}` or `PATCH .../sources/{id}`.

As built (M4, #126): the registry gains `draft_segment` (draft) and `explain_segment` (read), five
tools in all. `draft_segment` takes a whole definition and runs `parse_and_validate`, the
self-reference check and `compile` (which normalizes) against the catalog read for the call, and
returns the definition or `code at pointer` lines. `explain_segment` is checked against the saved
version's definition: every pointer a node, every condition cited once, plain text with a length
cap; its output goes to the caller and is not stored (a read tool that returns the checked
explanation as `ToolOutput::Draft`, since it is not request content). `ai_drafts` gained the
`segment` kind and a nullable `segment_id`, and the source and stream rules were split per kind. The
context is built from `segment_catalog_details` (definitions only, no value of any person). In
the saved definition each string literal compared with a trait or property not declared
non-personal becomes a placeholder before it is sent and is restored in the draft, only in the
node it was sent in. Accept takes the segments lock of a save, compares the digest of the current
version and re-checks the definition, and writes no version. The golden corpus has `segment` and
`explain` kinds.

### 13.3 Authorization and untrusted input

The server authenticates every tool call as the requesting user, rechecks its
RBAC permission and workspace, and rejects ids from another workspace. No tool
can publish, send, delete, mutate consent or invoke an arbitrary endpoint; the
absence of those tools is enforced by a registry test. Structurally valid model
output remains untrusted and receives semantic validation and normal limits.

System instructions identify event properties, template text, source samples
and pasted API documentation as data, never instructions. Tool arguments may
only select registered fields and typed ids; retrieved content cannot add tools
or loosen policy. Prompt-injection tests cover indirect instructions in each
context source.

As built (M3, #93): both routes need an admin and a signed-in user (an API key is 403
`ai_user_required`), and every tool call authorizes that user again, so a user demoted mid-run is
refused at the next call. The draft tools take only the artifact: the target comes from the
authorized request, and a list id or source of another workspace is refused. A field name a source
reports, the keys of a pasted webhook sample and pasted documentation are `Untrusted`; record
values are `Personal`, now fenced like `Untrusted` and bringing the same notice. The keys of a
webhook sample that parse as an email or phone are left out of the context.

### 13.4 Context, audit and limits

Schema-only context is the default: field and event names, types, descriptions
and aggregate counts, without identifier or property values. Sending personal
data requires per-workspace opt-in and may be restricted to configured local
providers. Without opt-in, request construction rejects identifier values even
if a caller attempts to supply them.

Every call records workspace, user, feature, provider, model, input/output
tokens, estimated cost, latency, outcome and resulting draft id. Assistant
transcripts are not stored by default (OQ-26). A workspace admin can opt in to
a debugging window of 1 to 30 days (default 7), a retention job purges it, and
changing the setting is audited. The content-free call audit log is always
kept. Per-workspace budgets stop
new calls before dispatch with a clear error, and per-user rate limits bound
abuse and accidental loops. Audit records never contain prompt or response
content.

As built (M3, AI provider layer): `api::assistant::dispatch` is the only way product code calls a
model. In order it loads the settings (refusing with `Disabled` when the workspace is off, has no
key or its model left the allowlist), refuses over the per-user per-minute limit (429), refuses when
the month's spend has reached the budget (402; a budget of 0 allows no calls), builds the request
under the workspace's `Privacy`, opens the key, reserves the call, calls the provider, then finishes
the `ai_calls` row and, if the workspace opted in, adds an `ai_transcripts` row. A refusal at a gate
sends and records nothing. Spend and rate are read from `ai_calls` (the sum of
`estimated_cost_micros` since the first of the month in UTC; the calls of the last minute by the
database clock), so they hold across instances. The reservation is one transaction under an advisory
lock on the workspace: it counts every row, `pending` ones included, and inserts a `pending` row
whose cost is the worst case (one input token per byte of the request text plus framing, the full
output cap), and admits the call only when the month's spend plus that cost stays within the budget,
so concurrent calls cannot all pass the gates and real spend never exceeds the budget. The call then
runs in a task of its own, so a dropped handler cannot leave the row pending, and the row is
finished with the real tokens and cost. A panic marks it `abandoned`; a process that dies mid-call
leaves it `pending`, and the next reservation of the workspace marks `pending` rows older than the
client timeout plus a margin `abandoned`. Both keep the worst-case cost, which is conservative and
intended. Personal data is a
distinct request content type (`Personal`), and a request builder at `SchemaOnly` privacy returns
`PersonalDataNotAllowed` for it, so the check does not depend on the caller. Untrusted content is
fenced in `<untrusted_data>` delimiters, with a system notice added whenever it appears. Bounded
repair sends each attempt through the same gates. `ai_calls` has no column that can hold content
(a test lists its columns); `ai_transcripts` carries `expires_at`, and the
`maintenance.ai_transcripts` job, scheduled hourly by the migration, deletes expired rows in batches.
Erasing a person deletes the workspace's transcripts whose text contains their ids or identifier
values, ignoring case, in the erase transaction.

As built (M3, #93): the two dispatch residuals are closed. The reservation now runs in the same
spawned task as the provider call, so a dropped handler cannot leave a reserved row without a
flight, and a provider panic is caught inside that task and finishes the row `abandoned` at its
worst-case cost. `Dispatcher::for_draft` sets `ai_calls.draft_id`; the features are `mapping`,
`manifest` and `evaluation`.

### 13.5 Evaluation and later MCP exposure

A provider-independent evaluation harness holds golden natural-language prompts
and expected artifact properties. Operators run it manually or nightly against
configured providers; scores and model metadata are recorded but do not gate a
release. CI uses a deterministic fake provider that replays recorded tool calls
to test validation, repair, privacy, budget and authorization paths. CI never
calls an LLM.

After 1.0, an MCP server may expose the same read-and-draft tool registry. It
uses the same workspace scoping, RBAC, audit and draft-only constraints; its
authentication model is OQ-30.

As built (M3, #93): the corpus is `api/src/assistant/golden.json`, compiled in. Each case holds a
prompt, optional documentation, fixture fields and traits, expected properties (JSON pointer
checks `equals`, `contains` and `absent`, with `*` for any element) and a replay of the tool-call
arguments a good model gave. CI runs every case on the fake provider through the same prompt
builder, repair loop and pure validators, with no database. `zirv-flow admin evaluate-assistant
--workspace <slug> [--case <name>]` runs it against the workspace's model through the gated
dispatcher (feature `evaluation`; budget and rate limit apply) and prints one JSON line per case.
It exits 0 whatever the score. `ai/evaluations` (stored scores, an API) is not built.

## 14. Queue and scheduler

`jobs(id, workspace_id, kind, payload, priority, run_at, state, attempts,
max_attempts, lease_token, lease_expires_at, locked_by, unique_key, last_error)`
has a partial index `(priority, run_at) WHERE state = 'ready'` and a partial
unique index on `unique_key WHERE state IN ('ready','running')`. `schedules(kind,
cron, timezone, next_run_at, payload)` holds recurring work. Kinds: `sync.run`,
`kafka.consume`, `ingest.drain`, `ingest.replay`, `event.forward`, `person.merge.rewrite`, `segment.reevaluate`, `segment.refresh`,
`flow.enroll`, `flow.step`, `delivery.send`, `delivery.sync`, `webhook.call`,
`template.test_send`, `privacy.export`, `privacy.erase`, `maintenance.*`.

`kafka.consume` is a long-running consumer job per Kafka stream with a time
budget: it heartbeats its lease, applies batches that each commit the partition
offsets with the lease fenced inside the transaction, and re-enqueues itself
when the budget ends. A Kafka stream is therefore not a `sync.run` batch, and a
lost lease moves the stream to another worker without a skipped or repeated
offset (7.6).

As built (M3, Kafka): a slice's successor has the unique key `kafka:consume:<run>:<generation + 1>`,
since the running job still holds its own key and the unique index covers running rows; it is
enqueued through the handler's transaction, so the completion fence makes it exactly once.
```sql
-- Claim: short transaction, committed before the handler runs.
UPDATE jobs SET state = 'running', attempts = attempts + 1, locked_by = $worker,
       lease_token = gen_random_uuid(), lease_expires_at = now() + $lease
WHERE id IN (SELECT id FROM jobs
             WHERE state = 'ready' AND run_at <= now() AND kind = ANY($kinds)
             ORDER BY priority, run_at LIMIT $batch
             FOR UPDATE SKIP LOCKED)
RETURNING id, kind, payload, attempts, lease_token;
-- Complete: fenced by the lease token; a worker whose lease expired cannot complete.
DELETE FROM jobs WHERE id = $id AND lease_token = $token;
```

**Completion.** The runtime opens the handler's transaction and first takes a
key-share lock on the job row, `SELECT 1 FROM jobs WHERE id = $1 AND
lease_token = $2 FOR KEY SHARE`; no row means the lease was lost and the
transaction is abandoned. A key-share lock, not `FOR UPDATE`, because the
heartbeat's `UPDATE` runs on another connection while the handler works and must
not wait for the handler to finish. The handler writes and enqueues through that
transaction. When it returns, the runtime runs the fenced `DELETE FROM jobs
WHERE id = $1 AND lease_token = $2` in the same transaction and commits only if
a row was deleted; otherwise the lease was lost meanwhile and the whole
transaction, handler writes included, rolls back. Of two workers that ran the
same job, exactly one deletes it, so a job never completes twice. Failure, retry,
heartbeat and dead are fenced on the token the same way. `last_error` is cut to
2 KB and never holds payload values; handlers return a typed error with a fixed
message.

**Scheduler slots.** A slot is enqueued only by `UPDATE schedules SET
next_run_at = $next WHERE id = $id AND next_run_at = $slot`, a compare-and-set
in the same transaction as the enqueue, with unique key `schedule:<id>:<slot>`.
Advancing `next_run_at` is the durable record that a slot fired, so a failover
or a two-leader overlap fires no slot twice, even after the first slot's job
completed and was deleted. A schedule far behind enqueues only its latest due
slot and logs how many it skipped. A DST gap runs a fixed wall-clock time at the
first instant after the gap, and an overlap runs it once.

**Fairness (OQ-8, decided).** One shared queue with a per-workspace running cap:
the claim query skips a workspace that already has `worker.max_running_per_workspace`
jobs running (default unlimited; instance-level jobs are never capped). While a
cap is set, claims take a transaction-level advisory lock so concurrent workers
cannot overshoot it. Weighted claiming is rejected for v1: it is more complex
and nothing yet shows the cap is not enough. M8 validates the cap under load.

| Concern | Design |
|---------|--------|
| Execution | Outside any claim transaction. The handler's writes and the jobs it enqueues commit in its own transaction, which also checks the lease token (see Completion below). Long handlers heartbeat every third of the lease (default 30 s, `worker.lease_seconds`); a heartbeat that finds the lease gone cancels the handler and discards its transaction. |
| Retries | `run_at = now() + min(base * 2^attempts, cap) * jitter`; after `max_attempts` the job is `dead`, visible and retryable on the Jobs admin page. |
| Idempotency | Database effects are idempotent by construction (upserts, unique keys, `state_version`). External effects carry stable keys: effectively-once where the destination honours them; where it cannot (SMTP), a crash between provider acceptance and recording can duplicate a send. Documented, not hidden. |
| Scheduler | Leader enqueues due schedules with unique key `(schedule_id, slot)`, reaps expired leases, pre-creates event partitions and watches the default partition, enforces retention, refreshes the stream freshness gauges every 30 s (stale-source detection: the state is derived on each pass, nothing is marked), triggers scheduled segment refreshes. |
| Hygiene | Completed jobs are deleted at once (history only for failures/dead); aggressive autovacuum on `jobs`; applied inbox rows deleted; dead-tuple counts exported as metrics. |
| Rate limits | Per-destination token bucket in a row; workers take tokens in blocks with one `UPDATE ... RETURNING`; `429` shrinks the rate and honours `Retry-After`. Cross-workspace fairness is a per-workspace running cap (OQ-8, below). |

## 15. API and UI

REST under `/api/v1`, JSON, OpenAPI 3.1 (utoipa, committed). Workspace resources
live under `/api/v1/workspaces/{ws}/...`. Cursor pagination,
`application/problem+json` errors (RFC 9457), `Idempotency-Key` on creating
POSTs. After 1.0, `/api/v1` changes only additively.

**Authentication:** local accounts (argon2id), server-side sessions in
HttpOnly SameSite cookies with CSRF tokens on mutations; API keys random, stored
as SHA-256 hashes, workspace-scoped, with scopes `ingest` (write-only), `read`,
`write`, `admin`. OIDC SSO works with any compliant provider, with Keycloak as
the reference: the authorization-code flow with PKCE, a state cookie bound to a
single-use server-side login, and ID-token verification in `zirv-flow-identity`.
An identity links to a user by issuer and subject, or by a verified e-mail
address; provider roles are not mapped, so an admin grants workspace access.
Local sign-in can be turned off. **First-run setup:** while no owner exists,
`serve` mints a one-time setup token (kept as a SHA-256 in memory, logged once)
and `GET /api/v1/setup` reports `{required}`; `POST /api/v1/setup` with
`{token, email, password}` creates the owner through the shared audited admin
operation and signs them in like `POST /api/v1/session` (403 on a wrong token,
409 when an owner exists). Each `serve` process has its own token. **Jobs
administration (instance owner session only; API keys get 403):**
`GET /api/v1/admin/jobs` (ready, running and dead counts per kind and the age of
the oldest runnable job), `GET /api/v1/admin/jobs/dead` (dead letters newest
first, keyset-paginated, with trace ids and no payload) and
`POST /api/v1/admin/jobs/{id}/retry` (requeues a dead job under its original
trace; audited as `job.retried`, plus a `job.retry` decision for workspace
jobs). **Public endpoints:**
`/api/v1/ingest/{identify,track,batch}` (ingestion key implies workspace; size
and per-key rate limits; schema validation; `202` after durable append),
`/hooks/{id}` (HMAC signature), `/t/*` and `/u/*` (signed tokens), `/a/*`
(content-addressed image assets, read-only).

The SPA (TypeScript, React, Vite, React Flow) is served from `/` (history
fallback, hashed assets, strict CSP). Every destructive or bulk action previews
its impact first, such as how many people would enter a flow now.

| Area | API resources | UI |
|------|---------------|----|
| Sources | sources, streams, sync-runs, rejects, imports | Freshness badges, setup wizard (test connection, discover, mapping preview), run history, rejects |
| Events and traces | event-schemas, ingest-rejects, reject-replays, debugger stream, event-explorer, event-exports, traces, forwarding-destinations and attempts | Schema registry and validation modes; live debugger; explorer with charts, breakdowns and CSV; reject replay; trace tree; forwarding lag and dead letters |
| People | people, timeline, identity-conflicts, traits (registry), lists, consent, topics, suppressions | Search; profile with identities by source, traits with provenance, unified event and decision timeline, consent, lists, segments with explain, messages with gate outcomes; conflict review |
| Segments | segments, versions, preview, members, people/{id}/explain | Builder over registry and event catalog, live count with data age, sample, membership history |
| Flows | flows, versions, publish, enrollments, simulate, stats | Canvas editor with inline validation, publish with impact preview, test mode, enrollment inspector, per-node stats |
| Delivery | destinations, messages, sender identities | Destinations, topic consent policy, frequency caps, quiet hours |
| Templates | templates, versions, publish, where-used, snippets, brand theme, assets, preview, test-sends | Email block builder with code mode, merge-tag picker, visibility conditions, desktop/mobile preview, test sends; webhook payload editor; snippets, theme, asset library; version history and where-used |
| Assistant | ai/settings, ai/models, ai/drafts, ai/evaluations, ai/usage | Provider/model and privacy settings; authoring panel with validated diff and previews; usage, budget and call audit |
| Settings | workspaces, members, users, api-keys, audit, privacy/exports, privacy/erasures, jobs, health | Workspaces, members and roles, API keys, audit log, retention, privacy requests, jobs admin |

As built (M3, #93): `ai/drafts` is `POST /api/v1/workspaces/{ws}/ai/drafts` (run) and
`POST .../ai/drafts/{id}/accept`; there is no list or read route for drafts. The assistant panel is
shown in the Map dialog and the manifest editor.

## 16. Security, privacy and compliance

### 16.1 Trust boundaries

| Boundary | Threat | Control |
|----------|--------|---------|
| Browser to `serve` | Session theft, CSRF, XSS | HttpOnly SameSite cookies, CSRF tokens, CSP without inline scripts |
| Ingestion, webhooks | Floods, forged data | Write-only keys or signatures, size and rate limits, schema validation |
| Tracking, unsubscribe | Enumeration, forged unsubscribes, open redirect | HMAC tokens, no personal data in URLs, recorded-link allowlist |
| Outbound HTTP | SSRF into internal networks or cloud metadata | Egress policy checked after DNS resolution and bound to the connection: deny loopback, private, link-local, metadata ranges unless allowlisted |
| Templates | Injection, resource exhaustion | MiniJinja sandbox, fuel and output limits |
| Rich text in blocks | Script or markup injection into emails and the builder | Allowlist sanitizer (tags, attributes, URL schemes) on save and on render |
| Preview of marketer HTML | Raw HTML block or code mode running script in the app | Shown only in a sandboxed iframe without `allow-scripts` or same-origin, with its own CSP; never rendered unsandboxed in the app. It is the marketer's own content, so it is not sanitized for the email itself. |
| Asset uploads | Disguised files, SVG script, storage exhaustion | Type sniffed from the bytes, raster allowlist (no SVG), size and quota limits, `nosniff`, served from `/a/` only |
| Test sends | Use as an open relay | Recipients limited to members' verified addresses or a workspace allowlist; rate-limited |
| Segment builder | SQL injection | AST only, bind parameters, registry-only identifiers, no raw SQL in v1 |
| LLM provider egress | Personal data disclosure, SSRF, unapproved model | Provider/model/base-URL allowlist, common egress policy after DNS resolution, schema-only context by default, workspace PII opt-in |
| Assistant context and output | Prompt injection in stored or pasted data; structurally valid unsafe output | Treat context as data, fixed read/draft tool registry, per-call RBAC and workspace checks, deterministic semantic validators, bounded repair |
| Stored credentials | Database dump exposes secrets | Envelope encryption (16.2) |
| Database sources | Over-privileged credentials | Documented read-only roles; connectors open read-only transactions |
| Kafka brokers | SSRF through broker metadata | Bootstrap and every broker address from metadata pass the egress policy; connect by the checked IP |
| Logs | Personal data leakage | Log person ids, not identifier values; secrets redacted |

**Egress policy.** The pure policy lives in `core::egress`: address
classification, allowlist matching and the decision type. Each outbound crate
enforces it at its own connect point. `connectors` and `ai` install a reqwest
DNS-resolver hook that classifies every resolved address. For database and
Kafka drivers, which resolve names themselves, the caller resolves, checks, then
connects by IP. The checked address is bound to the actual connection, so a
second lookup cannot swap it. Redirects, retries and endpoints a driver
discovers (Kafka broker metadata) are checked again. No new crate or edge is
needed (5.1).

As built (M3): `zirv_flow_core::egress` holds the pure policy and `zirv-flow-connectors` enforces it
for HTTP through `GuardedClient`. The PostgreSQL and MySQL sources call the same policy at their own
connect points (`SourceContext::resolve_checked`, below); the Kafka consumer and the `ai` crate will
call it at theirs when they land. The Kafka source reaches the network only through its
SOCKS5 relay, which resolves with `resolve_checked` for every connection the client opens: the
bootstrap hosts, every broker the metadata names and every reconnect.

- **Classes.** An address is public, loopback, private, unique-local, link-local, shared CGNAT,
  metadata, unspecified, multicast, broadcast, documentation, benchmarking or reserved. IPv4-mapped,
  IPv4-compatible, NAT64 and 6to4 IPv6 addresses are classified by the IPv4 address they embed;
  Teredo is reserved. Only public addresses are reachable by default.
- **Allowlist.** `egress.allow` takes CIDR ranges only (no host names, no ports); a bare address or
  an entry with host bits set fails startup and `config check`. A range unlocks the non-public
  addresses it contains. A metadata address (`169.254.169.254`, `169.254.170.2`, `fd00:ec2::254`,
  `100.100.100.200`, `168.63.129.16`) is unlocked only by an entry naming exactly it (`/32` or `/128`), never by a
  broader range.
- **Mixed resolutions.** A host name that resolves to several addresses is refused whole if any one
  is denied; the denied address is never filtered out silently.
- **Connect point.** The reqwest DNS hook checks every resolved address and returns only those, so
  the connector dials the checked addresses and DNS cannot change between check and connect. An
  IP-literal URL never reaches DNS and is decided before sending; each redirect hop is checked
  again; environment proxies are ignored. Denials count in `zirv_flow_egress_denied_total{class}`
  and log the host and class, never the path or query.
- **Database connect point.** `SourceContext::resolve_checked(host)` resolves the host (an IP literal
  skips DNS), checks every address with the same policy (one denied address refuses the host; more
  than 8 distinct addresses are refused only after all were checked), and returns the checked list.
  Each source calls it at every connect, so a changed DNS answer is checked again, and hands the
  list, and nothing else, to the driver: PostgreSQL gets one `host`/`hostaddr` pair per address
  (`hostaddr` is the TCP target, `host` the TLS name) and MySQL `resolved_ips`. Neither driver follows
  a redirect or a server-supplied endpoint, and the options that could dial something the caller did
  not name (a Unix socket host, `prefer_socket`, a connection string, local infile) are never set;
  tests on the built options pin that. TLS is verified by default against the configured CA alone
  or the compiled-in public roots (never the system store), and `tls = off` is refused when any
  checked address is public.
- **Exempt.** The OIDC provider calls in `zirv-flow-identity` stay outside the policy, as above.

### 16.2 Secrets, RBAC, audit

Connector credentials, OAuth tokens and AI provider keys use **envelope encryption**: a random
data key per secret (XChaCha20-Poly1305), wrapped by an operator-held master
key (`secrets.master_key_file` or env, identified by key id).
`zirv-flow admin rotate-master-key` re-wraps data keys in one transaction
without decrypting payloads. The API never returns plaintext. A lost master key means
credentials must be re-entered (documented).

As built (M2): the master key arrives in M2 as `secrets.master_key_file` or `secrets.master_key`, with per-purpose HMAC keys derived by HKDF-SHA256 (identifier and redaction hashes); M3 reuses it for the envelope encryption above.

Where it lives (M3): envelope crypto (seal and open, wrap and unwrap, key id)
is pure code in `core::secrets` next to `MasterKey`, adding `chacha20poly1305`.
One `encrypted_secrets` table and repository in `store` holds the ciphertext,
the wrapped data key and the key id. Decryption happens only in `api` and
`jobs`: they open a secret right before use and pass a typed plaintext
credential, zeroized after use, into `connectors` or `ai`, which never decrypt.

As built (M3): `core::secrets` seals a secret with a random 32-byte data key and a random 24-byte nonce (XChaCha20-Poly1305). The associated data is `zirv-flow/secret/v1|<workspace_id>|<secret_id>|<purpose>`, so a ciphertext moved to another row or workspace does not open. The data key is wrapped by a key encryption key (HKDF-SHA256 over the master key bytes, info `zirv-flow/kek/v1`) and stored with the wrapping key id, in `encrypted_secrets` (workspace scope) or `deployment_keys` (deployment scope). `store::secrets` handles sealed values only; `api` and `jobs` open them right before use, into zeroizing values that have no `Display`, `Serialize` or `Clone`. The HMAC keys no longer derive from the master key but from the **HMAC root** in `deployment_keys`: it is created once, atomically, from the master key that produced the existing hashes (`insert ... on conflict do nothing`, then every start reads back the winner), keeps the same HKDF labels, and is re-wrapped, never changed, by `admin rotate-master-key`. A previous key is configured with `secrets.previous_master_key_file` or `secrets.previous_master_key` for the rotation window, and a start with a previous key and no root is refused so a root is never bootstrapped from a rotation's new key. Rotation re-wraps both tables in one transaction and audits key ids and the count. Rotation locks the HMAC root row `FOR UPDATE` first and is refused when it is absent; every secret insert and replace locks it `FOR SHARE` and proceeds only when its wrapping key is one the writer's keyring holds, so a process left on the old key cannot store a row after a rotation, and a write racing a rotation is either re-wrapped by it or refused. A write with no root is refused.

| Role (proposed, confirmed in M8) | Scope | Can |
|------|-------|-----|
| Owner | Organization | Everything: workspaces, users, instance settings, key rotation |
| Admin | Workspace | Sources, destinations, credentials, API keys, members, retention, assistant provider and privacy settings |
| Publisher | Workspace | Editor rights plus publish/pause flows, publish templates, snippets and the brand theme, run imports |
| Editor | Workspace | Draft segments, flows, templates, snippets, lists; assistant drafts when enabled; previews, test mode and test sends |
| Viewer | Workspace | Read only; personal data masked unless granted `view_pii` |

Editors create segments, save current segment versions (ruling e4aa60bb) and refresh a segment now; viewers read its members and history.

M1 ships owner, admin and member; the full matrix lands in M8. The **audit
log** is append-only (actor, action, object, time, request id, change summary
without personal data values) and covers sign-ins, membership and key changes,
credential access, publishes, imports, exports, erasures and configuration.
Assistant calls also write the dedicated call log in section 13.4 whether they
succeed, fail validation or are stopped by policy or budget.

### 16.3 Export, erasure and retention

**Export** (`privacy.export`) produces JSON of everything held on a person
(profile, identities, traits with sources, events, consent history, membership
history, enrollments, messages) behind an expiring link. **Erasure**
(`privacy.erase`): (1) record the request in the erasure ledger (person id,
identifier HMACs); (2) add suppression hashes; (3) delete identities, ledger
contributions, events in all partitions, ingest rejects, list members, consent,
segment memberships, enrollments, step executions, messages, test-send log entries,
person-scoped decision records, stored assistant transcripts, pending
and dead forwarding payloads and pending exports, and clear saved preview-person
references; (4) call `erase` on forwarding and delivery destinations that
declare it and record results; (5) delete the person; (6) write an audit entry
without personal data. Templates,
snippets, themes and assets hold no personal data by design. Each milestone
that adds a table extends erasure, and a test asserts no row for an erased
person survives anywhere. Backups keep erased data until they age out;
after a restore, `zirv-flow admin replay-erasures` re-applies the ledger
(keeping a ledger copy outside the database is OQ-13). **Retention** (proposed
defaults, enforced by scheduler jobs): events 25 months deployment-wide, with
per-name workspace overrides (drop monthly partitions; batched deletes where
an override is shorter), messages 13 months, ingest rejects (including those from syncs and imports) 30 days, import files 7 days after the import ends,
job failure history 14 days, audit log 2 years. Decision-log retention is OQ-28.
Assistant transcripts are not stored unless a workspace opts in to a 1 to 30
day window (default 7), which a retention job purges (13.4, OQ-26).

As built (M2): the ledger is `erasure_requests` (person id, identifier HMACs with their kinds,
status, requester, trace); it is the one table the erasure scan skips on purpose.
`POST /privacy/erasures` records the request and enqueues `privacy.erase` in one transaction;
the job then runs in one transaction: it deletes the pending `ingest_inbox` rows whose
identifiers, parsed and hashed, match any hash of the group (locked `FOR UPDATE`, so a drain either
applied them before or never sees them), after folding the group's identities and ids into the ledger row,
upserts `erased` suppressions from the ledger hashes, calls `delete_person` for each ledger id, completes the ledger row and audits `privacy.erased` with the request id.
`admin replay-erasures` re-applies completed entries after a restore (suppressions, restored
people, and pending inbox rows by hash with the master key). Exports are `privacy_exports`:
`privacy.export` builds the gzip JSON `zirv-flow.person-export/v1`, the download is
admin-only, audited and answers 410 after 24 hours, and the leader's hourly upkeep drops expired
content. A request whose job went dead reads as `failed`. Destination `erase` calls, message and
enrollment tables and backup-copy handling of the ledger (OQ-13) are not built yet.

Segment membership rows (6) join the erasure scan and `delete_person` in M4: both remove a
person's rows, current and closed. A merge does not delete them: it closes the merged person's
rows, and the survivor is re-evaluated through its `merge` change class (7.1).

## 17. Observability and operations

| Area | Design |
|------|--------|
| Logs and traces | Structured JSON through `tracing` on standard error for the long-running commands (`serve`, `worker`, `scheduler`, `all`); one-shot commands print plain text. An `http.request` span carries `request_id`, `trace_id`, `method` and the matched `route`; a `job` span carries `job_id`, `kind`, `workspace_id`, `attempt`, `trace_id`, `cause_kind` and `cause_id`. JSON lines include the current span's fields. The response header `x-trace-id` and the `trace_id` of problem bodies name the trace; with `log.otlp.endpoint` set, spans are exported over OTLP/HTTP (protobuf, batched, dropped on a full queue or a collector error, flushed at shutdown) with the zirv trace id as the OTel trace id and no invented parent span, so `x-trace-id`, the logged `trace_id`, the decision log and the exported trace agree; a valid `traceparent` request header becomes a span link on `http.request` and never a parent or the trace id, `log.filter` applies to exported spans too, and export is off (no provider, layer or task) without the setting. Operational spans link to, but never replace, the customer-event, audit or decision stores in section 12. |
| Metrics | Prometheus, `zirv_flow_` prefix: HTTP latency/status (`zirv_flow_http_requests_total`, `zirv_flow_http_request_duration_seconds`); queue depth `zirv_flow_jobs{kind,state}` for `ready`, `running` and `dead`, claim lag `zirv_flow_job_claim_lag_seconds{kind}` (`now - run_at`), handler duration `zirv_flow_job_duration_seconds{kind}`, attempts `zirv_flow_jobs_total{kind,outcome}`; ingestion rate, inbox age, validation outcomes, duplicates and late events; debugger delivery latency and clients; explorer duration/timeouts/exports; decision-log append duration; sync duration, records, stream freshness; segment refresh duration/size, dirty backlog; active/waiting enrollments, timer lateness; sends by outcome and skip reason, rate-limit waits; forwarding lag/retries/dead letters; template render duration and failures; assistant calls/tokens/estimated cost/latency/outcomes/budget denials; DB pool `zirv_flow_db_pool_connections{pool,state}` and `zirv_flow_db_pool_max_connections{pool}`, jobs-table dead tuples. `/metrics` is served only by processes that run `serve` (so not by a worker-only or scheduler-only process, #21); the leader exports the queue gauges, so they appear where `serve` and the leader run together |
| Health | `/healthz` liveness; `/readyz` readiness (DB reachable, schema version matches, role loops running) |
| Config | TOML file + env overrides `ZIRV_FLOW_<SECTION>__<KEY>` (e.g. `ZIRV_FLOW_DATABASE__URL`). Sections: `server` (bind, `public_url` for tracking links), `database` (url, `read_url`, pool, `migrate_on_start`), `worker` (concurrency, kinds), `ops` (`bind`, off by default: metrics and health listener for worker-only and scheduler-only processes), `log` (`filter`, an EnvFilter directive, default `info`; `format`, `json` or `text`, default `json`), `secrets` (`master_key_file`, `master_key`), `ai` (allowed providers/models/base URLs). `config check` prints the result with secrets redacted. |
| Migrations | sqlx, embedded, forward-only. `zirv-flow migrate` explicitly, or guarded `database.migrate_on_start` / `--migrate` under an advisory lock (one migrator). Startup refuses an unknown (newer) schema, and pending migrations when migrating is not enabled. |
| Packaging | GHCR image (multi-stage, non-root, linux amd64/arm64); a static Linux release binary from CD; `docker-compose.yml` quickstart (`zirv-flow all` + Postgres, health checks, volume). Larger setups: `serve` behind a load balancer, N workers, two schedulers, and a CDN or caching proxy in front of `/a/` image assets. Worker-only and scheduler-only processes serve `/metrics`, `/healthz` and `/readyz` on `ops.bind` (off by default); with `serve` the endpoints stay on `server.bind`. PostgreSQL 16 or newer is required. |
| Backup and restore | Postgres is the only stateful component: `pg_dump` when small, WAL archiving + point-in-time recovery in production. Back up the master key and config separately. Restore = database, matching binary, erasure replay; rehearsed in M8. |
| Upgrades (v1) | Back up, stop all roles, `zirv-flow migrate` with the new binary, start. Mixed-version rolling upgrades are OQ-17. |

## 18. Testing strategy

| Layer | Tests |
|-------|-------|
| `core`, `segments` | Inline unit and property tests; golden SQL snapshots for the compiler; the AST generators (`proptest`, behind a `test-support` feature) and the golden corpus live in `segments`, golden files blessed with `ZIRV_FLOW_BLESS` |
| Segments on PostgreSQL | Tests in `zirv-flow-jobs` comparing compiled SQL on Postgres with the reference evaluator; CI runs fixed seeds, and a scheduled nightly workflow runs random seeds without filing issues |
| `engine` | Inline unit tests; deterministic simulation: virtual clock, crash after every effect, randomized event arrival. Invariants: no internal effect applied twice, every external effect enqueued with a stable key, no timer fires early, every enrollment ends or waits on a due wake. |
| Store and queue | Postgres-backed integration tests in CI: claim concurrency, lease expiry, fencing, unique keys, scheduler failover |
| Ingestion and events | Idempotent replay, identity scenario suite, schema-mode and unplanned-event matrix, redaction, duplicate/late/future handling, reject replay, debugger masking and SSE latency, explorer filters/timeouts/CSV, deletion propagation, erasure table scan |
| Connectors | Database sources against service containers; HTTP manifests against recorded fixtures and a mock server (pagination, OAuth refresh, `429`) |
| `templates` | Golden render corpus per block and per channel; representative email layouts validated through `mrml`; preview-equals-send byte test (tracking tokens excluded); golden tests per document schema migration; sanitizer allowlist tests |
| Causality | Trace-completeness integration fixtures for every derived action and cause; job retry and failure trees; assertions that decision records contain ids and reasons but no personal-data values |
| Assistant | Deterministic fake provider and recorded tool calls in CI; tool-registry allowlist, RBAC/workspace isolation, no-PII context, budget/rate-limit, bounded repair, prompt-injection and draft-only tests. Golden-prompt evaluations against real configured providers run manually or nightly, never as a CI gate. |
| API and web | Handler tests on real Postgres, RBAC matrix, OpenAPI drift check; SPA type check and unit tests, browser end-to-end tests from M5 |
| Performance | Load generator and benchmarks per milestone slice; full validation in M8 |
| Gates | The M0 CI gates (fmt, clippy, nextest, test-presence, advisory security scan) apply to every PR; from M1, the crate-dependency allowlist, purity check and frontend import-boundary lint in section 5.5 are also required |

## 19. Scalability envelope

Every figure is a **TARGET to validate, not a measurement**; measurements are
recorded in the "As measured" notes below the table. Each milestone measures its slice and M8 validates all of them on
reference hardware (assumed: PostgreSQL 8 vCPU / 32 GB RAM / NVMe; two
`zirv-flow` hosts of 4 vCPU). A missed target is fixed or revised in a recorded
decision, never dropped silently. Beyond the envelope: more workers, a read
replica for preview and explain (deferred, 8.3), event rollups (OQ-2), then the ADR-003 add-ons
(external broker, analytical event store), only on measured need.

| Dimension | v1 TARGET |
|-----------|-----------|
| People per workspace / per deployment | 10 million / 25 million |
| Retained events per deployment | 500 million rows |
| Sustained / burst ingestion | 2,000 events/s / 10,000 events/s for 10 min, backlog drained within 15 min |
| Ingestion API latency | p99 < 100 ms at sustained rate |
| Inbox to applied profile | p95 < 5 s at sustained rate |
| Live debugger delivery from durable inbox append | p95 < 2 s at sustained ingestion rate |
| Event explorer interactive query | p95 < 5 s for a 90-day filtered count or property breakdown; hard timeout 15 s |
| Decision-log append overhead | p95 < 5 ms added transaction time and < 10% ingestion-throughput loss against the same workload with decision writes disabled |
| Segment preview, 10 M people | p95 < 3 s trait-only; < 15 s with a 90-day event aggregate |
| Full segment refresh, 10 M people | < 2 min trait-only; < 10 min with event aggregates |
| Event to segment entry to enrollment | p95 < 60 s |
| Waiting enrollments; timer lateness | 10 million; p99 < 60 s at that volume |
| Job throughput | 1,000 jobs/s claimed and completed |
| Send throughput | 300 messages/s per deployment, provider limits permitting |
| Template render, typical block email (about 20 blocks, conditions pre-evaluated) | p95 < 50 ms per message |

**As measured (M2).** The ingestion rows were measured on a development machine, not the reference
hardware, at three sizes of the zirv-flow stack (2, 4 and 8 vCPU) in
[docs/benchmarks/m2-ingestion.md](benchmarks/m2-ingestion.md). Sustained 2,000 items/s on one workspace
holds (API p99 11 to 80 ms, inbox to applied p95 35 to 46 ms). Missed there: the decision-log append
(52.9 ms of added p95 per 500-event drain transaction, 34% throughput loss; #75) and the API p99 of
500-item batches (125 ms; #76). The 10-minute burst drain first missed (projected at about 19 minutes
from a 2-minute run; #74): one workspace's drain applied about 3,400 mixed items per second because its
shards shared one task. With each shard on its own task (#74) the drain applies about 4,000 to 4,400
items per second, and a measured 10-minute burst at 10,000/s drained in 890 s (14.8 minutes), 10 s inside
the 15-minute target, a thin margin. The figures
are not a v1 sign-off; the targets are re-checked on reference hardware before 1.0.

**As measured (M4).** The segment rows were measured at 10 million people and 50 million events on a
development machine (PostgreSQL on cpuset `8-31`, the app at 4 vCPU) in
[docs/benchmarks/m4-segments.md](benchmarks/m4-segments.md), first on image 0.56.0 and again on 0.57.0
after #142 to #144. On 0.57.0: trait-only preview p95 1.06 s on slots (3 s), aggregate preview p95
4.68 s and property-filtered aggregate preview p95 1.20 s (15 s, no 503s), full refresh median 45.7 s
trait-only (2 min) and 14.5 s with an aggregate (10 min). OQ-2 is decided (21.1): no rollup for v1.
Still missed: event to `segment.entered` (p95 278 s against 60 s, a plan problem in the batch diff
that #144 exposed, section 4.3, follow-up D, of the benchmark document), and an aggregate preview on a cold cache
(about 15 s right after an app restart). On 0.56.0 the previews had missed because the persistent
prepared statement fell back to a generic plan after five executions, a slot backfill left no
statistics, and an aggregate was a correlated subquery per person (38.7 s and 97.5 s uncapped; 503 at
the 10 s limit, which also contradicted the 15 s target). The figures are not a v1 sign-off.

## 20. Key decisions

ADR-001 to ADR-005 are **Accepted (M0)**. ADR-006 to ADR-009 are **Accepted**;
they were ruled as their capabilities were added to the plan.

### ADR-001: Sync-first data model

- **Context.** Segments must combine many sources, resolve identity across them, show freshness, and run fast and repeatably. Many sources are SaaS APIs that cannot answer analytical queries; live cross-source joins are neither consistent nor fast.
- **Options.** (1) Federated live queries only: no copies, but impossible for most APIs, no cross-source identity, availability tied to every source. (2) Sync-first into an own profile store. (3) Warehouse-native: requires every operator to run a warehouse, breaking the binary + Postgres baseline.
- **Decision.** (2). Connectors incrementally import records and events into the Postgres profile store (identity-resolved people, typed traits, events); segments are evaluated there. Per-source freshness is visible, ingestion is idempotent and incremental, deletions propagate from the start. A federated live-query source mode follows in a later milestone for data that cannot or must not be copied.
- **Consequences.** Fast, explainable, source-independent segments with event history. Personal data is copied, so erasure and retention must reach the copies. Data is as fresh as the last sync, which the UI must show. Storage grows with sources. The AST must not assume all data is local (OQ-18).
- **Reverse if.** Many deployments cannot copy their people data (policy or volume); measured sync lag cannot meet freshness needs live queries would; or copied volume exceeds the Postgres envelope and the analytical add-on does not fix it.

### ADR-002: Embedded TypeScript + React SPA

- **Context.** The core UI is highly interactive (flow canvas with drag, zoom, edge routing; nested segment builder with live counts), and the product must stay a single binary.
- **Options.** (1) Server-rendered HTML with progressive enhancement: one toolchain, but a graph editor is impractical. (2) A Rust-to-WASM UI framework: one language, but a less mature graph-editor ecosystem and a narrower contributor pool. (3) TypeScript + React SPA with React Flow, embedded.
- **Decision.** (3). Built in CI, embedded with rust-embed, served by axum.
- **Consequences.** Mature canvas library; typed client generated from OpenAPI. Two toolchains (Node in CI and for UI work); frontend dependencies become a supply-chain concern; binary grows by the asset size.
- **Reverse if.** A Rust-native UI stack reaches comparable graph-editing maturity and the dual toolchain measurably slows contributions; or embedding blocks a needed deployment pattern (e.g. separately hosted UI).

### ADR-003: Baseline deployment = binary + PostgreSQL

- **Context.** Small teams self-host. Each extra stateful service adds backup, upgrade, monitoring and HA work. Domain writes and job enqueues must be atomic or work is lost or duplicated.
- **Options.** (1) Postgres + external message broker: more queue throughput, but a second system and dual writes or an outbox relay. (2) Postgres + in-memory store for queues and rate limits: durability gaps, one more service. (3) Postgres only.
- **Decision.** (3). Postgres is system of record, profile/event store, durable job queue (`FOR UPDATE SKIP LOCKED`, short claim transactions, leases, retries, idempotent handlers, indexed due times) and scheduler; jobs execute outside the claim transaction. External broker only when measured throughput or fan-out justifies it; an analytical event store (e.g. ClickHouse) is an optional later add-on.
- **Consequences.** One component to operate; transactional enqueue. Queue throughput shares Postgres write capacity; vacuum and bloat need active management; Postgres is the single point of failure (HA = operator's replication).
- **Reverse if.** At the section 19 targets, queue tables become the dominant write load or claim lag p99 exceeds the timer-lateness target on an otherwise healthy database; campaign fan-out cannot reach the send target because of claim contention; event queries miss preview targets (analytical add-on).

### ADR-004: Extension model

- **Context.** The long tail of sources and destinations is mostly paginated HTTP with standard auth. Auth refresh, retries, rate limits, checkpoints and delivery semantics are where integrations fail, so they must be implemented once, in core. No third party ships connectors yet.
- **Options.** (1) Built-in Rust connectors only: every API needs a release. (2) Built-ins + bounded declarative HTTP manifest + generic webhooks. (3) Sandboxed WASM component plugins now: ABI, host interface and sandbox to maintain before anyone needs them. (4) External connector processes over stdio: any language, but process management, weaker contracts, runtime concerns leave core. (5) Embedded scripting: unbounded, hard to secure and test.
- **Decision.** (2). Built-in Rust connectors behind `Source`/`Destination` traits; a bounded manifest for auth, pagination and field mapping; generic inbound/outbound webhooks. Auth, retries, rate limits, checkpoints and delivery semantics are core connector concerns. WASM plugins deferred until third-party isolation or independent plugin distribution is a demonstrated need.
- **Consequences.** Uniform reliability; manifests are reviewable data testable with fixtures. Non-HTTP or logic-heavy integrations need a Rust PR and release. The manifest language must resist becoming a programming language.
- **Reverse if.** Requests manifests cannot express regularly outpace what maintainers merge; third parties want to distribute connectors independently; operators need to run untrusted connector code.

### ADR-005: Release cadence

- **Context.** Before M1 nothing is functional; per-merge releases would publish binaries that do nothing. After M1, frequent releases keep `main` releasable and feedback short.
- **Options.** (1) Release on every merge from the start: noise, non-functional releases. (2) Tag-driven only: batches changes, depends on someone tagging. (3) Manual dispatch until M1, then every merge to `main`.
- **Decision.** (3). CD publishes only by manual dispatch until M1 (first functional milestone), then on every merge to `main`.
- **Consequences.** From M1 every merge is a release: migrations, config and API changes must be release-safe in every PR, and versioning must be automated. Before M1 a maintainer must remember to dispatch.
- **Reverse if.** Per-merge releases cause measurable user harm (a broken release reaches users, or operators report upgrade churn); then batched or tagged releases.

### ADR-006: Template builder

- **Context.** Marketers must build content for every channel without writing HTML. Email HTML has to render across clients with very uneven CSS support. Merge fields, unsubscribe links and sender identities must be checked against Zirv Flow's own registry and consent model at publish, and preview must show exactly what will be sent. Rendering runs in Rust on the server for both preview and send, and must be deterministic for golden tests.
- **Options.** (1) Zirv Flow's own channel-aware block document (versioned JSON) with a server-side Rust renderer. (2) Embed an open-source JavaScript email editor and store the HTML it produces, with form editors for other channels: fast to start, but the stored HTML is opaque to merge-field and compliance checks, every channel gets a different model, and the embedded editor's format and output changes become dependencies Zirv Flow does not control. (3) Code-only templates (status quo): nothing to maintain, but marketers write responsive email HTML by hand, which is the problem being solved.
- **Decision.** (1). The React builder edits the block document. Rust renders it per channel: email blocks become MJML and then responsive HTML via the `mrml` crate, plus a generated text part; webhook content (later SMS, push and in-app) renders from typed content schemas. Code mode (raw HTML + text with MiniJinja) stays as an escape hatch, one-way only: blocks to code is allowed, code to blocks never. Preview uses the same renderer as send. The first release is narrow: a small email block set, typed forms for other channels, explicit document schema versions. Representative email layouts are validated through `mrml` before the initial block set is committed (an M7 exit criterion). v1.0 covers email and webhook payloads; SMS, push and in-app content types are reserved in the schema, and their builders ship with those channels after 1.0.
- **Consequences.** Zirv Flow owns the editor and the document schema migrations; this cost is accepted. Structured content makes validation, personalization checks and preview-equals-send possible. `mrml` becomes a key dependency, so its MJML gaps become Zirv Flow's. Each block added is a permanent migration commitment (OQ-23). The raw HTML block and code mode escape most structural checks other than parsing, size and the required unsubscribe link.
- **Reverse if.** Block-set requests consistently outpace delivery, so marketers fall back to code mode for common layouts; `mrml` output fails client compatibility for representative layouts and cannot be fixed or worked around; or maintaining the editor and its migrations measurably crowds out the segmentation, data source and integration work.

### ADR-007: Governed customer events and a distinct causality log

- **Context.** Customer events drive segments and flows, but an unversioned catalog cannot prevent schema drift, and customer events alone cannot explain retries, skips or system decisions. Audit and operational logs have different privacy and retention purposes and cannot safely become product history.
- **Options.** (1) Keep an informal catalog and diagnose from operational logs. (2) Put validation and every derived decision into the customer-event stream. (3) Use a versioned event schema registry and keep customer events, audit records, append-only decision records and operational logs distinct, linked by durable ids.
- **Decision.** (3). Event schemas carry typed properties, required fields, enums, descriptions and PII flags, with per-event `allow`/`warn`/`block` validation and a workspace policy for unplanned names. Redaction precedes storage; rejects, debugger, explorer and replay make ingestion visible. Every derived action carries a propagated `trace_id` and typed cause into a personal-data-free, time-partitioned decision log and trace API. Client `messageId` dedupe gives effectively-once event storage; forwarding is at-least-once with stable keys and visible retries. Section 12 is authoritative.
- **Consequences.** More writes and partitions buy deterministic investigation and safe replay. Every handler must append its decision record transactionally and extend erasure. The explorer stays on Postgres until measured targets justify an analytical add-on. Replays are explicit and do not silently re-trigger flows.
- **Reverse if.** On reference hardware, partitioning and indexing cannot meet the section 19 debugger, explorer or decision-write targets without harming core ingestion, after the measured Postgres optimizations are exhausted. Then move the relevant query or append workload behind the same API to the analytical add-on; do not collapse the four record types or weaken trace semantics.

### ADR-008: Draft-only LLM assistance over typed tools

- **Context.** Natural-language assistance can shorten authoring and explanation, but direct model output is nondeterministic, may contain injected instructions, may disclose personal data and must not bypass existing validators or publishing permissions. Operators need a provider choice that includes private local servers.
- **Options.** (1) No LLM support. (2) Feature-specific prompt endpoints that accept provider text and write product objects directly. (3) An optional provider abstraction whose models can call only shared, typed read-and-draft tools; validate the result, show a diff and require user acceptance into a draft.
- **Decision.** (3). `zirv-flow-ai` supports OpenAI-compatible and Anthropic APIs, including explicitly allowlisted local base URLs. Deployment and workspace settings constrain providers, models, budgets and PII use; credentials use envelope encryption and calls use the egress policy. Tool schemas come from API Rust types, every call is re-authorized as the user, repair attempts are bounded, and no publish, send, delete or consent-mutating tool exists. Model output and supplied context are untrusted. Real-model evaluations are manual or nightly; CI uses a deterministic fake. The same read-and-draft layer may be exposed through MCP after 1.0.
- **Consequences.** Provider behaviour can change without changing artifact semantics, and generated drafts remain testable through deterministic validators. The product gains provider HTTP dependencies, cost accounting, prompt-injection surface and transcript policy. Schema-only context is less personalized but is the safe default; PII requires explicit workspace opt-in.
- **Reverse if.** Golden-prompt and usability evidence shows no sustained authoring benefit over the manual builders at acceptable cost and latency; required model capabilities are unavailable to self-hosters; or security testing shows the fixed tool and context boundary cannot contain prompt injection or personal-data disclosure. Then disable or remove providers while retaining the ordinary typed drafts and validators.

### ADR-009: Modular monolith with enforced boundaries

- **Context.** Zirv Flow ships as one binary, but sources, people, events, segments, flows, templates, delivery and assistance change for different reasons. Informal layering would let I/O leak into deterministic code, feature internals become shared by accident and every new implementation add another dispatch path.
- **Options.** (1) Layered crates, vertical feature modules inside each crate and exactly five trait-backed implementation registries, with dependency and import boundaries enforced in CI. (2) Vertical bounded-context crates over a shared kernel: stronger domain ownership, but premature while one team owns the system and the domain boundaries are still being validated. (3) One crate with modules only: fewer manifests, but no machine-checked dependency direction between layers.
- **Decision.** (1). Keep the layered crates and the direct-dependency allowlist in section 5.1. Inside them, use feature modules with `pub(crate)` by default and explicit `mod.rs`/`lib.rs` re-exports. The only implementation registries are connectors (`Source`), destinations and channels (`Destination`), template content types, job kinds and AI tools. The SPA mirrors the domain features behind `index.ts` boundaries. CI enforces crate edges, pure-crate dependencies and frontend imports from M1.
- **Consequences.** The deployable remains one binary and features can share transactions through the composition crates, while illegal dependency directions and deep imports fail before review. Adding an implementation is one module plus one registry line. Some cross-feature work needs an explicit public type or function instead of a shortcut, and changes that genuinely alter layering require an architecture decision and allowlist update.
- **Reverse if.** A domain area demonstrates independent ownership and a stable dependency boundary that justifies a bounded-context crate, or repeated legitimate changes must cross feature surfaces and keep forcing changes to the dependency table. Extract that bounded context over the smallest shared kernel and replace this ADR with a superseding decision; do not dissolve the enforced boundaries.

## 21. Open questions

| ID | Question | Decide by |
|----|----------|-----------|
| OQ-7 | Can in-flight enrollments migrate to a new flow version? | M5 |
| OQ-9 | Rate-limit token block size and behaviour under worker churn | M6 |
| OQ-10 | How to flag machine-generated opens, and whether segments exclude them by default | M6 |
| OQ-11 | Scope and timing of a dedicated transactional messaging API | M6 |
| OQ-12 | Bounce handling for SMTP-only setups without provider webhooks (VERP, DSN mailbox polling)? | M6 |
| OQ-13 | Where the erasure ledger is kept outside the database so it survives a restore | M8 |
| OQ-14 | Should flows pause sends automatically when an input source is stale? | M5 |
| OQ-16 | Is OIDC SSO required for v1.0, or post-1.0? Decided in M1: OIDC SSO through any compliant provider, with Keycloak as the reference. | Closed (M1) |
| OQ-17 | Rolling, mixed-version upgrades with expand/contract migrations | Post-1.0 |
| OQ-18 | Federated mode shape: which AST nodes run against a live source; freshness and identity there | M9 |
| OQ-19 | Suppression scope: per workspace only, or also organization-wide across brands? | M6 |
| OQ-21 | Asset limits: per-file size, per-workspace quota, and when object storage becomes required rather than optional | M7 |
| OQ-22 | Dark-mode handling: themed dark variants, or rely on client inversion with contrast checks? | M7 |
| OQ-23 | Block-set growth policy: what evidence and process add a block, given each block is a permanent migration commitment | M7 |
| OQ-24 | Localization: multi-language variants per template selected by a person trait, in v1.0 or post-1.0? | M7 |
| OQ-25 | Representative layout set and mail-client matrix used to validate `mrml` output | M7, before the block set is frozen |
| OQ-28 | What is the default decision-log retention, and what per-workspace volume limits preserve useful traces without unbounded storage? | M8, by retention and load validation |
| OQ-29 | Which measured explorer workload triggers the optional analytical store: retained rows, query p95, write interference or all three? | M8 benchmark; M9 promotion decision |
| OQ-30 | How does the post-1.0 MCP server authenticate users and API clients while preserving workspace RBAC and per-call audit? | M9, before an MCP milestone is promoted |
| OQ-32 | Consent dimensions and policies: are dimensions a per-workspace catalog with typed values, or topics with categories? How does the policy reuse the M4 condition AST, and how does a recorded-consent read address dimension values? | M6, before the send pipeline ships |

### 21.1 Decided questions

- **OQ-2:** No per-person daily rollup for v1. Event aggregates compile to a grouped
  set over `events` (workspace, name, window, property filters, `GROUP BY person_id
  HAVING agg op N`) semi-joined to `people` (#144), and the incremental path
  restricts that set to the batch's people. Decided by the D5 rule of the M4
  benchmark (#127) on 0.57.0 at 10 million people and 50 million events: the
  90-day aggregate preview is p95 4.68 s and the property-filtered sum p95
  1.20 s, both under 15 s with no 503s, and a full aggregate refresh takes a
  median 14.5 s against 10 min (38.7 s, 97.5 s and 57.6 s before). The grouped
  set reads every event of the window, so a preview of a large aggregate is
  I/O-bound: measured 8.7 s to 15 s on a cold cache, which is the thin margin to
  watch at M8. The same run showed the incremental path missing its target (p95
  278 s against 60 s) on a plan problem in the batch diff, not on the aggregate
  (`docs/benchmarks/m4-segments.md`, section 4.3, follow-up D). Revisit rollups only if
  M8 on reference hardware misses the preview target with a warm cache. See
  [docs/benchmarks/m4-segments.md](benchmarks/m4-segments.md).

- **OQ-4:** Custom object types are post-1.0. v1 sources map to people, events
  and list membership (7.2); order- or account-like data arrives as events with
  typed properties or as traits. The store model is unchanged.
- **OQ-5:** The bounded transform set already in `core::mapping` is the v1
  mapping language, plus nested JSON path selection for field sources. There is
  no expression language; a request outside the set becomes an issue (7.2).
- **OQ-31:** Kafka uses `rskafka` (pure Rust, rustls TLS, SASL PLAIN and SCRAM)
  without consumer groups. The worker holding the stream's job lease discovers
  the partitions and fetches each from per-partition next offsets stored in our
  database, written in the apply-batch transaction with the lease fenced inside
  it. JSON values only in v1; Avro and Protobuf with a schema registry come
  post-1.0. The TLS and compression features are validated on both musl
  architectures (amd64, arm64). Sections 7.6 and 14.
- **OQ-26:** Assistant transcripts are not stored by default. A workspace admin
  can opt in to a debugging window of 1 to 30 days (default 7), purged by a
  retention job; changing the setting is audited. The content-free call audit
  log is always kept (13.4, 16.3).
- **OQ-27:** A model needs structured tool calls with a JSON Schema subset
  (object, string, number, integer, boolean, array, enum, required), token usage
  reporting and a context window of at least 32k tokens. Streaming is optional.
  A capability probe on save checks tool calls and usage; the window comes from
  provider metadata or operator configuration; enablement is refused when a
  requirement is missing (13.1).
- **OQ-6:** A definition edit writes a new version and applies the membership diff with cause
  `definition_change`; events and decisions are recorded. Flow segment-entry and segment-exit
  triggers ignore `definition_change` changes. A flow that wants the current members chooses
  "enroll current members" at publish (M5). Section 8.4.

- **Segment semantics (M4):** leaves are two-valued, `not` is boolean negation that normalization
  never pushes into a leaf operator, and aggregates over zero events are missing except `count`
  (8.1). Dates compare at UTC midnight: per-workspace time zones for date conditions are out of
  scope in v1, a recorded limitation to revisit with flow schedules (M5).

- **OQ-1:** Hot traits are promoted to typed columns with btree indexes; every other trait stays in the
  `people.traits` `jsonb`. At 1 million people and 20 mixed-type traits (M2 benchmark, tier M's
  PostgreSQL, medians of 5), against `jsonb` with expression indexes on the same three traits: segment-style
  counts are 17 to 21 times faster (1.0 and 1.3 ms against 18.1 and 27.4 ms, because typed columns allow
  index-only scans), the three-trait AND count and id scan 1.8 times faster (26 ms against 47 ms), an upsert of
  10,000 trait changes 1.23 times faster (133 ms against 164 ms) and the table 12% smaller; the plain id scans
  are within 6 to 12% (31 against 28 ms, 19 against 18 ms), the one place `jsonb` is ahead. The difference is
  outside noise, so the simpler design is not kept. Cost: a promoted trait has one fixed type and
  two copies to keep in step. Promotion needs no per-workspace DDL: `people` gains a fixed set of generic typed slot
  columns per promotable scalar type, 8 string, 8 number, 6 timestamp, 4 date and 4 boolean per workspace, each with a
  partial btree index on `(workspace_id, slot)` where the slot is not null. An admin marks a trait "indexed for
  segments"; the registry assigns a free slot of its type, and a backfill job copies the values race-safely alongside
  writes. The write path keeps `jsonb` and the slot in step, and the compiler reads the slot only after the backfill
  completes. Unmarking retires the slot safely before it is cleared or reassigned. Marking is refused when no slot of the type is free; the trait then
  stays in `jsonb`. `string_list` traits stay in `jsonb`. The promotion is built in the
  segment milestone (M4); until the slots exist the `jsonb_path_ops` index stays, and it is reconsidered once promoted
  traits cover the equality searches. The slots are live (#125): states `backfilling`, `ready` and `retiring`; writers take the shared slot lock before they read the assignments and assignment changes take the exclusive one, so a backfill never leaves a stale value (`people/slots.rs`); the routes are `POST` and `DELETE /traits/{key}/index`.
  Known limitation: migration 0033 builds its 30 partial indexes in one transaction, which blocks writes to `people` for the build on a populated install; index creation must be revisited before upgrades of populated installations are supported.
  Details in [docs/benchmarks/m2-ingestion.md](benchmarks/m2-ingestion.md).

- **OQ-8:** A per-workspace running cap in the claim query
  (`worker.max_running_per_workspace`, default unlimited); weighted claiming is
  rejected for v1. M8 validates it under load (section 14).
- **OQ-3:** No unmerge in v1. Per-kind identifier rules (7.3) prevent bad
  merges. A blocked merge is reviewed as an identity conflict, and the review
  either keeps the people separate or moves the identifier to one person. Every
  merge records its provenance (moved ids in a merge record, plus a decision
  record). Moving an identifier is not a person split: an unmerge would also
  have to decide who owns contributions, events and memberships added after the
  merge, so provenance alone does not make it deterministic. Unmerge is an M9
  theme, promoted on evidence.
- **OQ-20:** HTTP API only in v1. `identify`, `track` and `batch` are documented
  with curl and `fetch` examples (write-only keys, limits, `messageId` reuse on
  retries). Browser and mobile SDKs are an M9 theme, promoted when adoption
  shows repeated integration friction.
- **OQ-15:** PostgreSQL 16 is the minimum supported version. CI covers
  PostgreSQL 16 and 18. Reassess the minimum before v1.0 against the versions
  supported at that release date.
