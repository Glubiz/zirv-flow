#!/usr/bin/env bash
# Benchmark-feature gate. The `bench-no-decisions` feature of zirv-flow-store turns decision
# appends into no-ops, for the decision-overhead benchmark only. The store refuses to compile it
# unless ZIRV_FLOW_BENCH_BUILD is set, so `--all-features`, aliases and default lists cannot enable
# it by accident; this gate checks the two things left:
#
#   1. the guard works: `cargo check --features bench-no-decisions` without the variable must FAIL
#      with the guard's message;
#   2. neither the variable nor the feature name appears on a non-comment line of a shipped-build
#      input: any workflow but ci.yaml, a Dockerfile, a compose file, or a script those invoke.
#
# Usage: check-bench-features.sh [ROOT]   |   check-bench-features.sh --self-test
# Exits 1 listing the offences, 2 on a setup error. Requires bash, grep and (for 1) cargo.

set -euo pipefail

scan() {
  cd "$1"
  seeds="$(
    {
      find .github/workflows -maxdepth 1 -type f \( -name '*.yaml' -o -name '*.yml' \) ! -name ci.yaml 2>/dev/null
      find . -maxdepth 3 -type f \( -name 'Dockerfile*' -o -name 'docker-compose*' -o -name 'compose*.y*ml' \) \
        -not -path './node_modules/*' -not -path './target/*' -not -path './.git/*' -not -path './.zirv/*' \
        -not -path './.claude/*'
    } | sed 's|^\./||' | sort -u
  )"
  queue=()
  declare -A seen=()
  while IFS= read -r f; do [ -z "$f" ] || { seen[$f]=1; queue+=("$f"); }; done <<<"$seeds"
  for ((i = 0; i < ${#queue[@]}; i++)); do   # the queue grows as scripts are discovered
    while IFS= read -r token; do
      token="${token#./}"
      [ -f "$token" ] && [ -z "${seen[$token]+x}" ] && { seen[$token]=1; queue+=("$token"); }
    done < <(grep -v '^[[:space:]]*#' "${queue[$i]}" | grep -oE '[A-Za-z0-9_./-]+\.sh' || true)
  done
  [ "${#queue[@]}" -eq 0 ] && return 0
  grep -nHE 'ZIRV_FLOW_BENCH_BUILD|bench-no-decisions' -- "${queue[@]}" \
    | grep -Ev '^[^:]+:[0-9]+:[[:space:]]*#' || true
}

run_gate() {
  hits="$(scan "$1")"
  if [ -n "$hits" ]; then
    echo "benchmark-feature gate: ZIRV_FLOW_BENCH_BUILD and bench-no-decisions must not appear in a shipped build input:" >&2
    printf '%s\n' "$hits" | sed 's/^/  - /' >&2
    return 1
  fi
}

run_guard_check() {
  if output="$(cd "$1" && env -u ZIRV_FLOW_BENCH_BUILD cargo check --quiet -p zirv-flow-store \
      --features bench-no-decisions 2>&1)"; then
    echo "benchmark-feature gate: bench-no-decisions compiled without ZIRV_FLOW_BENCH_BUILD" >&2
    return 1
  fi
  if [[ "$output" != *"is for benchmark builds only"* ]]; then
    echo "benchmark-feature gate: the check failed, but not with the guard's message:" >&2
    printf '%s\n' "$output" | tail -n 15 >&2
    return 1
  fi
}

run_self_test() {
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  failures=0
  expect() {   # name, wanted exit, mutation applied to a fresh clean tree
    dir="$work/.claude/worktrees/$1"
    mkdir -p "$dir/.github/workflows" "$dir/scripts"
    printf 'FROM rust\nRUN cargo build --release -p zirv-flow\n' >"$dir/Dockerfile"
    printf 'steps:\n  - run: bash scripts/build.sh\n' >"$dir/.github/workflows/cd.yaml"
    printf '#!/usr/bin/env bash\ncargo build --release\n' >"$dir/scripts/build.sh"
    printf 'services: {}\n' >"$dir/docker-compose.yml"
    printf 'steps:\n  - run: ZIRV_FLOW_BENCH_BUILD=1 cargo clippy --features bench-no-decisions\n' \
      >"$dir/.github/workflows/ci.yaml"
    (cd "$dir" && eval "$3")
    set +e; run_gate "$dir" >/dev/null 2>&1; got=$?; set -e
    if [ "$got" -eq "$2" ]; then echo "self-test ok: $1 (exit $got)"
    else echo "self-test FAILED: $1 (expected exit $2, got $got)" >&2; failures=$((failures + 1)); fi
  }
  # Every fixture lives under .claude/worktrees/, which must still be scanned.
  expect "clean tree passes (ci.yaml may use both)" 0 true
  expect "a comment is not a use" 0 'echo "# never set ZIRV_FLOW_BENCH_BUILD" >>Dockerfile'
  expect "the variable in a Dockerfile fails" 1 'echo "ENV ZIRV_FLOW_BENCH_BUILD=1" >>Dockerfile'
  expect "the variable in a CD-invoked script fails" 1 'echo "ZIRV_FLOW_BENCH_BUILD=1 cargo build" >>scripts/build.sh'
  expect "the variable in a nested script fails" 1 \
    'echo "bash scripts/inner.sh" >>scripts/build.sh; echo "export ZIRV_FLOW_BENCH_BUILD=1" >scripts/inner.sh'
  expect "the feature name in the compose file fails" 1 'echo "# x" >>docker-compose.yml; echo "  command: --features bench-no-decisions" >>docker-compose.yml'
  expect "the feature name in the CD workflow fails" 1 'echo "  - run: cargo build --features bench-no-decisions" >>.github/workflows/cd.yaml'
  [ "$failures" -eq 0 ] || { echo "check-bench-features.sh --self-test: $failures scenario(s) failed" >&2; return 1; }
  echo "check-bench-features.sh --self-test: all scenarios passed"
}

case "${1:-}" in
  --self-test) run_self_test ;;
  -*) echo "usage: $0 [ROOT | --self-test]" >&2; exit 2 ;;
  *)
    root="${1:-$(git rev-parse --show-toplevel 2>/dev/null)}" || { echo "not inside a git repository" >&2; exit 2; }
    run_gate "$root"
    run_guard_check "$root"
    ;;
esac
