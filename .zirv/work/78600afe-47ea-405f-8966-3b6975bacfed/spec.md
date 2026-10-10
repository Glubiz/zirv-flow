# ADR-009: Modular monolith with enforced boundaries

**Status:** Accepted

## Context

Zirv Flow ships as one binary, while its domain areas and external integrations
change for different reasons. Informal boundaries would allow I/O to leak into
deterministic code, feature internals to become accidental APIs and extension
dispatch to spread across the codebase.

## Options

1. Layered crates, vertical feature modules and exactly five trait-backed
   implementation registries, with boundaries enforced in CI.
2. Vertical bounded-context crates over a shared kernel.
3. One crate with modules only.

## Decision

Choose option 1. Keep the layered crates and direct-dependency allowlist in
[architecture section 5](../../../docs/architecture.md#5-module-and-crate-layout).
Within crates, domain features expose narrow `mod.rs`/`lib.rs` surfaces and use
`pub(crate)` by default. The five registries are connectors (`Source`),
destinations and channels (`Destination`), template content types, job kinds and
AI tools. The frontend mirrors domain features behind `index.ts` boundaries.

From M1, CI rejects workspace edges outside the allowlist, I/O dependencies in
pure crates, cross-feature deep imports and feature imports from the app shell.

## Consequences

The product remains one deployable binary while dependency direction and module
ownership are machine-checked. A new extension is one implementation module and
one registry line. Legitimate cross-feature work must use an explicit supported
surface; a boundary change requires an architecture decision.

## Reverse if

Introduce a bounded-context crate only when a domain area demonstrates
independent ownership and a stable dependency boundary, or repeated legitimate
changes keep forcing dependency-table edits. Extract it over the smallest shared
kernel and supersede this decision; do not remove boundary enforcement.
