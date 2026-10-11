#!/usr/bin/env bash
# Benchmark-feature gate: the `bench-no-decisions` cargo feature of zirv-flow-store turns
# decision-log appends into no-ops and exists only for the decision-overhead benchmark. It must
# never reach a shipped build. This fails when:
#
#   1. the resolved feature table (`cargo metadata --no-deps`, which parses the manifests
#      properly, multi-line arrays and indirection included) has the feature in the transitive
#      closure of any workspace crate's `default` feature, or a dependency declares it in its
#      `features`;
#   2. the feature name appears on a non-comment line of the CD workflow (or any workflow but
#      ci.yaml), a Dockerfile, a compose file, a cargo config, or a Cargo manifest other than the
#      store's own definition, or on a `--release` line of ci.yaml;
#   3. `--all-features` appears on a non-comment line of those shipped-build inputs or of any
#      script they invoke (followed through nested scripts), since it would enable the feature.
#
# Usage:
#   check-bench-features.sh [ROOT]
#   check-bench-features.sh --self-test
#
# Exits 1 and lists every offence, 2 on a setup error. Requires bash, grep, jq and cargo.

set -euo pipefail

needle='bench-no-decisions'
store_manifest='crates/zirv-flow-store/Cargo.toml'

# Non-comment lines of $1 matching the fixed string $2, as "file:line:text".
mentions() {
  grep -nHF -- "$2" "$1" 2>/dev/null \
    | grep -Ev '^[^:]+:[0-9]+:[[:space:]]*#' || true
}

# The shipped-build inputs under $1, then every script they (transitively) invoke by a path
# ending in .sh that exists under $1. One path per line.
shipped_inputs() {
  root="$1"
  seeds="$(
    {
      find "$root/.github/workflows" -maxdepth 1 -type f \( -name '*.yaml' -o -name '*.yml' \) \
        ! -name 'ci.yaml' 2>/dev/null
      find "$root" -maxdepth 3 -type f \( -name 'Dockerfile*' -o -name 'docker-compose*.yml' \
        -o -name 'docker-compose*.yaml' -o -name 'compose*.yaml' -o -name 'compose*.yml' \) \
        -not -path '*/node_modules/*' -not -path '*/target/*' -not -path '*/.git/*' \
        -not -path '*/.zirv/*' -not -path '*/.claude/*'
      find "$root" -maxdepth 3 -path '*/.cargo/config*' -type f
    } | sort -u
  )"
  declare -A seen=()
  queue=()
  while IFS= read -r file; do
    [ -n "$file" ] || continue
    seen["$file"]=1
    queue+=("$file")
  done <<<"$seeds"
  i=0
  while [ "$i" -lt "${#queue[@]}" ]; do
    file="${queue[$i]}"
    i=$((i + 1))
    while IFS= read -r token; do
      token="${token#./}"
      candidate="$root/$token"
      if [ -f "$candidate" ] && [ -z "${seen[$candidate]+x}" ]; then
        seen["$candidate"]=1
        queue+=("$candidate")
      fi
    done < <(grep -v '^[[:space:]]*#' "$file" 2>/dev/null | grep -oE '[A-Za-z0-9_./-]+\.sh' || true)
  done
  printf '%s\n' "${queue[@]}"
}

