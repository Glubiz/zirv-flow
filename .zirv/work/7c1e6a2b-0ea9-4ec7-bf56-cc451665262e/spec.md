# Event logging and LLM assistance decisions

Status: ADR-007 and ADR-008 accepted. The binding architecture is
[architecture.md](../../../docs/architecture.md), sections 12, 13 and 20.

## ADR-007 — Governed events and distinct causality
### Context
Customer events drive segments and flows, but an informal catalog permits
schema drift and cannot explain derived actions, skips or failures. Audit
records and operational logs have different readers, privacy rules and
retention, so neither is a safe product-history store.

### Options
1. Keep an informal catalog and investigate through operational logs.
2. Store customer facts and all system decisions in one event stream.
3. Govern customer events with versioned schemas and keep customer events,
   audit records, decision records and operational logs distinct but linked.

### Decision
Choose option 3. Each event name has immutable schema versions with typed and
required properties, enums, descriptions and PII flags. Per-event validation
is `allow`, `warn` or `block`; a workspace either drafts a schema for an
unplanned name or rejects it. Redaction precedes storage. Rejects, validation
results, a masked SSE debugger, a bounded explorer, CSV export and replay are
product surfaces.

Every derived action carries a `trace_id` and typed cause through job payloads.
An append-only, time-partitioned decision log stores ids, outcomes and reasons,
never personal-data values. Its API/UI trace tree includes failures and skips.
Client `messageId` dedupe gives effectively-once event storage. Late and future
events are explicit; time-window memberships are recomputed. Replays do not
trigger flows unless separately chosen. Event forwarding is at-least-once with
stable keys, visible retries/dead letters and lag metrics.

### Consequences
Investigation is deterministic across ingestion, sync, segmentation, flows,
delivery and forwarding. Every new derived action must add a decision type,
propagate trace and request ids, and extend erasure. Event-name retention,
rejects, decisions and forwarding queues become privacy-managed data. The
extra writes and partitions consume Postgres capacity.

### Reversal evidence
If measured partitioning and indexing cannot meet the documented debugger,
explorer or decision-write targets without harming ingestion, move the
affected append or query workload behind the same API to an analytical add-on.
Do not collapse the four record types or weaken trace semantics.

## ADR-008 — Draft-only LLM assistance over typed tools
### Context
Natural language can shorten authoring and explanation, but model output is
nondeterministic, may follow instructions hidden in product data and may expose
personal data. It must not bypass validation, workspace RBAC or human publish.

### Options
1. Ship no LLM assistance.
2. Add feature-specific prompts whose text output writes objects directly.
3. Put configured providers behind one Rust trait and expose only shared typed
   read-and-draft tools, with validation, diff review and explicit acceptance.

### Decision

Choose option 3. Support OpenAI-compatible APIs, including allowlisted local
base URLs, and the Anthropic API. Deployment config permits providers/models;
workspace settings choose enablement, provider/model, budget and PII opt-in.
Keys use envelope encryption and calls use the egress policy.

Tools draft segment ASTs, flow graphs, template documents, source mappings and
HTTP manifests, and explain segments, flows and traces. Schemas come from the
same Rust API types. Existing validators drive bounded repair; the UI shows
diffs and previews. No tool publishes, sends, deletes or changes consent.
Every call runs as the user in one workspace and is audited. Context is
schema-only by default; model output and supplied data are always untrusted.

A golden-prompt harness runs manually or nightly. CI uses a deterministic fake
provider and never calls an LLM. The same read-and-draft layer may later be
exposed through MCP with the same RBAC and audit.

### Consequences

Provider changes cannot redefine product artifacts, and assistant paths remain
deterministically testable. Zirv Flow gains outbound HTTP dependencies, cost
and rate controls, prompt-injection surface, audits and transcript retention.
It does not host, train or fine-tune models and performs no autonomous action.

### Reversal evidence

Disable or remove providers if evaluations and usage show no sustained task
benefit at acceptable cost and latency, required capabilities are unavailable
to self-hosters, or security tests cannot contain prompt injection or PII
disclosure. Typed drafts and deterministic validators remain.
