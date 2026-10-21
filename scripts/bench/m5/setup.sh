#!/usr/bin/env bash
# Creates what the M5 timer benchmark needs through the HTTP API, as the bench owner (stack.sh up
# first, in the workspace of the M4 10 M-people database): three manual-trigger flows, published,
# and a list of sample people for each. Nothing is written to the database directly.
#
#   m5-spread  trigger manual, re-entry once: wait.duration (86400 s) -> exit. The 900,000
#              spread timers copy the rows this flow's enrollments leave behind.
#   m5-burst   the same with wait.until_time (a local date_time) -> exit. The 100,000 timers
#              that wake in one minute copy this flow's rows.
#   m5-probe   wait.duration (20 s) -> exit. A handful of enrollments that really wake, to prove
#              the lateness query on rows the product wrote itself.
#
# The flow ids go to $M4_STATE/state.env (M5_FLOW_SPREAD, M5_FLOW_BURST, M5_FLOW_PROBE); the list
# ids as M5_LIST_<name>. Safe to re-run: what exists is kept.
#
# Usage: scripts/bench/m5/setup.sh

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

ws="$(state_get M4_WS)"
[ -n "$ws" ] || die "no workspace in $M4_STATE/state.env: the 10 M database's setup.sh has not run"
login || die "cannot sign in; run stack.sh up"

# ensure_flow NAME STATE_KEY NODE_JSON: the flow by name (created and published when missing).
ensure_flow() {
  local name="$1" key="$2" wait_node="$3" id graph body
  id="$(state_get "$key")"
  if [ -n "$id" ] && api GET "/workspaces/$ws/flows/$id" >/dev/null 2>&1; then
    log "flow $name exists ($id)"
    return
  fi
  graph="$(jq -nc --argjson w "$wait_node" '
    {v: 1,
     nodes: [($w + {id: "w1"}), {id: "x1", type: "exit"}],
     edges: [{id: "e1", from: "w1", port: "next", to: "x1"}]}')"
  body="$(jq -nc --arg n "$name" --argjson g "$graph" \
    '{name: $n, graph: $g, settings: {trigger: {type: "manual"}, reentry: {mode: "once"}}}')"
  id="$(api_json POST "/workspaces/$ws/flows" "$body" | jq -r .id)"
  [ -n "$id" ] && [ "$id" != null ] || die "flow $name was not created"
  api POST "/workspaces/$ws/flows/$id/publish" >/dev/null
  state_put "$key" "$id"
  log "created and published flow $name ($id)"
}

ensure_flow m5-spread M5_FLOW_SPREAD '{"type":"wait.duration","seconds":86400}'
ensure_flow m5-burst M5_FLOW_BURST '{"type":"wait.until_time","when":{"kind":"date_time","at":"2099-01-01T12:00:00"}}'
ensure_flow m5-probe M5_FLOW_PROBE '{"type":"wait.duration","seconds":20}'

# ensure_list NAME STATE_KEY
ensure_list() {
  local name="$1" key="$2" id
  id="$(state_get "$key")"
  if [ -n "$id" ] && api GET "/workspaces/$ws/lists/$id" >/dev/null 2>&1; then return; fi
  id="$(api_json POST "/workspaces/$ws/lists" "$(jq -nc --arg n "$name" '{name: $n}')" | jq -r .id)"
  [ -n "$id" ] && [ "$id" != null ] || die "list $name was not created"
  state_put "$key" "$id"
  log "created list $name ($id)"
}
ensure_list m5-sample-spread M5_LIST_SPREAD
ensure_list m5-sample-burst M5_LIST_BURST
ensure_list m5-sample-probe M5_LIST_PROBE
echo "workspace=$ws"
