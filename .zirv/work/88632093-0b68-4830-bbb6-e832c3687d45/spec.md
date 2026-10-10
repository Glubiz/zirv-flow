# ADR-006: Template builder

Status: **Accepted**. It extends the Zirv Flow architecture
([architecture](../../../docs/architecture.md), section 11 for the design and
section 18 for the decision in context). ADR-001 to ADR-005 are unchanged.
Zirv Flow is a self-hosted marketing automation tool: one Rust binary plus
PostgreSQL, in which marketers connect data, segment people and run flows that
send messages. This record holds three rulings: the builder approach, the v1.0
channel scope and the milestone placement. None of them is reopened here.

## Context

- Marketers need to build message content for each channel without writing HTML. Until now the design offered only hand-written MiniJinja HTML and text.
- Email HTML has to render across mail clients with very uneven CSS support, so responsive layout must come from proven markup rather than ad hoc HTML.
- Merge fields, unsubscribe links and sender identities must be checked at publish against Zirv Flow's own field registry and consent model.
- Preview must show exactly what will be sent, and rendering must be deterministic so golden tests can pin it.
- The baseline remains the binary plus PostgreSQL (ADR-003), so rendering runs in Rust on the server, and the UI stays the embedded React SPA (ADR-002).

## Options considered

1. **Zirv Flow's own channel-aware block document with a server-side Rust renderer.** Chosen.
2. **Embed an open-source JavaScript email editor and store the HTML it produces, with form editors for other channels.**
   - It is the fastest start.
   - The stored HTML is opaque to merge-field and compliance checks.
   - Every channel gets a different content model.
   - The editor's format and output become dependencies Zirv Flow does not control.
3. **Code-only templates (the status quo).**
   - There is nothing new to maintain.
   - Marketers have to hand-write responsive email HTML, which is the problem being solved.

## Decision

- **Document and editor.** The content is a versioned JSON block document that is channel-aware, and the React builder edits it.
- **Email rendering.** Rust renders email blocks to MJML, then to responsive HTML through the `mrml` crate, and generates a text part.
- **Other channels.** Webhook content, and later SMS, push and in-app content, renders from typed content schemas.
- **Code mode.** Raw HTML and text with MiniJinja remains as an escape hatch. Conversion is one-way: blocks can become code, and code never becomes blocks.
- **Preview.** Preview uses the same renderer as send.
- **First release.** It stays narrow: a small email block set, typed forms for the other channels, and explicit document schema versions.
- **Layout validation.** Representative email layouts are validated through `mrml` before the initial block set is committed. This is an exit criterion of M7.
- **v1.0 channel scope.** v1.0 covers email and webhook payloads only. The content model is channel-agnostic and defines SMS, push and in-app content types from day one, reserved in the schema. Their builders ship with those channels after 1.0.
- **Milestone placement.** A new milestone, `M7 — Template builder`, follows M6. `M7 — Beta hardening and v1.0` becomes `M8 — Beta hardening and v1.0` and depends on M3 to M7. `M8 — Post-1.0 themes` becomes `M9 — Post-1.0 themes`, and its themes are promoted to M10, M11 and so on. M6 keeps the code-template renderer, the send pipeline and destinations, and M7 builds on them.

## Consequences

- **Accepted cost.** Zirv Flow owns the editor and the document schema migrations. Migrations are pure functions with golden tests. Published versions are never rewritten.
- **Structured content.** Publish-time validation, merge-tag checks against the registry, and per-block conditions on the segment AST all become possible. So does a preview-equals-send test.
- **`mrml` dependency.** `mrml` becomes a key dependency, so its MJML gaps become Zirv Flow's.
- **Block growth.** Every block added is a permanent migration commitment, so the block set grows only under an explicit policy.
- **Escape hatches.** The raw HTML block and code mode escape most structural checks. Parsing, size limits and the required unsubscribe link still apply. They are never rendered unsandboxed in the app.
- **New pure crate.** `zirv-flow-templates` is a pure renderer (no I/O) used by the API for preview and by the jobs crate for sends.

## Reversal evidence

- **Block requests outpace delivery.** Requests for new blocks consistently outpace delivery, so marketers fall back to code mode for common layouts.
- **Client compatibility fails.** `mrml` output fails client compatibility for representative layouts, and the failure cannot be fixed upstream or worked around in the block set.
- **Editor cost.** Maintaining the editor and its migrations measurably crowds out the segmentation, data source and integration work the product is focused on.
