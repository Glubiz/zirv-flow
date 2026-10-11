#!/usr/bin/env bash
# Benchmark-feature gate: the `bench-no-decisions` cargo feature of zirv-flow-store turns
# decision-log appends into no-ops and exists only for the decision-overhead benchmark. It must
# never reach a shipped build, so this fails when the feature name appears in the CD workflow or
# any other workflow except ci.yaml, in a Dockerfile or compose file, in a Cargo manifest other
# than the store's own definition, in a `default = [...]` list, in a cargo config, or on a
# `--release` line of ci.yaml.
#
# Usage:
#   check-bench-features.sh [ROOT]
#   check-bench-features.sh --self-test
#
# Exits 1 and lists every offending line. Requires bash and grep.

set -euo pipefail

needle='bench-no-decisions'
store_manifest='crates/zirv-flow-store/Cargo.toml'

# Prints "file:line:text" for each non-comment line of $1 containing the feature name.
mentions() {
  grep -nH -- "$needle" "$1" 2>/dev/null \
    | grep -Ev '^[^:]+:[0-9]+:[[:space:]]*#' || true
}

run_gate() {
  root="$1"
  hits=""

  # Shipped-build inputs: any mention is a violation.
  files=()
  while IFS= read -r file; do files+=("$file"); done < <(
    {
      find "$root/.github/workflows" -maxdepth 1 -type f \( -name '*.yaml' -o -name '*.yml' \) \
        ! -name 'ci.yaml' 2>/dev/null
      find "$root" -maxdepth 3 -type f \( -name 'Dockerfile*' -o -name 'docker-compose*.yml' \
        -o -name 'docker-compose*.yaml' -o -name 'compose*.yaml' -o -name 'compose*.yml' \) \
        -not -path '*/node_modules/*' -not -path '*/target/*' -not -path '*/.git/*' \
        -not -path '*/.zirv/*' -not -path '*/.claude/*'
      find "$root" -maxdepth 3 -path '*/.cargo/config*' -type f
      find "$root" -name Cargo.toml -type f \
        -not -path '*/node_modules/*' -not -path '*/target/*' -not -path '*/.git/*' \
        -not -path '*/.zirv/*' -not -path '*/.claude/*' \
        -not -path "$root/$store_manifest"
    } | sort -u
  )
  for file in "${files[@]}"; do
    found="$(mentions "$file")"
    [ -z "$found" ] || hits+="$found"$'\n'
  done

  # The store's own manifest may define the feature but never make it a default.
  if [ -f "$root/$store_manifest" ]; then
    found="$(grep -nH -E "^[[:space:]]*default[[:space:]]*=.*$needle" "$root/$store_manifest" || true)"
    [ -z "$found" ] || hits+="$found"$'\n'
  fi

  # ci.yaml may run the feature (a clippy pass) but never in a release build.
  if [ -f "$root/.github/workflows/ci.yaml" ]; then
    found="$(mentions "$root/.github/workflows/ci.yaml" | grep -- '--release' || true)"
    [ -z "$found" ] || hits+="$found"$'\n'
  fi

  if [ -n "$hits" ]; then
    echo "benchmark-feature gate: '$needle' must not reach a shipped build:" >&2
    printf '%s' "$hits" | sed 's/^/  - /' >&2
    return 1
  fi
  return 0
}

run_self_test() {
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  failures=0

  fixture() {
    dir="$work/$1"
    mkdir -p "$dir/.github/workflows" "$dir/crates/zirv-flow-store" "$dir/crates/zirv-flow"
    printf '[features]\n%s = []\n' "$needle" >"$dir/$store_manifest"
    printf '[dependencies]\nzirv-flow-store.workspace = true\n' >"$dir/crates/zirv-flow/Cargo.toml"
    printf 'FROM rust\nRUN cargo build --release -p zirv-flow\n' >"$dir/Dockerfile"
    printf 'steps:\n  - run: cargo build --release\n' >"$dir/.github/workflows/cd.yaml"
    printf 'steps:\n  - run: cargo clippy -p zirv-flow-store --features %s\n' "$needle" \
      >"$dir/.github/workflows/ci.yaml"
    printf '%s' "$dir"
  }

  expect() {
    name="$1" want="$2" dir="$3"
    set +e
    run_gate "$dir" >/dev/null 2>&1
    got=$?
    set -e
    if [ "$got" -eq "$want" ]; then
      echo "self-test ok: $name (exit $got)"
    else
      echo "self-test FAILED: $name (expected exit $want, got $got)" >&2
      failures=$((failures + 1))
    fi
  }

  expect "clean tree passes" 0 "$(fixture clean)"

  dir="$(fixture dockerfile)"
  printf 'RUN cargo build --release --features zirv-flow-store/%s\n' "$needle" >>"$dir/Dockerfile"
  expect "feature in the Dockerfile fails" 1 "$dir"

  dir="$(fixture cd)"
  printf '  - run: cargo build --release --features zirv-flow-store/%s\n' "$needle" \
    >>"$dir/.github/workflows/cd.yaml"
  expect "feature in the CD workflow fails" 1 "$dir"

  dir="$(fixture commented)"
  printf '# never pass %s here\n' "$needle" >>"$dir/Dockerfile"
  expect "a comment is not a use" 0 "$dir"

  dir="$(fixture default)"
  printf '[features]\ndefault = ["%s"]\n' "$needle" >"$dir/$store_manifest"
  expect "feature in the store's defaults fails" 1 "$dir"

  dir="$(fixture dependent)"
  printf 'zirv-flow-store = { workspace = true, features = ["%s"] }\n' "$needle" \
    >>"$dir/crates/zirv-flow/Cargo.toml"
  expect "feature enabled by a dependent manifest fails" 1 "$dir"

  dir="$(fixture ci-release)"
  printf '  - run: cargo build --release --features zirv-flow-store/%s\n' "$needle" \
    >>"$dir/.github/workflows/ci.yaml"
  expect "feature in a CI release build fails" 1 "$dir"

  if [ "$failures" -gt 0 ]; then
    echo "check-bench-features.sh --self-test: $failures scenario(s) failed" >&2
    return 1
  fi
  echo "check-bench-features.sh --self-test: all scenarios passed"
}

case "${1:-}" in
  --self-test) run_self_test ;;
  -*)
    echo "usage: $0 [ROOT | --self-test]" >&2
    exit 2
    ;;
  *)
    root="${1:-}"
    if [ -z "$root" ]; then
      root="$(git rev-parse --show-toplevel 2>/dev/null)" || {
        echo "check-bench-features.sh: not inside a git repository" >&2
        exit 2
      }
    fi
    run_gate "$root"
    ;;
esac
