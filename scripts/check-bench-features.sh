#!/usr/bin/env bash
# Test-only feature gate. Two features exist for benchmarks and tests only: `bench-no-decisions` of
# zirv-flow-store (decision appends become no-ops, for the decision-overhead benchmark) and
# `browser-test` of zirv-flow (the test server of the Playwright tests, #315). Each refuses to
# compile unless its build variable is set (ZIRV_FLOW_BENCH_BUILD, ZIRV_FLOW_BROWSER_TEST_BUILD),
# so `--all-features`, aliases and default lists cannot enable one by accident; this gate checks
# what is left:
#
#   1. the guards work: `cargo check --features bench-no-decisions` without its variable must FAIL
#      with the guard's message, and so must the browser-test build without its variable;
#   2. neither variable nor feature name appears on a non-comment line of a shipped-build input:
#      any workflow but ci.yaml, a Dockerfile, a compose file, cargo config (.cargo/config.toml,
#      .cargo/config), or a script those invoke (any scripts/... path, whatever its suffix);
#      benchmark script/data/tool paths are forbidden in the same inputs;
#   3. no Cargo.toml lists `browser-test` among its default features;
#   4. the server binary of a default build has no `/__test/` route in it.
#
# Usage: check-bench-features.sh [ROOT | --static-only [ROOT] | --self-test]
# --static-only checks shipped inputs without compiling; CI also runs the full gate.
# Exits 1 listing the offences, 2 on a setup error. Requires bash, grep, awk and (for 1 and 4) cargo.

set -euo pipefail

scan() {
  cd "$1"
  seeds="$(
    {
      # cargo config can set the variable ([env]) or alias the feature into every build
      for cfg in .cargo/config.toml .cargo/config; do [ ! -f "$cfg" ] || echo "$cfg"; done
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
    done < <(grep -v '^[[:space:]]*#' "${queue[$i]}" | grep -oE '[A-Za-z0-9_./-]+\.sh|scripts/[A-Za-z0-9_./-]+' || true)
  done
  [ "${#queue[@]}" -eq 0 ] && return 0
  grep -nHE 'ZIRV_FLOW_BENCH_BUILD|bench-no-decisions|ZIRV_FLOW_BROWSER_TEST_BUILD|browser-test|scripts/bench/|docs/benchmarks/|tools/(loadgen|renderbench)' -- "${queue[@]}" \
    | grep -Ev '^[^:]+:[0-9]+:[[:space:]]*#' || true
}

# Cargo.toml lines that put the browser-test feature in a default feature list, however the list
# is wrapped.
default_hits() {
  (
    cd "$1"
    files="$(find . -maxdepth 4 -name Cargo.toml -not -path './target/*' -not -path './node_modules/*' \
      -not -path './.git/*' -not -path './.zirv/*' -not -path './.claude/*' | sed 's|^\./||' | sort)"
    [ -n "$files" ] || exit 0
    # shellcheck disable=SC2086
    awk '
      FNR == 1 { listing = 0 }
      /^[[:space:]]*#/ { next }
      /^[[:space:]]*default[[:space:]]*=/ { listing = 1 }
      listing && /browser-test/ { print FILENAME ":" FNR ":" $0 }
      listing && /\]/ { listing = 0 }
    ' $files
  )
}

run_gate() {
  hits="$(scan "$1")"
  if [ -n "$hits" ]; then
    echo "test-feature gate: test features and benchmark files must not appear in a shipped build input:" >&2
    printf '%s\n' "$hits" | sed 's/^/  - /' >&2
    return 1
  fi
  hits="$(default_hits "$1")"
  if [ -n "$hits" ]; then
    echo "test-feature gate: browser-test must not be a default feature:" >&2
    printf '%s\n' "$hits" | sed 's/^/  - /' >&2
    return 1
  fi
}

# Checks that building `args` in `root` without the variable `var` fails with the guard's message `message`.
guard_fails() {
  root="$1"; var="$2"; message="$3"; shift 3
  if output="$(cd "$root" && env -u "$var" cargo check --quiet "$@" 2>&1)"; then
    echo "test-feature gate: $* compiled without $var" >&2
    return 1
  fi
  if [[ "$output" != *"$message"* ]]; then
    echo "test-feature gate: the check failed, but not with the guard's message:" >&2
    printf '%s\n' "$output" | tail -n 15 >&2
    return 1
  fi
}

run_guard_check() {
  guard_fails "$1" ZIRV_FLOW_BENCH_BUILD "is for benchmark builds only" -p zirv-flow-store --features bench-no-decisions
  guard_fails "$1" ZIRV_FLOW_BROWSER_TEST_BUILD "is for the browser test server only" -p zirv-flow --features browser-test
}

# The server binary of a default build must not carry the test sink route. Cargo's default target
# directory is `target`; CARGO_TARGET_DIR overrides it.
run_route_check() {
  (cd "$1" && cargo build --quiet -p zirv-flow --bin zirv-flow)
  target="${CARGO_TARGET_DIR:-$1/target}"
  binary=""
  for candidate in "$target/debug/zirv-flow" "$target/debug/zirv-flow.exe"; do
    [ ! -f "$candidate" ] || binary="$candidate"
  done
  if [ -z "$binary" ]; then
    echo "test-feature gate: the default build left no zirv-flow binary under $target/debug" >&2
    return 2
  fi
  if grep -aqF '/__test/' "$binary"; then
    echo "test-feature gate: the default zirv-flow binary contains a /__test/ route" >&2
    return 1
  fi
}

