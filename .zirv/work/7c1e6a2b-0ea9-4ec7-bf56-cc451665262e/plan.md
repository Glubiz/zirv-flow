# Plan

Docs-only change on PR #1 (`chore/repo-setup`), written by a Codex worker per
rulings aca769a3 (event logging scope), 3290630c (LLM approach) and 2dfb3ca4
(placement, no renumbering).

1. `docs/architecture.md`: new sections 12 (event logging and traceability) and
   13 (LLM assistance); later sections renumbered to 14-21 with all references;
   goals, non-goals, crate layout (`zirv-flow-ai`), domain model, API/UI,
   security, testing, targets, ADR-007/008, OQ-26..30.
   Verify: every section/anchor/ADR/OQ reference resolves.
2. `docs/roadmap.md`: woven scope and exit criteria in M1-M9; no renumbering.
3. `spec.md`: ADR-007 and ADR-008 decision record.
4. GitHub milestone descriptions regenerated from the roadmap.
   Verify: titles equal the `## M` headings.
5. No other automation product names; PR #1 CI green.
