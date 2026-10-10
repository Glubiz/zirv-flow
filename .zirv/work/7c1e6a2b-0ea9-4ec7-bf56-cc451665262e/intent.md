# Intent

## Problem

Zirv Flow must have best-in-class event logging and must support LLMs for
segmentation, flow building, template building and related authoring. The
architecture (`docs/architecture.md`) has person events, an audit log and a
per-message gate log, but no event schema governance, investigation tooling or
end-to-end causality, and no LLM support at all.

## Desired outcome

`docs/architecture.md` and `docs/roadmap.md` (PR #1, branch `chore/repo-setup`)
describe, per rulings aca769a3, 3290630c and 2dfb3ca4:

- Event logging: a typed customer-event pipeline (versioned schemas,
  allow/warn/block validation, property PII flags, visible rejects, live
  debugger, explorer, unified person timeline) and an end-to-end causality log
  (every derived action traceable to its cause, in UI and API), with delivery
  guarantees, duplicate/late handling, replay controls, privacy, and event
  forwarding to destinations. Customer events, audit records and decision logs
  stay distinct but linked by durable ids.
- LLM assistance: an optional provider layer (OpenAI-compatible incl. local
  servers, Anthropic) that drafts the product's own typed artifacts through
  validated tool calls; draft-only, RBAC per tool call, untrusted output,
  schema-only context by default, audited, budgeted; MCP exposure later.
- Placement woven into existing milestones without renumbering.

## Constraints

- Docs only; GitHub milestone descriptions stay in sync with the roadmap.
- ADR-001..006 stand. No other marketing- or workflow-automation product or
  company is named.
- Per-milestone rules hold: API and UI together, workspace isolation, erasure
  coverage, deterministic core testable in CI (LLM calls never in CI gates).

## Open questions

None; scope and placement ruled by the supervisor.

## Acceptance criteria

- [ ] Architecture covers both event-logging layers and the LLM layer (provider
      abstraction, tools, guardrails, privacy, audit, budgets, evaluation),
      with ADRs and updated goals, non-goals, crate layout, domain model,
      security, testing and open questions.
- [ ] Roadmap milestones M1-M7 carry the woven scope and observable exit
      criteria; GitHub milestone descriptions match.
- [ ] No contradictions or broken cross-references; no other automation
      product names; PR #1 CI green.
