# Plan

1. A Codex worker implements the spec: the store crate, binary startup, the
   `migrate` command, tests, the CI job, docs and the version bump. It runs the
   PostgreSQL tests against Docker containers for 16 and 18.
2. Run `git add -N` on every new file before any review.
3. Verify independently:
   - the full four checks, crate-deps and test-presence;
   - the ignored tests against PG 16 and 18 containers;
   - `zirv-flow migrate` and role startup by hand (newer, pending and dirty
     cases), plus a wrong-password connect.
4. A simplify pass, then two independent reviews of the full diff, each with the
   diff embedded. Fix what is confirmed.
5. Commit, push and open a PR closing #4. CI runs green on both PostgreSQL versions.
