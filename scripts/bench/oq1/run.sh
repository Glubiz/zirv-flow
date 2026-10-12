#!/usr/bin/env bash
# OQ-1 benchmark runner (architecture OQ-1, spec #47 D4): jsonb with expression indexes on 3 hot
# traits (variant A) against promoted typed columns plus jsonb (variant B), on a plain
# PostgreSQL 16. Each measured statement runs under EXPLAIN (ANALYZE, BUFFERS) $OQ1_RUNS times
# and the medians are printed as JSON on standard output (progress goes to standard error).
#
# It only creates, fills and drops its own schemas, oq1_bench_a and oq1_bench_b, in the database
# DATABASE_URL names. Point it at a scratch database, never at an instance holding real data:
# the schemas are dropped first if they exist, and again at the end unless OQ1_KEEP=1.
#
# Environment:
#   DATABASE_URL   required; postgres:// URL of a PostgreSQL 16 the role may create schemas in
#   OQ1_PEOPLE     people to generate (default 1000000); use 10000 for a smoke test
#   OQ1_RUNS       repetitions per statement (default 5)
#   OQ1_UPSERTS    trait changes in the upsert (default 10000, at most OQ1_PEOPLE)
#   OQ1_PLAN_DIR   when set, the last run's EXPLAIN output of each statement is written there
#   OQ1_KEEP       1 keeps the schemas afterwards
#   PSQL           the psql command, default "psql"; for a container with no client on the host:
#                  PSQL="docker exec -i my-pg psql" and a DATABASE_URL valid inside it
#
# Usage: DATABASE_URL=postgres://... scripts/bench/oq1/run.sh > oq1.json

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
: "${DATABASE_URL:?set DATABASE_URL to a scratch PostgreSQL 16 database}"
people="${OQ1_PEOPLE:-1000000}"
runs="${OQ1_RUNS:-5}"
upserts="${OQ1_UPSERTS:-10000}"
psql_cmd="${PSQL:-psql}"

case "$people$runs$upserts" in
  *[!0-9]* | "")
    echo "OQ1_PEOPLE, OQ1_RUNS and OQ1_UPSERTS must be positive integers" >&2
    exit 2
    ;;
esac
if [ "$upserts" -gt "$people" ] || [ "$runs" -lt 1 ]; then
  echo "need OQ1_RUNS >= 1 and OQ1_UPSERTS <= OQ1_PEOPLE" >&2
  exit 2
fi

# Word splitting of $psql_cmd is intended.
psql() { $psql_cmd "$DATABASE_URL" -X -q -v ON_ERROR_STOP=1 "$@"; }

work="$(mktemp -d)"
cleanup() {
  rm -rf "$work"
  if [ "${OQ1_KEEP:-0}" != 1 ]; then
    for variant in a b; do
      printf 'DROP SCHEMA IF EXISTS oq1_bench_%s CASCADE;\n' "$variant" | psql -f - >/dev/null 2>&1 || true
    done
  fi
}
trap cleanup EXIT

server_version="$(printf 'SHOW server_version;\n' | psql -At -f -)"
case "$server_version" in
  16.*) ;;
  *) echo "warning: PostgreSQL $server_version, the benchmark is specified for 16" >&2 ;;
esac

# The statement named $2 in queries file $1, without comments before the first marker.
statement() {
  awk -v want="$2" '/^-- name: /{ on = ($3 == want); next } on' "$1"
}

# Median of the numbers on standard input.
median() {
  sort -n | awk '{ a[NR] = $1 } END { if (NR == 0) print 0; else if (NR % 2) print a[(NR + 1) / 2]; else print (a[NR / 2] + a[NR / 2 + 1]) / 2 }'
}

json_escape() { sed 's/\\/\\\\/g; s/"/\\"/g'; }

vars=(-v "people=$people" -v "upserts=$upserts")
names=()
while IFS= read -r name; do names+=("$name"); done < <(awk '/^-- name: /{ print $3 }' "$here/queries_a.sql")

