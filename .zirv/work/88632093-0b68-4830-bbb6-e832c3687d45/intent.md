# Intent

## Problem

The architecture (`docs/architecture.md`) treats templates as hand-written
HTML/text with MiniJinja and lists a drag-and-drop email designer as a v1
non-goal; SMS, push and in-app channels are post-1.0 themes. Marketers need to
build message content for each notification channel without writing HTML, and
the design has no template builder.

## Desired outcome

`docs/architecture.md` and `docs/roadmap.md` (on PR #1, branch
`chore/repo-setup`) describe a template builder for the notification channels:

- the template content model (channel-aware, versioned, reusable parts),
- the builder UI and how content is rendered per channel (preview equals send),
- personalization against the field registry, validation and compliance rules
  (unsubscribe, sender identity, channel length limits),
- which channels the builder covers in v1 and which follow,
- where it lands in the milestones, with exit criteria,
- a decision record for the builder approach.

GitHub milestones stay identical to the roadmap headings.

## Constraints

- No product code; docs and GitHub milestones only.
- No other marketing- or workflow-automation product or company is named.
- Existing decisions (ADR-001..005) stand; templates stay sandboxed MiniJinja.
- Architecture conventions hold: API and UI per milestone, workspace isolation,
  erasure coverage, deterministic rendering testable in CI.

## Open questions

- Builder approach, v1 channel scope and milestone placement are routed to the
  supervisor.

## Acceptance criteria

- [ ] Architecture has a template builder section covering model, editor,
      rendering per channel, personalization, validation, versioning and
      storage; non-goals and section 10 no longer contradict it.
- [ ] A new ADR records the builder approach with options and reversal evidence.
- [ ] Roadmap places the work in a milestone with scope, out of scope, exit
      criteria and dependencies; the overview table and dependency graph match.
- [ ] GitHub milestone titles and descriptions match the roadmap headings.
- [ ] No other automation product names; PR #1 CI stays green.
