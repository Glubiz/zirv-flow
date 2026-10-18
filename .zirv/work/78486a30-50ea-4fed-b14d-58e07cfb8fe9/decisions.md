# zirv-flow-engine (#156): decisions

- Hash: first 8 bytes (big endian) of SHA-256 over `enrollment_id` bytes (16) then the UTF-8 node id, taken `% sum(weights)`; SHA-256 is specified, so it is stable across platforms and releases (std `DefaultHasher` is neither). Pinned by a test.
- Step key: `{enrollment_id}:{node_id}`; external effect key: `{step_key}:webhook`; wake key: `wake:{enrollment_id}:{state_version}` (architecture 9.3).
- DST gap (local time does not exist): read with the offset in force before the jump (02:30 in a 02:00 to 03:00 gap is the instant 03:30 shows). Reason: keeps the wait the intended length after the wall-clock and never lands before the requested time.
- DST overlap (local time twice): the first occurrence. Reason: the earlier instant is what "at 02:30" means to someone who has not yet seen the clocks fall back.
- Time of day: the next occurrence strictly after `now` (14 days look-ahead for weekday filters); a fixed date and time in the past is due at once.
- Zone: the person's `timezone` trait if it parses as an IANA name, else the workspace zone; the invalid name is returned in `Transition::zone_fallback` for the step decision. The context hands the person zone over as text so the engine, not the caller, decides validity.
- State fields: `enrollment_id`, `node` (next node to run, or where it stopped), `state_version`, `status` (active, waiting, completed, exited, failed), `wake_at`, `awaiting` (event name). Context ids are the runtime's concern (12.4), not the engine's.
- Version: every non-stale `step` returns `state_version + 1`; a timer or event input carries the version the wait was registered at (`wake_version`) and any other value is a `Stale` outcome with the state unchanged (the first commit wins). Version match with the wrong status is an `InvalidState` error.
- Branch: yields `NeedsConditions { node, conditions }` with state at the branch (status active, version + 1) and the effects so far; `ConditionsEvaluated` carries one bool per condition, the first true wins, else `else`. A wrong node or result count is `InputMismatch`.
- Resume: only from `failed`; retry re-runs the failed node (its earlier effects are not repeated), skip leaves it by `next` (refused for nodes without that port).
- Failures vs errors: content faults of one node (unparseable time, duration beyond the clock, split of zero weight) are `Outcome::Failed` so an operator can retry or skip; structural faults (unknown node, missing edge, cycle, no entry) are `EngineError`.
- Cycle guard: a walk longer than the node count is `EngineError::Cycle`; step never panics.
- Entry node: the first node no edge leads to (`EnrollmentState::new`).
- Effects: one ordered list; `Effect::is_external()` (has an effect key) splits internal (trait, list, goal conversion) from external (webhook). The webhook effect carries url and trait keys; the envelope is built by the runtime.
- `Visit` rows (node, step key) are returned for `step_executions` and the test-mode path.
- Dependencies: chrono (serde feature, for the persisted state), chrono-tz, serde, serde_json, sha2, uuid, core; proptest dev only. hashbrown is not used because nothing needs a map.
