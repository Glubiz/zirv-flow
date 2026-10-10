# Plan

1. Workspace manifests and crate moves -> verify: four gates green.
2. `scripts/check-crate-deps.sh` + self-test -> verify: self-test passes,
   real check passes on the workspace.
3. CI wiring and path filters; test-presence `lib.rs` exemption with self-test
   case -> verify: all script self-tests pass.
4. Agent context module map, regenerate -> verify: `zirv context sync --check`.