variant_json=()
for variant in a b; do
  schema="oq1_bench_$variant"
  echo "[$variant] generating $people people in schema $schema ..." >&2
  cat "$here/gen.sql" "$here/variant_$variant.sql" | psql "${vars[@]}" -v "schema=$schema" -f - >/dev/null

  sizes="$(printf 'SET search_path = %s, pg_catalog;\nSELECT pg_table_size($$people$$), pg_indexes_size($$people$$);\n' "$schema" | psql -At -F, -f -)"
  table_bytes="${sizes%%,*}"
  index_bytes="${sizes##*,}"

  tsv="$work/$variant.tsv"
  : >"$tsv"
  for name in "${names[@]}"; do
    body="$(statement "$here/queries_$variant.sql" "$name")"
    for run in $(seq 1 "$runs"); do
      echo "[$variant] $name run $run/$runs" >&2
      out="$(
        {
          printf 'SET search_path = %s, pg_catalog;\nBEGIN;\n' "$schema"
          printf '\\timing on\nEXPLAIN (ANALYZE, BUFFERS)\n%s;\n\\timing off\nROLLBACK;\n' "$body"
        } | psql -At "${vars[@]}" -f -
      )"
      exec_ms="$(sed -n 's/^Execution Time: \([0-9.]*\) ms.*/\1/p' <<<"$out" | head -n 1)"
      plan_ms="$(sed -n 's/^Planning Time: \([0-9.]*\) ms.*/\1/p' <<<"$out" | head -n 1)"
      wall_ms="$(sed -n 's/^Time: \([0-9.]*\) ms.*/\1/p' <<<"$out" | head -n 1)"
      buffers="$(grep -m1 'Buffers:' <<<"$out" || true)"
      hit="$(grep -o 'hit=[0-9]*' <<<"$buffers" | head -n 1 | cut -d= -f2 || true)"
      read_="$(grep -o 'read=[0-9]*' <<<"$buffers" | head -n 1 | cut -d= -f2 || true)"
      top="$(head -n 1 <<<"$out" | sed 's/ *(cost=.*//')"
      if [ -z "$exec_ms" ] || [ -z "$wall_ms" ]; then
        echo "no timing in the output of $variant/$name:" >&2
        printf '%s\n' "$out" >&2
        exit 1
      fi
      printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$name" "$exec_ms" "${plan_ms:-0}" "$wall_ms" "${hit:-0}" "${read_:-0}" >>"$tsv"
      printf '%s' "$top" >"$work/$variant.$name.top"
      if [ -n "${OQ1_PLAN_DIR:-}" ]; then
        mkdir -p "$OQ1_PLAN_DIR"
        printf '%s\n' "$out" >"$OQ1_PLAN_DIR/$variant.$name.txt"
      fi
    done
  done

  entries=()
  for name in "${names[@]}"; do
    col() { awk -F'\t' -v n="$name" -v c="$1" '$1 == n { print $c }' "$tsv" | median; }
    top="$(json_escape <"$work/$variant.$name.top")"
    entries+=("$(printf '"%s":{"exec_ms":%s,"planning_ms":%s,"wall_ms":%s,"shared_hit":%s,"shared_read":%s,"top_node":"%s"}' \
      "$name" "$(col 2)" "$(col 3)" "$(col 4)" "$(col 5)" "$(col 6)" "$top")")
  done
  joined="$(IFS=,; printf '%s' "${entries[*]}")"
  variant_json+=("$(printf '"%s":{"table_bytes":%s,"index_bytes":%s,"statements":{%s}}' \
    "$variant" "$table_bytes" "$index_bytes" "$joined")")
done

variants="$(IFS=,; printf '%s' "${variant_json[*]}")"
printf '{"benchmark":"oq1","postgres":"%s","people":%s,"runs":%s,"upserts":%s,"statistic":"median","variants":{%s}}\n' \
  "$server_version" "$people" "$runs" "$upserts" "$variants"
