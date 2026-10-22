#!/usr/bin/env bash
# Workspace dependency gate: section 5.1 of docs/architecture.md is the
# allowlist for direct workspace edges, and pure crates may not acquire I/O
# dependencies. Purity is checked from the all-targets normal-dependency cargo
# tree; the tree's enabled-feature field is used to reject tokio's `net` feature.
#
# Usage:
#   check-crate-deps.sh
#   check-crate-deps.sh --self-test
#
# Exits 1 and lists every disallowed edge, undocumented workspace crate, and
# purity violation. Requires bash, jq, Cargo, and rustc's cargo-tree command.

set -euo pipefail

case "${1:-}" in
  "") ;;
  --self-test) self_test=1 ;;
  *)
    echo "usage: $0 [--self-test]" >&2
    exit 2
    ;;
esac
if [ "$#" -gt 1 ]; then
  echo "usage: $0 [--self-test]" >&2
  exit 2
fi

declare -A table_crates=()
declare -A allowed_edges=()

load_allowlist() {
  architecture="$1"
  table_crates=()
  allowed_edges=()
  in_table=0

  while IFS= read -r line; do
    line="${line%$'\r'}"
    if [[ "$line" == "### 5.1 Allowed crate dependencies" ]]; then
      in_table=1
      continue
    fi
    if [ "$in_table" -eq 1 ] && [[ "$line" == "### "* ]]; then
      break
    fi
    if [ "$in_table" -ne 1 ] || [[ "$line" != *'`zirv-flow'* ]]; then
      continue
    fi

    IFS='|' read -r _ crate_cell deps_cell _ <<<"$line"
    crate="${crate_cell#*\`}"
    crate="${crate%%\`*}"
    [ -n "$crate" ] || continue
    table_crates["$crate"]=1

    rest="$deps_cell"
    while [[ "$rest" == *'`'* ]]; do
      rest="${rest#*\`}"
      dependency="${rest%%\`*}"
      allowed_edges["$crate|$dependency"]=1
      rest="${rest#*\`}"
    done
  done <"$architecture"

  if [ "${#table_crates[@]}" -eq 0 ]; then
    echo "crate-dependency gate: no section 5.1 crate rows found in $architecture" >&2
    return 2
  fi
}

run_gate() {
  architecture="$1"
  manifest="$2"
  load_allowlist "$architecture" || return $?

  if ! metadata="$(cargo metadata --manifest-path "$manifest" --format-version 1 --no-deps)"; then
    echo "crate-dependency gate: cargo metadata failed" >&2
    return 2
  fi

  mapfile -t workspace_rows < <(
    jq -r '
      .workspace_members as $members
      | .packages[]
      | select(.id as $id | $members | index($id))
      | [
          .name,
          ([.dependencies[]
            | select(.kind == null or .kind == "build")
            | .name]
           | unique
           | join(","))
        ]
      | @tsv
    ' <<<"$metadata"
  )

  declare -A workspace_crates=()
  for row in "${workspace_rows[@]}"; do
    IFS=$'\t' read -r crate _ <<<"$row"
    workspace_crates["$crate"]=1
  done

  violations=()
  for row in "${workspace_rows[@]}"; do
    IFS=$'\t' read -r crate dependencies <<<"$row"
    # jq on Windows ends its lines with CRLF, which would hide the last dependency.
    dependencies="${dependencies%$'\r'}"
    if [ -z "${table_crates[$crate]+present}" ]; then
      violations+=("workspace crate missing from section 5.1: $crate")
    fi

    [ -n "${dependencies:-}" ] || continue
    IFS=',' read -r -a dependency_names <<<"$dependencies"
    for dependency in "${dependency_names[@]}"; do
      if [ -n "${workspace_crates[$dependency]+present}" ] \
        && [ -z "${allowed_edges[$crate|$dependency]+allowed}" ]; then
        violations+=("$crate -> $dependency")
      fi
    done
  done

  pure_crates=(
    zirv-flow-core
    zirv-flow-segments
    zirv-flow-engine
    zirv-flow-templates
  )
  for crate in "${pure_crates[@]}"; do
    [ -n "${workspace_crates[$crate]+present}" ] || continue
    if ! tree="$(cargo tree --manifest-path "$manifest" -p "$crate" \
      --target all -e normal --prefix none --format $'{p}\t{f}')"; then
      violations+=("$crate purity check could not inspect its cargo tree")
      continue
    fi

    while IFS=$'\t' read -r package features; do
      dependency="${package%% *}"
      case "$dependency" in
        sqlx|reqwest|hyper|ureq)
          violations+=("$crate purity -> $dependency")
          ;;
        tokio)
          if [[ ",${features:-}," == *,net,* ]]; then
            violations+=("$crate purity -> tokio feature net")
          fi
          ;;
      esac
    done <<<"$tree"
  done

  if [ "${#violations[@]}" -gt 0 ]; then
    echo "crate-dependency gate: violations:" >&2
    printf '  - %s\n' "${violations[@]}" >&2
    return 1
  fi
  return 0
}