run_self_test() {
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  failures=0
  expect() {   # name, wanted exit, mutation applied to a fresh clean tree
    dir="$work/.claude/worktrees/$1"
    mkdir -p "$dir/.github/workflows" "$dir/scripts" "$dir/crates/zirv-flow"
    printf 'FROM rust\nRUN cargo build --release -p zirv-flow\n' >"$dir/Dockerfile"
    printf 'steps:\n  - run: bash scripts/build.sh\n' >"$dir/.github/workflows/cd.yaml"
    printf '#!/usr/bin/env bash\ncargo build --release\n' >"$dir/scripts/build.sh"
    printf 'services: {}\n' >"$dir/docker-compose.yml"
    printf '[features]\nbrowser-test = []\n' >"$dir/crates/zirv-flow/Cargo.toml"
    printf 'steps:\n  - run: ZIRV_FLOW_BENCH_BUILD=1 cargo clippy --features bench-no-decisions\n  - run: ZIRV_FLOW_BROWSER_TEST_BUILD=1 cargo build --features browser-test\n' \
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
  expect "an m8 bench file in a Dockerfile fails" 1 'echo "COPY scripts/bench/m8/run.sh /bench" >>Dockerfile'
  expect "an m8 compose file in CD fails" 1 'echo "  - run: docker compose -f scripts/bench/m8/compose.m8.yaml up" >>.github/workflows/cd.yaml'
  expect "a bench data file in a CD script fails" 1 'echo "cat docs/benchmarks/data/m8/ingest.json" >>scripts/build.sh'
  expect "the variable in a CD-invoked script fails" 1 'echo "ZIRV_FLOW_BENCH_BUILD=1 cargo build" >>scripts/build.sh'
  expect "the variable in a nested script fails" 1 \
    'echo "bash scripts/inner.sh" >>scripts/build.sh; echo "export ZIRV_FLOW_BENCH_BUILD=1" >scripts/inner.sh'
  expect "the feature name in the compose file fails" 1 'echo "# x" >>docker-compose.yml; echo "  command: --features bench-no-decisions" >>docker-compose.yml'
  expect "the feature name in the CD workflow fails" 1 'echo "  - run: cargo build --features bench-no-decisions" >>.github/workflows/cd.yaml'
  expect "the variable in a script without .sh fails" 1 \
    'echo "scripts/release" >>scripts/build.sh; echo "export ZIRV_FLOW_BENCH_BUILD=1" >scripts/release'
  expect "a workflow-invoked script without .sh fails" 1 \
    'echo "  - run: scripts/ship" >>.github/workflows/cd.yaml; echo "cargo build --features bench-no-decisions" >scripts/ship'
  expect "the variable in .cargo/config.toml [env] fails" 1 \
    'mkdir .cargo; printf "[env]\nZIRV_FLOW_BENCH_BUILD = \"1\"\n" >.cargo/config.toml'
  expect "the feature in a .cargo/config alias fails" 1 \
    'mkdir .cargo; printf "[alias]\nb = \"build --features bench-no-decisions\"\n" >.cargo/config'
  expect "a comment in .cargo/config.toml is not a use" 0 \
    'mkdir .cargo; printf "# not ZIRV_FLOW_BENCH_BUILD\n[build]\njobs = 4\n" >.cargo/config.toml'
  expect "the browser-test variable in a Dockerfile fails" 1 'echo "ENV ZIRV_FLOW_BROWSER_TEST_BUILD=1" >>Dockerfile'
  expect "the browser-test feature in the CD workflow fails" 1 \
    'echo "  - run: cargo build --features browser-test" >>.github/workflows/cd.yaml'
  expect "the test server binary in a compose file fails" 1 'echo "  command: zirv-flow-browser-test" >>docker-compose.yml'
  expect "the browser-test variable in a CD-invoked script fails" 1 \
    'echo "ZIRV_FLOW_BROWSER_TEST_BUILD=1 cargo build" >>scripts/build.sh'
  expect "the browser-test feature in a .cargo/config alias fails" 1 \
    'mkdir .cargo; printf "[alias]\nb = \"build --features browser-test\"\n" >.cargo/config.toml'
  expect "a comment naming browser-test is not a use" 0 'echo "# never build browser-test here" >>Dockerfile'
  expect "browser-test as a default feature fails" 1 \
    'printf "[features]\ndefault = [\"browser-test\"]\nbrowser-test = []\n" >crates/zirv-flow/Cargo.toml'
  expect "browser-test in a wrapped default list fails" 1 \
    'printf "[features]\ndefault = [\n    \"other\",\n    \"browser-test\",\n]\nbrowser-test = []\n" >crates/zirv-flow/Cargo.toml'
  expect "a commented default line is not a use" 0 \
    'printf "[features]\n# default = [\"browser-test\"]\ndefault = []\nbrowser-test = []\n" >crates/zirv-flow/Cargo.toml'
  [ "$failures" -eq 0 ] || { echo "check-bench-features.sh --self-test: $failures scenario(s) failed" >&2; return 1; }
  echo "check-bench-features.sh --self-test: all scenarios passed"
}

case "${1:-}" in
  --self-test) run_self_test ;;
  --static-only)
    [ "$#" -le 2 ] || { echo "usage: $0 --static-only [ROOT]" >&2; exit 2; }
    root="${2:-$(git rev-parse --show-toplevel 2>/dev/null)}" || { echo "not inside a git repository" >&2; exit 2; }
    run_gate "$root"
    ;;
  -*) echo "usage: $0 [ROOT | --static-only [ROOT] | --self-test]" >&2; exit 2 ;;
  *)
    root="${1:-$(git rev-parse --show-toplevel 2>/dev/null)}" || { echo "not inside a git repository" >&2; exit 2; }
    run_gate "$root"
    run_guard_check "$root"
    run_route_check "$root"
    ;;
esac
