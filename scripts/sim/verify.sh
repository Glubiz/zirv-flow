#!/usr/bin/env bash
# The checks of design 8.2. Each is a named step; the script prints one PASS or FAIL line per step
# and exits non-zero when any step failed.
# usage: verify.sh [--provision] [group...]
#   groups: sources kafka webhooks ingest consistency destinations delivery forwarding prometheus
#   --provision runs the quick structural subset that provisioning ends with.
set -uo pipefail
umask 022
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'

SIM_PROJECT=zirv-flow-sim
SIM_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SIM_DIR=$(if command -v cygpath > /dev/null 2>&1; then cygpath -m "$SIM_DIR"; else printf '%s' "$SIM_DIR"; fi)
STATE="$SIM_DIR/.state"
KEYS="$STATE/keys"
# shellcheck source=scripts/sim/lib.sh
source "$SIM_DIR/lib.sh"

WS=
CSRF=
OWNER_ID=
RUN=$(date +%s)
FAILED=()
PASSED=0

[[ -f "$KEYS/workspace.id" ]] || die 'not provisioned: run sim.sh provision first'
sign_in
P="/workspaces/$WS"

# ---------------------------------------------------------------- helpers

as_array() { jq -c --arg k "$1" 'if type == "array" then . else .[$k] end'; }
zf_sql() { compose exec -T zf-db psql -XqAt -F ' ' -U zirv -d zirv_flow -c "$1" 2> /dev/null | tr -d '\r'; }
get() { local out; out=$(api GET "$1" 2> /dev/null) || return 1; [[ ${out##*$'\n'} =~ ^2 ]] || return 1; printf '%s' "${out%$'\n'*}"; }
post() { local out; out=$(api POST "$1" "${2:-\{\}}" 2> /dev/null) || return 1; [[ ${out##*$'\n'} =~ ^2 ]] || return 1; printf '%s' "${out%$'\n'*}"; }
source_id() { cat "$KEYS/source-$1.id"; }
prom() { curl -fsS --max-time 10 --data-urlencode "query=$1" http://localhost:9090/api/v1/query | command jq -r '.data.result[0].value[1] // empty' | tr -d '\r'; }
counter() { # counter <service> <port> <metric> : the summed value of a fixture counter
  metric "$1" "$2" "$3" | awk '{s += $NF} END {print s + 0}'
}

# eventually <seconds> <command...>: polls every 2 s until the command succeeds.
eventually() {
  local seconds=$1 end
  shift
  end=$((SECONDS + seconds))
  while ((SECONDS < end)); do
    if "$@" > /dev/null 2>&1; then return 0; fi
    sleep 2
  done
  "$@" > /dev/null 2>&1
}

# check <name> <function>: runs the function, prints PASS or FAIL (with its last message).
MESSAGE=
check() {
  local name=$1 started=$SECONDS
  MESSAGE=
  if "$2"; then
    printf 'PASS %-34s %3ss %s\n' "$name" "$((SECONDS - started))" "$MESSAGE"
    PASSED=$((PASSED + 1))
  else
    printf 'FAIL %-34s %3ss %s\n' "$name" "$((SECONDS - started))" "$MESSAGE"
    FAILED+=("$name")
  fi
}

# person_by <kind> <value>: the id of the person that identifier names, or nothing.
person_by() {
  post "$P/people/search" "$(jq -n --arg k "$1" --arg v "$2" '{identifier:{kind:$k,value:$v}}')" | jq -r '.people[0].id // empty'
}

ingest() { # ingest <path> <json>: the HTTP status of a call with the ingest key
  curl -sS --max-time 20 -o "$STATE/curl.out" -w '%{http_code}' -X POST -H "authorization: Bearer $(< "$KEYS/ingest.key")" \
    -H 'content-type: application/json' --data-binary "$2" "http://localhost:8080/api/v1$1"
}

hook_post() { # hook_post <form> <body> [timestamp] [secret-override]: the HTTP status of a signed form post
  local form=$1 body=$2 stamp=${3:-$(date +%s)} id secret sig
  id=$(jq -r --arg f "$form" '.[$f].id' "$KEYS/hooks.json")
  secret=${4:-$(jq -r --arg f "$form" '.[$f].secret' "$KEYS/hooks.json")}
  sig=$(printf '%s' "$stamp.$body" | openssl dgst -sha256 -hmac "$secret" | sed 's/^.* //')
  curl -sS --max-time 20 -o "$STATE/curl.out" -w '%{http_code}' -X POST -H 'content-type: application/json' \
    -H "X-Zirv-Timestamp: $stamp" -H "X-Zirv-Signature: sha256=$sig" --data-binary "$body" \
    "http://localhost:8080/hooks/$id"
}

# ---------------------------------------------------------------- sources

pull_streams() { # lines of "<source file> <stream name>"
  local file
  for file in postgres mysql billing warehouse; do
    get "$P/sources/$(source_id "$file")/streams" | as_array streams | jq -r --arg f "$file" '.[] | select(.enabled) | "\($f) \(.name)"'
  done
}

t_sources_first_runs() {
  local file stream bad=() runs seen
  while read -r file stream; do
    runs=$(get "$P/sources/$(source_id "$file")/runs?limit=100" | jq -c '.runs')
    seen=$(jq -r --arg s "$stream" '[.[] | select(.stream == $s and .state == "succeeded" and .records_read > 0)] | length' <<< "$runs")
    [[ $seen -gt 0 ]] || bad+=("$file/$stream")
  done < <(pull_streams)
  MESSAGE="without a succeeded run that read records: ${bad[*]:-none}"
  [[ ${#bad[@]} -eq 0 ]]
}

t_sources_fresh() {
  local file bad=()
  for file in postgres mysql billing warehouse kafka; do
    get "$P/sources/$(source_id "$file")/streams" | as_array streams |
      jq -e 'all(.[] | select(.enabled); .freshness.state == "fresh")' > /dev/null || bad+=("$file")
  done
  MESSAGE="not fresh: ${bad[*]:-none}"
  [[ ${#bad[@]} -eq 0 ]]
}

latest_run_id() { get "$P/sources/$1/runs?limit=50" | jq -r --arg s "$2" '[.runs[] | select(.stream == $s)][0].id // empty'; }

# A second run on every incremental stream must read what the generators changed meanwhile.
t_sources_cursor_moves() {
  local file stream id before state read total=0 bad=()
  declare -A started=()
  while read -r file stream; do
    [[ $file != warehouse ]] || continue
    id=$(source_id "$file")
    before=$(latest_run_id "$id" "$stream")
    # A run that is already queued or running reads the changes too; start one only when idle.
    state=$(get "$P/sources/$id/runs?limit=50" | jq -r --arg s "$stream" '[.runs[] | select(.stream == $s)][0].state // "none"')
    case "$state" in queued | running) ;; *) post "$P/sources/$id/runs" "$(jq -n --arg s "$stream" '{stream:$s}')" > /dev/null ;; esac
    started["$file/$stream"]="$before"
  done < <(pull_streams)
  sleep 5
  for key in "${!started[@]}"; do
    file=${key%%/*}
    stream=${key#*/}
    id=$(source_id "$file")
    eventually 240 run_finished "$id" "$stream"
    read=$(get "$P/sources/$id/runs?limit=50" | jq -r --arg s "$stream" '[.runs[] | select(.stream == $s and (.state == "succeeded"))][0].records_read // 0')
    total=$((total + read))
    [[ $read -gt 0 ]] || bad+=("$key")
  done
  MESSAGE="records read by the second runs: $total; idle streams: ${bad[*]:-none}"
  [[ $total -gt 0 && ${#bad[@]} -eq 0 ]]
}

run_finished() {
  local state
  state=$(get "$P/sources/$1/runs?limit=50" | jq -r --arg s "$2" '[.runs[] | select(.stream == $s)][0].state // "none"')
  [[ $state != queued && $state != running ]]
}

g_sources() {
  check sources_first_runs t_sources_first_runs
  check sources_fresh t_sources_fresh
  check sources_cursor_moves t_sources_cursor_moves
}

# ---------------------------------------------------------------- kafka

t_kafka_running() {
  local runs
  runs=$(get "$P/sources/$(source_id kafka)/runs?limit=20" | jq -c '.runs')
  jq -e '[.[] | select(.state == "running")] | map(.stream) | unique | length == 2' <<< "$runs" > /dev/null || {
    MESSAGE='both topic streams need a running run'
    return 1
  }
}

t_kafka_lag() {
  local lag
  lag=$(prom 'max(zirv_flow_stream_lag_messages)')
  MESSAGE="max lag ${lag:-none} messages (limit 20000)"
  [[ -n "$lag" ]] && awk -v l="$lag" 'BEGIN { exit !(l < 20000) }'
}

KAFKA_PREFIX="verify-$RUN"
t_kafka_known_keys() {
  compose --profile data run --rm -v "$SIM_DIR/defs:/defs:ro" seed python -B /defs/kafka_probe.py produce "$KAFKA_PREFIX" 100 1 > /dev/null 2>&1 || {
    MESSAGE='the probe could not produce'
    return 1
  }
  eventually 120 kafka_applied 100
  MESSAGE="applied $(zf_sql "SELECT count(*) FROM source_records WHERE stream = 'app-feature-used' AND record_key LIKE '$KAFKA_PREFIX-%'") of 100"
  kafka_applied 100
}
kafka_applied() { [[ $(zf_sql "SELECT count(*) FROM source_records WHERE stream = 'app-feature-used' AND record_key LIKE '$KAFKA_PREFIX-%' AND deleted_at IS NULL") -ge $1 ]]; }

t_kafka_tombstone() {
  compose --profile data run --rm -v "$SIM_DIR/defs:/defs:ro" seed python -B /defs/kafka_probe.py tombstone "$KAFKA_PREFIX-0" > /dev/null 2>&1 || return 1
  eventually 120 kafka_removed
  MESSAGE="record $KAFKA_PREFIX-0: $(zf_sql "SELECT CASE WHEN count(*) = 0 THEN 'gone' WHEN bool_and(deleted_at IS NOT NULL) THEN 'deleted' ELSE 'live' END FROM source_records WHERE stream = 'app-feature-used' AND record_key = '$KAFKA_PREFIX-0'")"
  kafka_removed
}
kafka_removed() { [[ $(zf_sql "SELECT count(*) FROM source_records WHERE stream = 'app-feature-used' AND record_key = '$KAFKA_PREFIX-0' AND deleted_at IS NULL") == 0 ]]; }

g_kafka() {
  check kafka_streams_running t_kafka_running
  check kafka_lag_below_max t_kafka_lag
  check kafka_known_keys_applied t_kafka_known_keys
  check kafka_tombstone_removes t_kafka_tombstone
}

# ---------------------------------------------------------------- webhooks

HOOK_ID="verify-$RUN"
hook_body() { jq -nc --arg id "$HOOK_ID" '{id:$id, email:"p5@mail.example.com", customer_id:"C0000005", name:"Verify", form:"newsletter", utm_source:"verify", consent:{newsletter:true}}'; }

t_webhook_accepted() {
  local status
  status=$(hook_post newsletter "$(hook_body)")
  MESSAGE="status $status"
  [[ $status == 202 ]]
}

t_webhook_applied() {
  local person list
  person=$(person_by external:customer_id C0000005)
  list=$(get "$P/lists" | as_array lists | jq -r '[.[] | select(.name == "Newsletter subscribers")][0].id')
  eventually 60 hook_applied "$person" "$list"
  MESSAGE="person ${person:0:8}"
  hook_applied "$person" "$list"
}
hook_applied() {
  [[ $(zf_sql "SELECT count(*) FROM events WHERE person_id = '$1' AND name = 'form_submitted' AND message_id LIKE 'wh:%:$HOOK_ID'") -ge 1 ]] &&
    [[ $(zf_sql "SELECT count(*) FROM list_members WHERE list_id = '$2' AND person_id = '$1'") -ge 1 ]]
}

t_webhook_bad_signature() {
  local status
  status=$(hook_post newsletter "$(hook_body)" "$(date +%s)" zfwh_not_the_secret)
  MESSAGE="status $status"
  [[ $status == 401 ]]
}

t_webhook_stale() {
  local status
  status=$(hook_post newsletter "$(hook_body)" "$(($(date +%s) - 3600))")
  MESSAGE="status $status"
  [[ $status == 401 ]]
}

t_webhook_dedupe() {
  local status count
  status=$(hook_post newsletter "$(hook_body)")
  sleep 5
  count=$(zf_sql "SELECT count(*) FROM events WHERE name = 'form_submitted' AND message_id LIKE 'wh:%:$HOOK_ID'")
  MESSAGE="repeat status $status, $count event"
  [[ $status == 202 && $count == 1 ]]
}

g_webhooks() {
  check webhook_signed_accepted t_webhook_accepted
  check webhook_applied_event_and_list t_webhook_applied
  check webhook_bad_signature_401 t_webhook_bad_signature
  check webhook_stale_timestamp_401 t_webhook_stale
  check webhook_repeat_dedupes t_webhook_dedupe
}

# ---------------------------------------------------------------- ingest

INGEST_USER="verify-user-$RUN"
t_ingest_roundtrip() {
  local a b
  a=$(ingest /ingest/identify "$(jq -nc --arg u "$INGEST_USER" --arg r "$RUN" '{userId:$u, identifiers:{email:("verify-"+$r+"@example.org")}, traits:{plan:"verify"}, messageId:("verify-id-"+$r)}')")
  b=$(ingest /ingest/track "$(jq -nc --arg u "$INGEST_USER" --arg r "$RUN" '{userId:$u, event:"page_viewed", properties:{path:"/verify"}, messageId:("verify-track-"+$r)}')")
  eventually 30 ingest_applied
  MESSAGE="identify $a, track $b, event $(zf_sql "SELECT count(*) FROM events WHERE message_id = 'verify-track-$RUN'")"
  [[ $a == 202 && $b == 202 ]] && ingest_applied
}
ingest_applied() { [[ $(zf_sql "SELECT count(*) FROM events WHERE message_id = 'verify-track-$RUN'") == 1 ]]; }

t_ingest_inbox_age() {
  local age
  age=$(prom 'max(zirv_flow_ingest_inbox_age_seconds)')
  MESSAGE="max inbox age ${age:-none} s (alert threshold 30 s)"
  [[ -z "$age" ]] || awk -v a="$age" 'BEGIN { exit !(a < 30) }'
}

g_ingest() {
  check ingest_identify_and_track t_ingest_roundtrip
  check ingest_inbox_age t_ingest_inbox_age
}

# ---------------------------------------------------------------- consistency

t_consistency_search() {
  local n id_a id_b email wrong=0
  for _ in $(seq 1 20); do
    n=$(((RANDOM * 32768 + RANDOM) % $(env_value SIM_PEOPLE) + 1))
    id_a=$(person_by external:customer_id "$(printf 'C%07d' "$n")")
    [[ -n "$id_a" ]] || { wrong=$((wrong + 1)); continue; }
    email=$(get "$P/people/$id_a" | jq -r '[.identity_groups[].identities[] | select(.kind == "email")][0].value // empty')
    id_b=$(person_by email "$email")
    [[ "$id_a" == "$id_b" ]] || wrong=$((wrong + 1))
  done
  MESSAGE="$wrong of 20 people differ between the customer id and the email"
  [[ $wrong -eq 0 ]]
}

t_consistency_sources() {
  local n id detail missing=0 sources keys
  for _ in $(seq 1 20); do
    n=$(((RANDOM * 32768 + RANDOM) % $(env_value SIM_PEOPLE) + 1))
    id=$(person_by external:customer_id "$(printf 'C%07d' "$n")")
    detail=$(get "$P/people/$id") || { missing=$((missing + 1)); continue; }
    sources=$(jq '[.identity_groups[].source] | unique | length' <<< "$detail")
    keys=$(jq -r '[.traits[].key] | join(" ")' <<< "$detail")
    [[ $sources -ge 4 && " $keys " == *" first_name "* && " $keys " == *" plan_status "* && " $keys " == *" loyalty_tier "* &&
      " $keys " == *" billing_status "* && " $keys " == *" newsletter_opt_in "* ]] || missing=$((missing + 1))
  done
  # Customers the generators closed or cancelled have lost their records on purpose, so a few may lack a source.
  MESSAGE="$missing of 20 people lack a source (customers, subscriptions, loyalty, billing, export); at most 3 allowed"
  [[ $missing -le 3 ]]
}

g_consistency() {
  check consistency_search_same_person t_consistency_search
  check consistency_traits_from_sources t_consistency_sources
}

# ---------------------------------------------------------------- destinations

t_destinations_check() {
  local id out bad=()
  for id in $(get "$P/destinations" | as_array destinations | jq -r '.[].id'); do
    out=$(post "$P/destinations/$id/check") || { bad+=("$id"); continue; }
    jq -e '.ok == true' <<< "$out" > /dev/null || bad+=("$id")
  done
  MESSAGE="failing: ${bad[*]:-none}"
  [[ ${#bad[@]} -eq 0 ]]
}

dest_id() { get "$P/destinations" | as_array destinations | jq -r --arg n "$1" '[.[] | select(.name == $n)][0].id'; }
template_id() { get "$P/templates" | as_array templates | jq -r --arg n "$1" '[.[] | select(.name == $n)][0].id'; }
sender_id() { get "$P/sender-identities" | as_array sender_identities | jq -r --arg a "$1@mail.sim.example.com" '[.[] | select(.from_address == $a)][0].id'; }

# test_send <destination name> <template name> <sender> <service> <port> <metric>
# The mailer also carries flow mail, so its counter may rise by more than one; the SMTP sink only by this one.
test_send() {
  local before after body exact=${7:-exact}
  before=$(counter "$4" "$5" "$6")
  body=$(jq -nc --arg d "$(dest_id "$1")" --arg s "$(sender_id "$3")" --arg m "$OWNER_ID" '{version:1,destination_id:$d,sender_identity_id:$s,to:{member:$m}}')
  post "$P/templates/$(template_id "$2")/test-send" "$body" > /dev/null || { MESSAGE='the test send was refused'; return 1; }
  eventually 90 sink_increased "$before" "$4" "$5" "$6"
  after=$(counter "$4" "$5" "$6")
  MESSAGE="sink counter $before -> $after"
  if [[ $exact == exact ]]; then [[ $after -eq $((before + 1)) ]]; else [[ $after -ge $((before + 1)) ]]; fi
}
sink_increased() { [[ $(counter "$2" "$3" "$4") -gt $1 ]]; }

t_testsend_smtp() { test_send 'Transactional SMTP' 'Order confirmation' orders smtp-sink 8025 sim_smtp_messages_total; }
t_testsend_mailer() { test_send 'Marketing mailer' 'Welcome 1' news mailer 8000 sim_mailer_messages_total atleast; }

t_testsend_outcomes() {
  local outcomes
  outcomes=$(get "$P/test-sends" | jq -c '[.test_sends[:2][] | .outcome]')
  MESSAGE="latest outcomes $outcomes"
  jq -e 'length > 0 and all(.[]; . == "sent")' <<< "$outcomes" > /dev/null
}

g_destinations() {
  check destinations_check_ok t_destinations_check
  check testsend_smtp_sink_plus_one t_testsend_smtp
  check testsend_mailer_plus_one t_testsend_mailer
  check testsend_outcome_sent t_testsend_outcomes
}

# ---------------------------------------------------------------- delivery (flow-driven marketing mail)

DELIVERY_LIST=
ORG_BEFORE=0
BOUNCE_ADDRESS="bounce-$RUN@example.org"
SAMPLE=${SIM_VERIFY_SAMPLE:-200}

blast_flow() { get "$P/flows" | as_array flows | jq -r '[.[] | select(.name == "Re-engagement blast")][0].id'; }

t_delivery_start() {
  local list_name="Verify sample $RUN" n identifiers
  DELIVERY_LIST=$(post "$P/lists" "$(jq -nc --arg n "$list_name" '{name:$n}')" | jq -r .id)
  ingest /ingest/identify "$(jq -nc --arg e "$BOUNCE_ADDRESS" --arg u "verify-bounce-$RUN" '{userId:$u, identifiers:{email:$e}, traits:{first_name:"Bounce"}}')" > /dev/null
  ORG_BEFORE=$(org_suppressions)
  eventually 60 bounce_person_exists
  identifiers=$(for _ in $(seq 1 "$SAMPLE"); do
    n=$(((RANDOM * 32768 + RANDOM) % $(env_value SIM_PEOPLE) + 1))
    printf '{"kind":"external:customer_id","value":"C%07d"}\n' "$n"
  done | jq -sc --arg e "$BOUNCE_ADDRESS" '. + [{kind:"email",value:$e}] | unique')
  post "$P/lists/$DELIVERY_LIST/members" "$(jq -nc --argjson i "$identifiers" '{identifiers:$i}')" > /dev/null || { MESSAGE='list fill failed'; return 1; }
  post "$P/flows/$(blast_flow)/start" "$(jq -nc --arg l "$DELIVERY_LIST" '{list_id:$l}')" > /dev/null || { MESSAGE='flow start refused'; return 1; }
  MESSAGE="list of $(zf_sql "SELECT count(*) FROM list_members WHERE list_id = '$DELIVERY_LIST'") people started on the blast flow"
}
bounce_person_exists() { [[ -n "$(person_by email "$BOUNCE_ADDRESS")" ]]; }

delivered_count() { zf_sql "SELECT count(*) FROM events WHERE name = 'email.delivered' AND person_id IN (SELECT person_id FROM list_members WHERE list_id = '$DELIVERY_LIST')"; }

t_delivery_sent() {
  local want=$((SAMPLE * 9 / 10))
  eventually 900 sent_enough "$want"
  MESSAGE="$(zf_sql "SELECT count(*) FROM messages WHERE status = 'sent' AND NOT test AND person_id IN (SELECT person_id FROM list_members WHERE list_id = '$DELIVERY_LIST')") messages sent (at least $want)"
  sent_enough "$want"
}
sent_enough() { [[ $(zf_sql "SELECT count(*) FROM messages WHERE status = 'sent' AND NOT test AND person_id IN (SELECT person_id FROM list_members WHERE list_id = '$DELIVERY_LIST')") -ge $1 ]]; }

t_delivery_delivered() {
  eventually 300 delivered_enough "$((SAMPLE * 8 / 10))"
  MESSAGE="$(delivered_count) email.delivered events"
  delivered_enough "$((SAMPLE * 8 / 10))"
}
delivered_enough() { [[ $(delivered_count) -ge $1 ]]; }

engaged() { zf_sql "SELECT count(*) FROM events WHERE name = '$1' AND person_id IN (SELECT person_id FROM list_members WHERE list_id = '$DELIVERY_LIST')"; }
t_delivery_open_click() {
  eventually 300 open_and_click
  MESSAGE="$(engaged email.opened) opened, $(engaged email.clicked) clicked of $SAMPLE"
  open_and_click
}
open_and_click() { [[ $(engaged email.opened) -ge 1 && $(engaged email.clicked) -ge 1 ]]; }

t_hard_bounce_suppresses() {
  eventually 300 bounce_suppressed
  MESSAGE="hard-bounce events of the address: $(bounce_events), organization suppressions +$(( $(org_suppressions) - ORG_BEFORE )); the API check says $(post "$P/suppressions/check" "$(jq -nc --arg v "$BOUNCE_ADDRESS" '{kind:"email",value:$v}')" | jq -r .suppressed) (B findings: it reads workspace scope only)"
  bounce_suppressed
}
# A hard bounce suppresses the address for the whole organization, a scope the suppressions API does not show.
org_suppressions() { zf_sql "SELECT count(*) FROM suppressions WHERE scope = 'organization'"; }
bounce_events() { zf_sql "SELECT count(*) FROM events WHERE name = 'email.bounced' AND person_id = '$(person_by email "$BOUNCE_ADDRESS")' AND properties->>'kind' = 'hard'"; }
bounce_suppressed() { [[ $(bounce_events) -ge 1 && $(org_suppressions) -gt $ORG_BEFORE ]]; }

t_suppressed_send_skipped() {
  local person list
  person=$(person_by email "$BOUNCE_ADDRESS")
  list=$(post "$P/lists" "$(jq -nc --arg n "Verify bounce $RUN" '{name:$n}')" | jq -r .id)
  post "$P/lists/$list/members" "$(jq -nc --arg p "$person" '{person_ids:[$p]}')" > /dev/null
  post "$P/flows/$(blast_flow)/start" "$(jq -nc --arg l "$list" '{list_id:$l}')" > /dev/null || { MESSAGE='flow start refused'; return 1; }
  eventually 900 second_send_skipped "$person"
  MESSAGE="messages of the bounced person: $(zf_sql "SELECT string_agg(status || coalesce('/' || reason, ''), ', ' ORDER BY created_at) FROM messages WHERE person_id = '$person'")"
  second_send_skipped "$person"
}
second_send_skipped() { [[ $(zf_sql "SELECT count(*) FROM messages WHERE person_id = '$1' AND status = 'skipped'") -ge 1 ]]; }

g_delivery() {
  check delivery_blast_started t_delivery_start
  check delivery_messages_sent t_delivery_sent
  check delivery_feedback_delivered t_delivery_delivered
  check delivery_open_and_click_seen t_delivery_open_click
  check delivery_hard_bounce_suppresses t_hard_bounce_suppresses
  check delivery_suppressed_send_skipped t_suppressed_send_skipped
}

# ---------------------------------------------------------------- forwarding

RECEIVER_FILE="$STATE/receiver.json"
BIG_ORDER="verify-order-$RUN"

big_order() { # big_order <suffix>
  ingest /ingest/track "$(jq -nc --arg u "$INGEST_USER" --arg id "$BIG_ORDER-$1" \
    '{userId:$u, event:"order_placed", properties:{order_id:$id, amount:900, currency:"EUR", category:"verify", items:1}, messageId:$id}')"
}

t_forward_once() {
  local before dup_before dup after
  before=$(counter receiver 8000 sim_receiver_accepted_total)
  dup_before=$(counter receiver 8000 sim_receiver_duplicate_total)
  [[ $(big_order a) == 202 ]] || { MESSAGE='ingest refused the order'; return 1; }
  eventually 90 receiver_increased "$before"
  after=$(counter receiver 8000 sim_receiver_accepted_total)
  dup=$(counter receiver 8000 sim_receiver_duplicate_total)
  MESSAGE="receiver accepted $before -> $after, duplicates +$((dup - dup_before)), bad signatures $(counter receiver 8000 sim_receiver_bad_signature_total)"
  [[ $after -gt $before && $dup -eq $dup_before ]]
}
receiver_increased() { [[ $(counter receiver 8000 sim_receiver_accepted_total) -gt $1 ]]; }

t_forward_retry() {
  local peak=0 depth
  echo '{"fail_fraction": 1}' > "$RECEIVER_FILE"
  big_order b > /dev/null
  big_order c > /dev/null
  for _ in $(seq 1 12); do
    sleep 5
    depth=$(prom 'sum(zirv_flow_destination_forwarding_depth)')
    depth=${depth%.*}
    [[ ${depth:-0} -gt $peak ]] && peak=${depth:-0}
    [[ $peak -gt 0 ]] && break
  done
  echo '{}' > "$RECEIVER_FILE"
  eventually 180 forwarding_drained
  MESSAGE="forwarding depth peaked at $peak while the receiver answered 503, drained: $(prom 'sum(zirv_flow_destination_forwarding_depth)')"
  [[ $peak -gt 0 ]] && forwarding_drained
}
forwarding_drained() { local d; d=$(prom 'sum(zirv_flow_destination_forwarding_depth)'); [[ ${d%.*} == 0 ]]; }

g_forwarding() {
  check forwarding_delivered_once_signed t_forward_once
  check forwarding_503_depth_rises_drains t_forward_retry
}

# ---------------------------------------------------------------- prometheus

t_prom_targets() {
  local down
  down=$(curl -fsS http://localhost:9090/api/v1/targets | command jq -r '[.data.activeTargets[] | select(.health != "up") | .scrapePool] | join(",")' | tr -d '\r')
  MESSAGE="targets not up: ${down:-none}"
  [[ -z "$down" ]]
}

t_prom_rules() {
  local summary
  summary=$(curl -fsS http://localhost:9090/api/v1/rules | command jq -r '[.data.groups[].rules[]] | "\(length) rules, \([.[] | select(.health != "ok")] | length) unhealthy"' | tr -d '\r')
  MESSAGE=$summary
  [[ $summary == *" 0 unhealthy" && $summary != "0 rules"* ]]
}

g_prometheus() {
  check prometheus_targets_up t_prom_targets
  check prometheus_rules_load t_prom_rules
}

# ---------------------------------------------------------------- main

main() {
  local groups=() provision=false
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --provision) provision=true ;;
      sources | kafka | webhooks | ingest | consistency | destinations | delivery | forwarding | prometheus) groups+=("$1") ;;
      *) die "unknown argument: $1" ;;
    esac
    shift
  done
  if $provision; then
    check ingest_identify_and_track t_ingest_roundtrip
    check webhook_signed_accepted t_webhook_accepted
    check webhook_bad_signature_401 t_webhook_bad_signature
    check destinations_check_ok t_destinations_check
    g_prometheus
  else
    [[ ${#groups[@]} -gt 0 ]] || groups=(sources kafka webhooks ingest consistency destinations delivery forwarding prometheus)
    local group
    for group in "${groups[@]}"; do "g_$group"; done
  fi
  echo "verify: $PASSED passed, ${#FAILED[@]} failed${FAILED:+: ${FAILED[*]}}"
  [[ ${#FAILED[@]} -eq 0 ]]
}

main "$@"