run_self_test() {
  repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  failures=0

  make_fixture() {
    name="$1"
    mode="$2"
    fixture="$work/$name"
    mkdir -p "$fixture/docs" "$fixture/crates" "$fixture/vendor/sqlx/src"
    cp "$repo_root/docs/architecture.md" "$fixture/docs/architecture.md"

    cat >"$fixture/Cargo.toml" <<'EOF'
[workspace]
members = ["crates/*"]
exclude = ["vendor/sqlx"]
resolver = "3"
EOF

    for crate in zirv-flow zirv-flow-core zirv-flow-store zirv-flow-api zirv-flow-jobs zirv-flow-render; do
      mkdir -p "$fixture/crates/$crate/src"
      if [ "$crate" = "zirv-flow" ]; then
        target='main.rs'
        body='fn main() {}'
      else
        target='lib.rs'
        body='//! Fixture crate.'
      fi
      printf '%s\n' "$body" >"$fixture/crates/$crate/src/$target"
      cat >"$fixture/crates/$crate/Cargo.toml" <<EOF
[package]
name = "$crate"
version = "0.0.0"
edition = "2024"
EOF
    done

    add_dependencies() {
      crate="$1"
      shift
      printf '\n[dependencies]\n' >>"$fixture/crates/$crate/Cargo.toml"
      for dependency in "$@"; do
        printf '%s = { path = "../%s" }\n' "$dependency" "$dependency" \
          >>"$fixture/crates/$crate/Cargo.toml"
      done
    }

    add_dependencies zirv-flow zirv-flow-api zirv-flow-jobs zirv-flow-render
    add_dependencies zirv-flow-store zirv-flow-core
    add_dependencies zirv-flow-api zirv-flow-core zirv-flow-store
    add_dependencies zirv-flow-jobs zirv-flow-core zirv-flow-store
    add_dependencies zirv-flow-render zirv-flow-core

    if [ "$mode" = "bad-edge" ]; then
      cat >>"$fixture/crates/zirv-flow-api/Cargo.toml" <<'EOF'
zirv-flow-jobs = { path = "../zirv-flow-jobs" }
EOF
    fi
    case "$mode" in
      render-to-store | render-to-api | render-to-jobs)
        target="zirv-flow-${mode##*-to-}"
        echo "$target = { path = \"../$target\" }" \
          >>"$fixture/crates/zirv-flow-render/Cargo.toml"
        ;;
    esac
    if [ "$mode" = "bad-purity" ]; then
      cat >"$fixture/vendor/sqlx/Cargo.toml" <<'EOF'
[package]
name = "sqlx"
version = "0.0.0"
edition = "2024"
EOF
      printf '%s\n' '//! Fake sqlx package for the purity self-test.' >"$fixture/vendor/sqlx/src/lib.rs"
      cat >>"$fixture/crates/zirv-flow-core/Cargo.toml" <<'EOF'

[dependencies]
sqlx = { path = "../../vendor/sqlx" }
EOF
    fi
    printf '%s' "$fixture"
  }

  check_fixture() {
    name="$1"
    mode="$2"
    expected_exit="$3"
    expected_text="$4"
    fixture="$(make_fixture "$name" "$mode")"
    set +e
    output="$(run_gate "$fixture/docs/architecture.md" "$fixture/Cargo.toml" 2>&1)"
    actual_exit=$?
    set -e
    if [ "$actual_exit" -eq "$expected_exit" ] \
      && { [ -z "$expected_text" ] || [[ "$output" == *"$expected_text"* ]]; }; then
      echo "self-test ok: $name (exit $actual_exit)"
    else
      echo "self-test FAILED: $name (expected exit $expected_exit, got $actual_exit)" >&2
      printf '%s\n' "$output" | sed 's/^/    /' >&2
      failures=$((failures + 1))
    fi
  }

  check_fixture "real-table-valid-fixture" "valid" 0 ""
  check_fixture "api-to-jobs-is-rejected" "bad-edge" 1 "zirv-flow-api -> zirv-flow-jobs"
  check_fixture "render-to-store-is-rejected" "render-to-store" 1 "zirv-flow-render -> zirv-flow-store"
  check_fixture "render-to-api-is-rejected" "render-to-api" 1 "zirv-flow-render -> zirv-flow-api"
  check_fixture "render-to-jobs-is-rejected" "render-to-jobs" 1 "zirv-flow-render -> zirv-flow-jobs"
  check_fixture "sqlx-in-pure-crate-is-rejected" "bad-purity" 1 "zirv-flow-core purity -> sqlx"

  if [ "$failures" -gt 0 ]; then
    echo "check-crate-deps.sh --self-test: $failures scenario(s) failed" >&2
    return 1
  fi
  echo "check-crate-deps.sh --self-test: all scenarios passed"
}

if [ "${self_test:-0}" -eq 1 ]; then
  run_self_test
  exit $?
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  echo "check-crate-deps.sh: not inside a git repository" >&2
  exit 2
}
cd "$repo_root"
run_gate "docs/architecture.md" "Cargo.toml"
