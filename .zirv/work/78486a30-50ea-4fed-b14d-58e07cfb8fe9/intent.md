# Intent: #156, the flow engine `step` interpreter

Flows need a deterministic, pure interpreter that moves an enrollment through a published graph. The runtime (#159) persists its transitions and executes their effects. The test mode (#164) and the simulation (#157) call it without I/O.

**Outcome:** a new pure crate, `zirv-flow-engine`, depending on `zirv-flow-core` only. Its `step(graph, state, input, context)` returns the next state, the ordered effects (with idempotency keys), the wake and waits, and an outcome.

**Rulings that bind it (#154):**
- a branch yields `NeedsConditions`, and the runtime evaluates them in the same transaction;
- the webhook effect is a fixed signed envelope;
- the local time is the person's zone, else the workspace's.

**Non-goals:**
- persistence, timers, HTTP and condition evaluation;
- send and sync nodes, which come in M6.
