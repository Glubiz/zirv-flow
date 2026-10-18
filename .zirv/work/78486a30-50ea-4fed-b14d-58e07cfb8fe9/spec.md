# Spec: #156

The scope is exactly GitHub issue #156, and `docs/architecture.md` sections 9.2 and 9.3.

- **The function:** `step(&FlowGraph, &EnrollmentState, StepInput, &dyn StepContext) -> Result<Transition, EngineError>`.
- **`StepInput`:**
  - `Start`;
  - `TimerDue { wake_version }`;
  - `EventArrived { event }`;
  - `ConditionsEvaluated { node, results }`;
  - `Resume { skip }`.
- **`StepContext`:** `now`, the workspace zone and the person's zone.
- **`Transition`:**
  - the new state, with `state_version + 1`;
  - the ordered effects: internal (trait update, list change, goal conversion) and external (webhook);
  - the wake;
  - the waits to register or clear;
  - an outcome: `Waiting`, `NeedsConditions`, `Completed`, `Exited { reason }` or `Failed`.
- **Advancing:** one call advances through consecutive non-blocking nodes and stops at:
  - a wait;
  - a branch yield;
  - an exit or goal-exit;
  - the end of the graph.
- **Determinism:**
  - effect keys are derived from `(enrollment_id, node_id)`;
  - the `split.random` hash is stable across platforms and releases;
  - a documented DST rule for `wait.until_time`.
- **Stale inputs:** a stale `wake_version` or a mismatched input is a typed no-op or error. It never panics.
