#!/usr/bin/env bash
# Creates what the M4 seed and the measurements need through the HTTP API, as the bench owner
# (stack.sh up first): the workspace, the 20 trait definitions of the OQ-1 data shape, the five
# event schemas the load generator's names use (each published, so the segment compiler knows
# their properties), and an `ingest` API key for loadgen. Nothing is written to the database
# directly. The workspace id goes to $M4_STATE/state.env (key M4_WS) and the ingest key to
# $M4_STATE/loadgen.key (mode 600, never printed). Safe to re-run: what exists is kept.
#
# Environment: see common.sh. M4_WS_NAME is the workspace name, default m4-bench.
#
# The event names are loadgen's (tools/loadgen/src/payload.rs: bench_page_viewed,
# bench_item_added, bench_order_placed, bench_plan_changed, bench_support_opened) and each
# schema declares the properties loadgen sends (amount, quantity, bench_seq), so the incremental
# measurement's events are valid. bench_order_placed also declares `category` (string), which
# only the seed writes.
#
# Usage: scripts/bench/m4/setup.sh

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

login || die "cannot sign in; run stack.sh up"

ws="$(state_get M4_WS)"
if [ -z "$ws" ] || ! api GET "/workspaces/$ws" >/dev/null 2>&1; then
  ws="$(api_json POST /workspaces "$(jq -nc --arg n "${M4_WS_NAME:-m4-bench}" '{name: $n}')" | jq -r .id)"
  [ -n "$ws" ] && [ "$ws" != null ] || die "the workspace was not created"
  state_put M4_WS "$ws"
  log "created workspace $ws"
else
  log "workspace $ws exists"
fi

# The 20 traits of oq1/gen.sql: 6 strings, 5 numbers, 4 booleans, 2 timestamps, 1 date, 2 lists.
existing="$(api GET "/workspaces/$ws/traits" | jq -r '.[].key')"
while read -r key type; do
  if grep -qx "$key" <<<"$existing"; then continue; fi
  api_json POST "/workspaces/$ws/traits" "$(jq -nc --arg k "$key" --arg t "$type" '{key: $k, type: $t}')" >/dev/null
  log "trait $key ($type)"
done <<'TRAITS'
country string
plan string
lifecycle string
channel string
city string
region string
age number
ltv number
seats number
orders number
score number
marketing_opt_in boolean
email_verified boolean
is_vip boolean
trial boolean
signup_at timestamp
last_seen_at timestamp
birthday date
tags string_list
interests string_list
TRAITS

props='[{"name":"amount","type":"number"},{"name":"quantity","type":"integer"},{"name":"bench_seq","type":"integer"}]'
schemas="$(api GET "/workspaces/$ws/event-schemas" | jq -r '.[].name')"
for name in bench_page_viewed bench_item_added bench_order_placed bench_plan_changed bench_support_opened; do
  if grep -qx "$name" <<<"$schemas"; then continue; fi
  draft="$props"
  if [ "$name" = bench_order_placed ]; then
    draft="$(jq -c '. + [{name: "category", type: "string"}]' <<<"$props")"
  fi
  api_json POST "/workspaces/$ws/event-schemas" \
    "$(jq -nc --arg n "$name" --argjson p "$draft" '{name: $n, mode: "warn", properties: $p}')" >/dev/null
  api POST "/workspaces/$ws/event-schemas/$name/versions" >/dev/null
  log "event schema $name published"
done

if [ ! -s "$M4_STATE/loadgen.key" ]; then
  ( umask 077
    api_json POST "/workspaces/$ws/api-keys" '{"name":"loadgen","scopes":["ingest"]}' | jq -r .key >"$M4_STATE/loadgen.key" )
  [ -s "$M4_STATE/loadgen.key" ] || die "the ingest key was not created"
  log "ingest key for loadgen written to $M4_STATE/loadgen.key"
fi
echo "workspace=$ws"
