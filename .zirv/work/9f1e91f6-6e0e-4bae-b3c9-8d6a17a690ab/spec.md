# Zirv Flow decision record

Status: all five decisions **Accepted at M0**. Zirv Flow is a self-hosted
marketing automation tool: one Rust binary, `zirv-flow`, plus PostgreSQL.
Marketers connect data sources, segment the people in their business, and run
automation flows. This file records the binding decisions the M0 workflow
ruled on.

A decision is never edited in place. To change one, add a new entry that
supersedes it and link the two both ways. The full design is in
[docs/architecture.md](../../../docs/architecture.md); its section 17 holds these decisions in
context under the same ADR numbers.

## ADR-001: Sync-first data model

- **Context.** Segments must combine data from many sources, resolve one person across them, show how fresh the data is, and evaluate quickly and repeatably. Many sources are SaaS APIs that cannot answer analytical queries, and live joins across sources are neither consistent nor fast.
- **Options considered.**
  1. Federated live queries only. No copies, but impossible for most APIs, no cross-source identity resolution, and availability depends on every source.
  2. Sync into Zirv Flow's own profile store.
  3. Warehouse-native evaluation. Requires every operator to run a warehouse, which breaks the binary-plus-Postgres baseline.
- **Decision.** Option 2.
  - Connectors incrementally import records and events into Zirv Flow's Postgres profile store (identity-resolved people, typed traits, events), and segments are evaluated there.
  - Per-source freshness is visible. Ingestion is idempotent and incremental, and deletions propagate from the start.
  - A federated live-query source mode follows in a later milestone, for data that cannot or must not be copied.
- **Consequences.**
  - Segments are fast, explainable and independent of source uptime, and event history is kept.
  - Personal data is copied, so erasure and retention must reach every copy.
  - Data is only as fresh as the last sync, and the UI must show this.
  - Storage grows with the connected sources.
  - The segment definition format must not assume all data is local.
- **Reversal evidence.** Any of: a large share of deployments cannot copy their people data, for policy or volume reasons; measured sync lag cannot meet freshness needs that live queries would meet; copied volume exceeds the Postgres envelope and the analytical add-on (ADR-003) does not resolve it.

## ADR-002: Embedded TypeScript + React SPA

- **Context.** The core UI is highly interactive: a flow canvas with drag, zoom and edge routing, and a nested segment builder with live counts. The product must remain a single binary.
- **Options considered.**
  1. Server-rendered HTML with progressive enhancement. One toolchain, but a graph editor is impractical.
  2. A Rust-to-WASM UI framework. One language, but its graph-editor ecosystem is less mature and it narrows the contributor pool.
  3. A TypeScript + React SPA with React Flow, embedded in the binary.
- **Decision.** Option 3. CI builds the SPA, rust-embed embeds it in the binary, and the axum API server serves it.
- **Consequences.**
  - A mature canvas library, and a typed client generated from the OpenAPI spec.
  - Two toolchains (Node in CI and for UI work).
  - Frontend dependencies become a supply-chain concern.
  - The binary grows by the size of the assets.
- **Reversal evidence.** A Rust-native UI stack reaches comparable graph-editing maturity while the dual toolchain measurably slows contributions. Or embedding blocks a needed deployment pattern, such as a separately hosted UI.

## ADR-003: Baseline deployment = binary + PostgreSQL

- **Context.** Small teams self-host Zirv Flow, and every extra stateful service adds backup, upgrade, monitoring and HA work. Domain writes and job enqueues must commit atomically, or work is lost or duplicated.
- **Options considered.**
  1. Postgres plus an external message broker. More queue throughput, but a second system to run, plus dual writes or an outbox relay.
  2. Postgres plus an in-memory store for queues and rate limits. Durability gaps and one more service.
  3. Postgres only.
- **Decision.** Option 3.
  - Postgres is the system of record, the profile and event store, the durable job queue and the scheduler.
  - The queue uses `FOR UPDATE SKIP LOCKED`, short claim transactions, leases, retries, idempotent handlers and indexed due times. Jobs execute outside the claim transaction.
  - An external broker is added only when measured throughput or fan-out justifies it. An analytical event store (e.g. ClickHouse) is an optional later add-on.
- **Consequences.**
  - One stateful component, and enqueue that is transactional with domain writes.
  - Queue load competes with domain writes for Postgres capacity.
  - Vacuum and bloat on queue tables need active management.
  - Postgres is the single point of failure; HA is the operator's Postgres replication.
- **Reversal evidence.** At the documented v1 targets, any of: queue tables become the dominant write load; claim lag p99 exceeds the timer-lateness target on an otherwise healthy database; campaign fan-out cannot reach the send target because of claim contention. Event queries that miss the preview targets justify the analytical add-on.

## ADR-004: Extension model

- **Context.** Most sources and destinations are paginated HTTP APIs with standard authentication. Integrations fail on auth refresh, retries, rate limits, checkpoints and delivery semantics, so these must be built once, in core. No third party ships connectors yet.
- **Options considered.**
  1. Built-in Rust connectors only. Every new API needs a release.
  2. Built-in connectors, a bounded declarative HTTP manifest, and generic webhooks.
  3. Sandboxed WASM component plugins now. An ABI, a host interface and a sandbox to maintain before anyone needs them.
  4. External connector processes over stdio. Process management, weaker contracts, and the core concerns move out of core.
  5. Embedded scripting. Unbounded, and hard to secure and test.
- **Decision.** Option 2.
  - Built-in Rust connectors sit behind `Source`/`Destination` traits.
  - A bounded declarative HTTP connector manifest covers auth, pagination and field mapping.
  - Generic inbound and outbound webhooks cover the rest.
  - Auth, retries, rate limits, checkpoints and delivery semantics are core connector concerns.
  - Sandboxed WASM component plugins are deferred until third-party isolation or independent plugin distribution is a demonstrated need.
- **Consequences.**
  - Reliability is uniform across connectors.
  - Manifests are reviewable data that can be tested with fixtures.
  - A non-HTTP or logic-heavy integration needs a Rust PR and a release.
  - The manifest language must not grow into a programming language.
- **Reversal evidence.** Any of: requests that manifests cannot express regularly outpace what maintainers can merge; third parties want to distribute connectors independently; operators need to run connector code they do not trust.

## ADR-005: Release cadence

- **Context.** Before the first functional milestone (M1) there is nothing usable to release. After it, frequent releases keep `main` releasable and feedback loops short.
- **Options considered.**
  1. Release on every merge from day one. Noise, and releases that do nothing.
  2. Tag-driven releases only. Batches changes and depends on someone tagging.
  3. Manual dispatch until M1, then every merge to `main`.
- **Decision.** Option 3. CD publishes releases only by manual dispatch until M1, then on every merge to `main`.
- **Consequences.**
  - From M1 on, every merge is a release. Migrations, config changes and API changes must be release-safe in every PR, and versioning must be automated.
  - Until M1, a maintainer has to remember to dispatch.
- **Reversal evidence.** Per-merge releases cause measurable harm: a broken release reaches users, or operators report upgrade churn. In that case, switch to batched or tagged releases.
