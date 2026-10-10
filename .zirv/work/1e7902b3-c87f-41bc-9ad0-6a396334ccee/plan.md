# Plan

Codex is rate-limited, so the work runs on Sonnet workers.

1. A Sonnet worker implements the spec: the api crate, binary wiring, tests, the committed `openapi.json`, docs and the version bump. It runs `git add -N` on every new file.
2. Verify independently:
   - the four checks, crate-deps and test-presence;
   - the ignored tests against PostgreSQL 16 and 18 containers;
   - by hand, `serve` with curl on `/healthz`, `/readyz`, `/metrics`, `/api/v1/version` and an unknown path, then Ctrl-C;
   - the drift check: change an annotation and see the test fail.
3. Run a simplify pass, then two independent reviews of the full diff. Fix whatever they confirm.
4. Commit, push and open a PR closing #5.
