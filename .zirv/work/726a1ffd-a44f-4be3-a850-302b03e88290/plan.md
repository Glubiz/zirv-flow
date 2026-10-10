# Plan

1. A Sonnet worker implements the spec. Codex is rate-limited until about 00:57.
   - The work covers core, store and api, plus the binary CLI, tests, the
     regenerated `openapi.json`, docs and the version bump.
   - Every new file gets `git add -N`.
2. Verify independently:
   - the four checks;
   - crate-deps;
   - the ignored tests on PostgreSQL 16 and 18;
   - a manual acceptance flow with curl.
3. Run simplify, then two independent reviews of the full diff, including a
   security-focused one. Fix what they confirm.
4. Commit, run test-presence on the committed tree, push, and open a PR that
   closes #6.
