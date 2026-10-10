# Intent

## Problem

The operator wants Zirv Flow built in modules so the codebase stays clean. The
architecture lists crates (section 5) but no enforced dependency rules, no
module conventions inside crates, no extension-point pattern, and no frontend
module structure; the agent instructions every coding session follows say
nothing about modularity.

## Desired outcome

Per ruling 8c36f93b (modular monolith, layered crates kept):

- `docs/architecture.md` section 5 defines the allowed crate-dependency table
  (enforced in CI), feature modules inside each crate with a narrow
  `pub(crate)`-by-default surface, trait + registry extension points
  (connectors, destinations/channels, template content types, job kinds, AI
  tools), and matching `web/` feature folders with lint-enforced import
  boundaries; a principle and ADR-009 record it.
- `docs/roadmap.md` M1 builds the enforcement (dependency check, frontend
  boundary lint) with exit criteria; every-milestone rules require it.
- `.zirv/context/common.md` (and regenerated `CLAUDE.md`/`AGENTS.md`) and
  `CONTRIBUTING.md` state the module rules agents and contributors follow.

## Constraints

- Docs and agent context only; the checks themselves are built in M1 (no
  workspace exists yet). No product code.
- ADR-001..008 stand; no milestone renumbering; no other automation product
  names; `zirv context sync --check` and PR #1 CI stay green.

## Open questions

None.

## Acceptance criteria

- [ ] Section 5 holds the dependency table, module conventions, registries and
      frontend structure; section 2 and ADR-009 record the decision.
- [ ] M1 scope and exit criteria cover CI enforcement; GitHub milestone
      description matches.
- [ ] Agent context and CONTRIBUTING carry the module rules; generated files
      in sync.
- [ ] All cross-references resolve; CI green.
