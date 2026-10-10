# Plan

Docs-only change on PR #1 (`chore/repo-setup`). Rulings: builder approach
a90e4660, v1.0 channel scope 877e7ea0, placement d693a10d.

## Tasks

1. `docs/architecture.md`: new section 11 "Templates and the builder"; renumber
   sections 12-19 and every cross-reference; update goals, non-goals, 10.2,
   crate layout (`zirv-flow-templates`), domain model, security, erasure,
   API/UI, testing, scalability target, ADR-006, OQ-21..25.
   Verify: every `section N` / `(N.M)` reference resolves to the renumbered
   heading; anchor `#18-key-decisions` exists.
2. `docs/roadmap.md`: add `M7 — Template builder`; renumber beta hardening to
   M8 and post-1.0 themes to M9 in headings, overview table, Mermaid graph,
   dependencies and OQ decide-by references.
   Verify: milestone number/name pairs are consistent across both docs.
3. `.zirv/work/88632093-.../spec.md`: ADR-006 decision record.
4. GitHub milestones: create M7, rename #8 to M8 and #9 to M9, refresh every
   description from the roadmap.
   Verify: sorted milestone titles equal the sorted `## M` headings.
5. No other automation product names in the changed files; PR #1 CI green.