run_gate() {
  root="$1"
  hits=""

  # 1. The resolved feature table.
  if ! metadata="$(cargo metadata --manifest-path "$root/Cargo.toml" --format-version 1 --no-deps 2>/dev/null)"; then
    echo "benchmark-feature gate: cargo metadata failed for $root" >&2
    return 2
  fi
  found="$(
    jq -r --arg needle "$needle" '
      def reach($f):
        {seen: [], todo: ["default"]}
        | until(.todo | length == 0;
            .todo[0] as $x
            | .todo |= .[1:]
            | if (.seen | index($x)) then .
              else .seen += [$x] | .todo += ($f[$x] // []) end)
        | .seen;
      .workspace_members as $members
      | .packages[]
      | select(.id as $id | $members | index($id))
      | . as $p
      | ((reach($p.features)[] | select(. == $needle or endswith("/" + $needle)))
          | "\($p.name): default feature set enables \(.)"),
        ($p.dependencies[]
          | select(.features | index($needle))
          | "\($p.name): dependency \(.name) is declared with feature \($needle)")
    ' <<<"$metadata"
  )"
  [ -z "$found" ] || hits+="$found"$'\n'

  files=()
  while IFS= read -r file; do [ -z "$file" ] || files+=("$file"); done < <(shipped_inputs "$root")

  # 2. Mentions of the feature name.
  manifests=()
  while IFS= read -r file; do manifests+=("$file"); done < <(
    find "$root" -name Cargo.toml -type f \
      -not -path '*/node_modules/*' -not -path '*/target/*' -not -path '*/.git/*' \
      -not -path '*/.zirv/*' -not -path '*/.claude/*' \
      -not -path "$root/$store_manifest"
  )
  for file in "${files[@]}" "${manifests[@]}"; do
    found="$(mentions "$file" "$needle")"
    [ -z "$found" ] || hits+="$found"$'\n'
  done
  if [ -f "$root/.github/workflows/ci.yaml" ]; then
    found="$(mentions "$root/.github/workflows/ci.yaml" "$needle" | grep -- '--release' || true)"
    [ -z "$found" ] || hits+="$found"$'\n'
  fi

  # 3. --all-features would switch the feature on without naming it.
  for file in "${files[@]}"; do
    found="$(mentions "$file" "--all-features")"
    [ -z "$found" ] || hits+="$found"$'\n'
  done

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
    mkdir -p "$dir/.github/workflows" "$dir/scripts" "$dir/crates/zirv-flow-store/src" \
      "$dir/crates/zirv-flow/src"
    printf '[workspace]\nmembers = ["crates/*"]\nresolver = "3"\n' >"$dir/Cargo.toml"
    printf '[package]\nname = "zirv-flow-store"\nversion = "0.0.0"\nedition = "2024"\n\n[features]\n%s = []\n' \
      "$needle" >"$dir/$store_manifest"
    printf '[package]\nname = "zirv-flow"\nversion = "0.0.0"\nedition = "2024"\n\n[dependencies]\nzirv-flow-store = { path = "../zirv-flow-store" }\n' \
      >"$dir/crates/zirv-flow/Cargo.toml"
    : >"$dir/crates/zirv-flow-store/src/lib.rs"
    : >"$dir/crates/zirv-flow/src/lib.rs"
    printf 'FROM rust\nRUN cargo build --release -p zirv-flow\n' >"$dir/Dockerfile"
    printf 'steps:\n  - run: bash scripts/build.sh\n' >"$dir/.github/workflows/cd.yaml"
    printf '#!/usr/bin/env bash\ncargo build --release\n' >"$dir/scripts/build.sh"
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
  printf '# never pass %s or --all-features here\n' "$needle" >>"$dir/Dockerfile"
  expect "a comment is not a use" 0 "$dir"

  dir="$(fixture default)"
  printf '[package]\nname = "zirv-flow-store"\nversion = "0.0.0"\nedition = "2024"\n\n[features]\ndefault = ["%s"]\n%s = []\n' \
    "$needle" "$needle" >"$dir/$store_manifest"
  expect "feature in the store's single-line defaults fails" 1 "$dir"

  dir="$(fixture default-multiline)"
  printf '[package]\nname = "zirv-flow-store"\nversion = "0.0.0"\nedition = "2024"\n\n[features]\ndefault = [\n    "%s",\n]\n%s = []\n' \
    "$needle" "$needle" >"$dir/$store_manifest"
  expect "feature in a multi-line default fails" 1 "$dir"

  dir="$(fixture default-indirect)"
  printf '[package]\nname = "zirv-flow-store"\nversion = "0.0.0"\nedition = "2024"\n\n[features]\ndefault = ["extra"]\nextra = ["%s"]\n%s = []\n' \
    "$needle" "$needle" >"$dir/$store_manifest"
  expect "feature behind another default feature fails" 1 "$dir"

  dir="$(fixture default-dependent)"
  printf '[package]\nname = "zirv-flow"\nversion = "0.0.0"\nedition = "2024"\n\n[features]\ndefault = ["zirv-flow-store/%s"]\n\n[dependencies]\nzirv-flow-store = { path = "../zirv-flow-store" }\n' \
    "$needle" >"$dir/crates/zirv-flow/Cargo.toml"
  expect "a dependent's default enabling the feature fails" 1 "$dir"

  dir="$(fixture dependent)"
  printf '[package]\nname = "zirv-flow"\nversion = "0.0.0"\nedition = "2024"\n\n[dependencies]\nzirv-flow-store = { path = "../zirv-flow-store", features = [\n  "%s",\n] }\n' \
    "$needle" >"$dir/crates/zirv-flow/Cargo.toml"
  expect "feature enabled by a dependent manifest fails" 1 "$dir"

  dir="$(fixture ci-release)"
  printf '  - run: cargo build --release --features zirv-flow-store/%s\n' "$needle" \
    >>"$dir/.github/workflows/ci.yaml"
  expect "feature in a CI release build fails" 1 "$dir"

  dir="$(fixture all-features-dockerfile)"
  printf 'RUN cargo build --release --all-features\n' >>"$dir/Dockerfile"
  expect "--all-features in a Dockerfile fails" 1 "$dir"

  dir="$(fixture all-features-cd)"
  printf '  - run: cargo build --release --all-features\n' >>"$dir/.github/workflows/cd.yaml"
  expect "--all-features in the CD workflow fails" 1 "$dir"

  dir="$(fixture all-features-script)"
  printf 'cargo build --release --workspace --all-features\n' >>"$dir/scripts/build.sh"
  expect "--all-features in a script CD invokes fails" 1 "$dir"

  dir="$(fixture all-features-nested)"
  printf 'bash scripts/inner.sh\n' >>"$dir/scripts/build.sh"
  printf 'cargo build --all-features\n' >"$dir/scripts/inner.sh"
  expect "--all-features in a script a script invokes fails" 1 "$dir"

  dir="$(fixture all-features-ci)"
  printf '  - run: cargo clippy --all-features\n' >>"$dir/.github/workflows/ci.yaml"
  expect "--all-features in ci.yaml alone is not a shipped build" 0 "$dir"

  if [ "$failures" -gt 0 ]; then
    echo "check-bench-features.sh --self-test: $failures scenario(s) failed" >&2
    return 1
  fi
  echo "check-bench-features.sh --self-test: all scenarios passed"
}

command -v jq >/dev/null || {
  echo "check-bench-features.sh: jq is required" >&2
  exit 2
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
