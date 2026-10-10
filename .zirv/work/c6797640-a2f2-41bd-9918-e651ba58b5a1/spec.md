# Spec: workspace layout and crate checks (issue #2)

- Root `Cargo.toml` becomes a virtual manifest: `[workspace] members =
  ["crates/*"]`, `resolver = "3"`, `[workspace.package]` (version 0.2.0,
  edition, authors, license, repository), `[workspace.dependencies]` for shared
  deps, `[profile.release]` moved here. Member crates inherit with
  `*.workspace = true`.
- `crates/zirv-flow` holds today's `src/main.rs` and its tests unchanged in
  behaviour. Edges: bin -> api, jobs; api -> core, store; jobs -> core, store;
  store -> core. Library crates start with a crate doc comment only.
- `scripts/check-crate-deps.sh` (bash + jq, `--self-test`): reads `cargo metadata
  --no-deps` and the 5.1 table from `docs/architecture.md`; fails listing every
  workspace edge not allowed and every crate missing from the table; purity:
  `cargo tree -e normal` of each pure crate present must not contain sqlx,
  reqwest/hyper/ureq clients or tokio with the `net` feature.
- CI `test` job runs the check and its self-test before the build.
- Path filters: version-bump `^crates/` alongside `^src/`, test-presence scans
  `crates/*/src/**/*.rs` and exempts `lib.rs` like `mod.rs` (crate-root glue).
- `.zirv/context/common.md` module map updated; CLAUDE.md/AGENTS.md regenerated.
