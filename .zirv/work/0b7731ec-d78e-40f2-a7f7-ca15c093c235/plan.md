# Plan

1. One worker implements the spec: deps in `[workspace.dependencies]`, `cli.rs`,
   `config.rs`, a thin `main.rs`, the README section and the version bump. Inline
   tests cover every acceptance criterion.
2. Verify independently: build, nextest, fmt, clippy, crate-deps,
   test-presence, `config check` by hand with a password in the URL, and
   `--help`.
3. Simplify, then two independent reviews. Fix what is confirmed.
4. Commit, push and open a PR against `main` that closes #3.
