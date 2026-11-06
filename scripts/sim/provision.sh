#!/usr/bin/env bash
# Provisions the simulated production environment through the API (design section 5).
# usage: provision.sh [stage...]   stages: base registry people connections sending segments
#        templates syncs gate flows (default: all of them, in that order)
# Every stage is idempotent: it looks a thing up by name before it creates it.
set -euo pipefail
umask 022
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'

SIM_PROJECT=zirv-flow-sim
SIM_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SIM_DIR=$(if command -v cygpath > /dev/null 2>&1; then cygpath -m "$SIM_DIR"; else printf '%s' "$SIM_DIR"; fi)
STATE="$SIM_DIR/.state"
DEFS="$SIM_DIR/defs"
# shellcheck source=scripts/sim/lib.sh
source "$SIM_DIR/lib.sh"

STEP=start
WS=
OWNER_ID=
CSRF=
KEYS="$STATE/keys"

step() { STEP=$1; say "provision: $1"; }

# name_id <json-array> <name>: the id of the element whose .name (or .email) equals the name.
name_id() { jq -r --arg n "$2" '[.[] | select((.name // .email // .key) == $n)][0].id // empty' <<< "$1"; }

# ensure <list-path> <jq-path-to-array> <name> <create-path> <body>: the id, created when missing.
ensure() {
  local list=$1 array=$2 name=$3 create=$4 body=$5 found
  found=$(call GET "$list" | jq -c "$array")
  local id
  id=$(name_id "$found" "$name")
  if [[ -z "$id" ]]; then
    id=$(call POST "$create" "$body" | jq -er '.id')
  fi
  printf '%s' "$id"
}

ws() { call "$1" "/workspaces/$WS$2" "${3:-}" "${4:-}"; }

# render <file> <NAME=value>...: the file with every {{NAME}} replaced.
render() {
  local file=$1 text
  shift
  text=$(< "$file")
  local pair
  for pair in "$@"; do
    text=${text//"{{${pair%%=*}}}"/${pair#*=}}
  done
  printf '%s' "$text"
}

secret_file() { # secret_file <name>: a generated secret, kept across re-runs in .state/keys
  local file="$KEYS/$1"
  [[ -s "$file" ]] || { umask 077; random_secret > "$file"; }
  cat "$file"
}

stage_base() {
  step 'owner'
  local password
  password=$(env_value SIM_OWNER_PASSWORD)
  printf '%s\n' "$password" |
    compose exec -T zf-serve-1 zirv-flow admin create-owner --email owner@sim.example.com > /dev/null 2>&1 || true
  mkdir -p "$KEYS"
  sign_in
  step 'workspace and users'
  local list
  list=$(call GET /workspaces)
  WS=$(jq -r '[.[] | select(.slug == "sim-co")][0].id // empty' <<< "$list")
  [[ -n "$WS" ]] || WS=$(call POST /workspaces '{"name":"Sim Co","slug":"sim-co"}' | jq -er .id)
  echo "$WS" > "$KEYS/workspace.id"
  ws PUT "/members/$OWNER_ID" '{"role":"admin"}' > /dev/null
  local users user email role id extra
  users=$(call GET /users)
  for user in ada:admin ed:editor pat:publisher vic:viewer; do
    email="${user%%:*}@sim.example.com"
    role=${user##*:}
    id=$(jq -r --arg e "$email" '[.[] | select(.email == $e)][0].id // empty' <<< "$users")
    if [[ -z "$id" ]]; then
      id=$(call POST /users "$(jq -n --arg e "$email" --arg p "$(secret_file "user-${user%%:*}.password")" \
        '{email:$e,password:$p}')" | jq -er .id)
    fi
    extra=
    [[ $role != viewer ]] || extra=',"view_pii":false'
    ws PUT "/members/$id" "{\"role\":\"$role\"$extra}" > /dev/null
  done
  step 'api keys'
  local keys
  keys=$(ws GET /api-keys)
  if [[ ! -s "$KEYS/ingest.key" ]]; then
    ws POST /api-keys '{"name":"sim ingest","scopes":["ingest"]}' | jq -er .key | tr -d '\n' > "$KEYS/ingest.key"
  fi
  if [[ ! -s "$KEYS/read.key" ]]; then
    ws POST /api-keys '{"name":"sim read","scopes":["read","pii"]}' | jq -er .key | tr -d '\n' > "$KEYS/read.key"
  fi
  : "$keys"
}

stage_registry() {
  step 'traits'
  local existing
  existing=$(ws GET /traits | jq -c 'if type == "array" then . else (.traits // .items // []) end')
  jq -c '.[]' "$DEFS/traits.json" | while read -r trait; do
    local key
    key=$(jq -r .key <<< "$trait")
    if [[ -z "$(jq -r --arg k "$key" '[.[] | select(.key == $k)][0].key // empty' <<< "$existing")" ]]; then
      ws POST /traits "$trait" > /dev/null
    fi
  done
  step 'event schemas'
  existing=$(ws GET /event-schemas | jq -c 'if type == "array" then . else (.schemas // .event_schemas // .items // []) end')
  jq -c '.[]' "$DEFS/event-schemas.json" | while read -r schema; do
    local name
    name=$(jq -r .name <<< "$schema")
    if [[ -z "$(jq -r --arg k "$name" '[.[] | select(.name == $k)][0].name // empty' <<< "$existing")" ]]; then
      ws POST /event-schemas "$schema" > /dev/null
      ws POST "/event-schemas/$name/versions" '{}' > /dev/null
    fi
  done
  step 'topics and lists'
  existing=$(ws GET /topics | jq -c 'if type == "array" then . else (.topics // .items // []) end')
  jq -c '.[]' "$DEFS/topics.json" | while read -r topic; do
    local key
    key=$(jq -r .key <<< "$topic")
    if [[ -z "$(jq -r --arg k "$key" '[.[] | select(.key == $k)][0].key // empty' <<< "$existing")" ]]; then
      ws POST /topics "$topic" > /dev/null
    fi
  done
  existing=$(ws GET /lists | jq -c 'if type == "array" then . else (.lists // .items // []) end')
  local list
  for list in "Newsletter subscribers" "Win-back candidates"; do
    [[ -n "$(name_id "$existing" "$list")" ]] || ws POST /lists "$(jq -n --arg n "$list" '{name:$n}')" > /dev/null
  done
}

# Writes a fixture's key file atomically (the fixtures re-read it when it changes).
write_key_file() { # write_key_file <name> <json>
  printf '%s\n' "$2" > "$KEYS/$1.tmp" && mv "$KEYS/$1.tmp" "$KEYS/$1"
}

# credential_defs: the workspace credentials as a JSON array. Values generated here are kept in
# .state/keys so a re-run (and the fixtures that check them) see the same ones.
credential_defs() {
  local billing exports mailer_token feedback ops
  billing=$(secret_file billing.key)
  exports=$(secret_file exports.token)
  mailer_token=$(secret_file mailer.token)
  feedback=$(secret_file mailer.feedback)
  ops=$(secret_file ops.signing)
  jq -n --arg reader "$(env_value SIM_FLOW_READER_PASSWORD)" --arg billing "$billing" \
    --arg exports "$exports" --arg mt "$mailer_token" --arg fb "$feedback" --arg ops "$ops" \
    --arg smtp "$(env_value SIM_SMTP_PASSWORD)" '[
    {kind:"basic", name:"company-pg reader", fields:{username:"flow_reader"}, secret:{password:$reader}},
    {kind:"basic", name:"company-mysql reader", fields:{username:"flow_reader"}, secret:{password:$reader}},
    {kind:"api_key", name:"billing key", fields:{send_as:"header", header_name:"X-API-Key"}, secret:{api_key:$billing}},
    {kind:"bearer_token", name:"exports token", secret:{token:$exports}},
    {kind:"custom", name:"mailer API",
     schema:{fields:[
       {key:"token", label:"API token", type:"string", required:true, secret:true},
       {key:"feedback_secret", label:"Feedback signing secret", type:"string", required:true, secret:true}]},
     secret:{token:$mt, feedback_secret:$fb}},
    {kind:"smtp", name:"smtp sink",
     fields:{host:"smtp.sim.example.com", port:2525, tls:"tls", username:"sim"}, secret:{password:$smtp}},
    {kind:"signing_secret", name:"ops signing", secret:{signing_secret:$ops}}]'
}

# cred_id <name>: the id of a live credential.
cred_id() {
  local id
  id=$(ws GET /credentials | jq -r --arg n "$1" '(if type == "array" then . else .credentials end)
    | [.[] | select(.name == $n)][0].id // empty')
  [[ -n "$id" ]] || die "no credential named $1"
  printf '%s' "$id"
}

stage_connections() {
  step 'credentials'
  local existing defs item name
  existing=$(ws GET /credentials | jq -c 'if type == "array" then . else .credentials end')
  defs=$(credential_defs)
  while read -r item; do
    name=$(jq -r .name <<< "$item")
    [[ -n "$(name_id "$existing" "$name")" ]] || ws POST /credentials "$item" > /dev/null
  done < <(jq -c '.[]' <<< "$defs")
  # The fixtures check the same secrets the credentials hold.
  write_key_file billing.json "$(jq -n --arg k "$(secret_file billing.key)" '{api_key:$k}')"
  write_key_file receiver.json "$(jq -n --arg k "$(secret_file ops.signing)" '{secrets:[$k]}')"

  local file source_id body sources
  for file in postgres mysql billing warehouse kafka; do
    step "source $file"
    sources=$(ws GET /sources | jq -c 'if type == "array" then . else .sources end')
    name=$(jq -r .create.name "$DEFS/sources/$file.json")
    source_id=$(name_id "$sources" "$name")
    if [[ -z "$source_id" ]]; then
      local cname cid=
      cname=$(jq -r '.create.credential // empty' "$DEFS/sources/$file.json")
      [[ -z "$cname" ]] || cid=$(cred_id "$cname")
      body=$(jq -c --arg cid "$cid" \
        '.create | (if $cid != "" then .credential_id = $cid else . end) | del(.credential)' \
        "$DEFS/sources/$file.json")
      source_id=$(ws POST /sources "$body" | jq -er .id)
    fi
    write_key_file "source-$file.id" "$source_id"
    ws POST "/sources/$source_id/test" '{}' > /dev/null
    ws POST "/sources/$source_id/discover" '{}' > /dev/null
    local stream
    for stream in $(jq -r '.streams | keys[]' "$DEFS/sources/$file.json"); do
      ws PUT "/sources/$source_id/streams/$stream" "$(jq -c --arg s "$stream" '.streams[$s]' "$DEFS/sources/$file.json")" > /dev/null
    done
  done
  stage_forms
}

stage_forms() {
  step 'forms'
  local list webhooks hooks='{}' form key name id secret
  list=$(ws GET /lists | jq -r '(if type == "array" then . else .lists end) | [.[] | select(.name == "Newsletter subscribers")][0].id')
  webhooks=$(ws GET /webhooks | jq -c 'if type == "array" then . else .webhooks end')
  if [[ -s "$KEYS/hooks.json" ]]; then hooks=$(< "$KEYS/hooks.json"); fi
  while read -r form; do
    key=$(jq -r .key <<< "$form")
    name=$(jq -r .name <<< "$form")
    id=$(jq -r --arg k "$key" '.[$k].id // empty' <<< "$hooks")
    if [[ -z "$id" ]]; then
      local made
      made=$(ws POST /webhooks "$(jq -n --arg n "$name" '{name:$n,dedupe_field:"id",staleness_threshold_seconds:900}')")
      id=$(jq -er '.id' <<< "$made")
      secret=$(jq -er '.secret' <<< "$made")
      hooks=$(jq --arg k "$key" --arg i "$id" --arg s "$secret" '.[$k]={id:$i,secret:$s}' <<< "$hooks")
      write_key_file hooks.json "$hooks"
      write_key_file "webhook-$key.source" "$(jq -er '.source_id // .source.id' <<< "$made")"
    fi
    ws PUT "/sources/$(< "$KEYS/webhook-$key.source")/streams/payload" \
      "$(render "$DEFS/sources/forms.json" "NEWSLETTER_LIST=$list" | jq -c '.stream')" > /dev/null
  done < <(jq -c '.webhooks[]' "$DEFS/sources/forms.json")
  : "$webhooks"
}

# json_list <jq-array-or-wrapper-key>: reads a list response as a plain array.
as_array() { jq -c --arg k "$1" 'if type == "array" then . else .[$k] end'; }

stage_people() {
  step 'people backbone'
  compose --profile data run --rm seed python -B /gen/seed.py ingest-people
}

stage_syncs() {
  step 'first syncs'
  start_runs missing > /dev/null
}

# start_runs [missing|all]: a sync for every enabled pull stream that has no run waiting or running
# (missing: only where the last run failed or none exists; all: also after a success). A stream whose
# last run failed is started again (concurrent first syncs deadlock now and then, B findings).
start_runs() {
  local mode=${1:-missing} file id stream runs state started=0
  for file in "${PULL_SOURCES[@]}"; do
    id=$(< "$KEYS/source-$file.id")
    runs=$(ws GET "/sources/$id/runs?limit=40" | jq -c '.runs')
    for stream in $(ws GET "/sources/$id/streams" | as_array streams | jq -r '.[] | select(.enabled and .sync_mode != "stream") | .name'); do
      state=$(jq -r --arg s "$stream" '[.[] | select(.stream == $s)][0].state // "none"' <<< "$runs")
      case "$state" in
        queued | running) ;;
        succeeded) [[ $mode != all ]] || { ws POST "/sources/$id/runs" "$(jq -n --arg s "$stream" '{stream:$s}')" > /dev/null; started=$((started + 1)); } ;;
        *) ws POST "/sources/$id/runs" "$(jq -n --arg s "$stream" '{stream:$s}')" > /dev/null; started=$((started + 1)) ;;
      esac
    done
  done
  echo "$started"
}

stage_sending() {
  step 'sending domain'
  local domains domain_id body
  domains=$(ws GET /sending-domains | as_array domains)
  domain_id=$(jq -r '[.[] | select(.domain == "mail.sim.example.com")][0].id // empty' <<< "$domains")
  if [[ -z "$domain_id" ]]; then
    body=$(ws POST /sending-domains '{"domain":"mail.sim.example.com"}')
    domain_id=$(jq -er .id <<< "$body")
    jq '[.records[] | {key:.host,value:.expected_text}] | from_entries' <<< "$body" > "$STATE/dns/records.new"
    mv "$STATE/dns/records.new" "$STATE/dns/records.json"
  fi
  ws POST "/sending-domains/$domain_id/verify" '{}' > /dev/null
  wait_until 90 "the sending domain to verify" poll_jq "/workspaces/$WS/sending-domains/$domain_id" ".status == \"verified\""
  step 'sender identities'
  local senders addr
  senders=$(ws GET /sender-identities | as_array sender_identities)
  for addr in news orders; do
    if [[ -z "$(jq -r --arg a "$addr@mail.sim.example.com" '[.[] | select(.from_address == $a)][0].id // empty' <<< "$senders")" ]]; then
      ws POST /sender-identities "$(jq -n --arg d "$domain_id" --arg a "$addr@mail.sim.example.com" \
        '{domain_id:$d,from_name:"Sim Co",from_address:$a}')" > /dev/null
    fi
  done
  step 'destinations'
  local dests ca
  dests=$(ws GET /destinations | as_array destinations)
  ca=$(< "$STATE/tls/ca.pem")
  dest_ensure "Transactional SMTP" "$dests" "$(jq -n --arg c "$(cred_id 'smtp sink')" --arg ca "$ca" \
    '{name:"Transactional SMTP",kind:"smtp",credential_id:$c,config:{ca_certificate:$ca},rate:100,burst:200}')" > /dev/null
  local mailer
  mailer=$(dest_ensure "Marketing mailer" "$dests" "$(jq -n --arg c "$(cred_id 'mailer API')" \
    --slurpfile m "$DEFS/mailer-provider.json" \
    '{name:"Marketing mailer",kind:"provider",credential_id:$c,config:{manifest:$m[0]},rate:150,burst:300}')")
  write_key_file mailer.json "$(jq -n --arg d "$mailer" --arg t "$(secret_file mailer.token)" \
    --arg f "$(secret_file mailer.feedback)" '{destination_id:$d,token:$t,feedback_secret:$f}')"
  local ops
  ops=$(dest_ensure "Ops webhook" "$dests" "$(jq -n --arg c "$(cred_id 'ops signing')" \
    '{name:"Ops webhook",kind:"webhook",credential_id:$c,config:{url:"https://receiver.sim.example.com/ops"}}')")
  local id
  for id in $(ws GET /destinations | as_array destinations | jq -r '.[].id'); do
    ws POST "/destinations/$id/check" '{}' > /dev/null
  done
  step 'forwarding rule'
  local rules
  rules=$(ws GET /forwarding/rules | as_array rules)
  if [[ -z "$(name_id "$rules" 'Big orders')" ]]; then
    ws POST /forwarding/rules "$(jq -n --arg d "$ops" '{destination_id:$d,name:"Big orders",event_name:"order_placed",
      filter:[{prop:"amount",op:"gte",value:400}],
      mapping:{order_id:{from:"event.properties",pointer:"/order_id"},person:{from:"person.id"}}}')" > /dev/null
  fi
  step 'sending settings'
  local settings revision
  settings=$(ws GET /sending/settings)
  revision=$(jq -er .revision <<< "$settings")
  ws PUT /sending/settings "$(jq -n --argjson r "$revision" \
    '{revision:$r,frequency_caps:[{channel:"email",window:"P7D",limit:3,mode:"skip"}],quiet_hours:null}')" > /dev/null
}

stage_segments() {
  step 'segments'
  local existing item name
  existing=$(ws GET /segments | as_array segments)
  while read -r item; do
    name=$(jq -r .name <<< "$item")
    [[ -z "$(name_id "$existing" "$name")" ]] || continue
    ws POST /segments/preview "$(jq -c '{definition:.definition,sample_size:5}' <<< "$item")" > /dev/null
    ws POST /segments "$item" > /dev/null
  done < <(jq -c ".[]" "$DEFS/segments.json")
}

stage_templates() {
  step 'templates'
  local senders existing item name news orders sender id
  senders=$(ws GET /sender-identities | as_array sender_identities)
  news=$(jq -r '[.[] | select(.from_address == "news@mail.sim.example.com")][0].id' <<< "$senders")
  orders=$(jq -r '[.[] | select(.from_address == "orders@mail.sim.example.com")][0].id' <<< "$senders")
  existing=$(ws GET /templates | as_array templates)
  while read -r item; do
    name=$(jq -r .name <<< "$item")
    id=$(name_id "$existing" "$name")
    [[ -z "$id" ]] || continue
    sender=$news
    [[ $(jq -r .sender <<< "$item") == news ]] || sender=$orders
    id=$(ws POST /templates "$(jq -c --arg s "$sender" '{name:.name,channel:"email",category:.category,
      document:{schema:1,channel:"email",mode:"code",
        email:{subject:.subject,sender:$s,lang:"en",category:.category,html:.html,text:.text}}}' <<< "$item")" | jq -er .id)
    ws POST "/templates/$id/publish" '{}' > /dev/null
  done < <(jq -c '.[]' "$DEFS/templates.json")
}

# flow_vars: the NAME=value pairs the flow definitions use.
flow_vars() {
  local day=${SIM_WAIT_DAY_SECONDS:-120} t d
  t=$(ws GET /templates | as_array templates)
  d=$(ws GET /destinations | as_array destinations)
  local segs lists topics
  segs=$(ws GET /segments | as_array segments)
  lists=$(ws GET /lists | as_array lists)
  topics=$(ws GET /topics | as_array topics)
  printf '%s\n' \
    "DAY=$day" "DAYS3=$((day * 3))" "DAYS7=$((day * 7))" "DAYS14=$((day * 14))" \
    "TPL_WELCOME_1=$(name_id "$t" 'Welcome 1')" "TPL_WELCOME_2=$(name_id "$t" 'Welcome 2')" \
    "TPL_WE_MISS_YOU=$(name_id "$t" 'We miss you')" "TPL_ORDER_CONFIRMATION=$(name_id "$t" 'Order confirmation')" \
    "TPL_PAYMENT_FAILED=$(name_id "$t" 'Payment failed')" \
    "DEST_MAILER=$(name_id "$d" 'Marketing mailer')" "DEST_SMTP=$(name_id "$d" 'Transactional SMTP')" \
    "SEG_LAPSED=$(name_id "$segs" 'Lapsed customers')" "SEG_PAST_DUE=$(name_id "$segs" 'Past due')" \
    "LIST_WINBACK=$(name_id "$lists" 'Win-back candidates')" \
    "TOPIC_NEWSLETTER=$(jq -r '[.[] | select(.key == "newsletter")][0].id' <<< "$topics")"
}

# publish_flows early|late: creates and publishes the flows that may go out before the backfill,
# or only the ones that must wait for it (stage_flows).
publish_flows() {
  local only=${1:-all} vars existing item name id
  mapfile -t vars < <(flow_vars)
  existing=$(ws GET /flows | as_array flows)
  while read -r item; do
    name=$(jq -r .name <<< "$item")
    [[ $(jq -r '.after_backfill' <<< "$item") == $([[ $only == late ]] && echo true || echo false) ]] || continue
    step "flow $name"
    id=$(name_id "$existing" "$name")
    if [[ -n "$id" ]] && [[ $(jq -r --arg i "$id" '[.[] | select(.id == $i)][0].state' <<< "$existing") == published ]]; then
      continue
    fi
    if [[ -z "$id" ]]; then
      id=$(ws POST /flows "$(jq -c '{name:.name,graph:.graph,settings:.settings}' <<< "$item")" | jq -er .id)
    fi
    if [[ $(jq -r '.warmup' <<< "$item") == false ]]; then
      ws PUT "/flows/$id/warmup" '{"enabled":false,"seconds":300}' > /dev/null
    fi
    ws POST "/flows/$id/validate" '{}' > /dev/null
    ws POST "/flows/$id/publish" '{}' > /dev/null
  done < <(render "$DEFS/flows.json" "${vars[@]}" | jq -c '.[]')
}

PULL_SOURCES=(postgres mysql billing warehouse)

stamp() { # stamp <event>: records when a phase was reached, once per fresh start
  mkdir -p "$STATE"
  grep -q "^$1 " "$STATE/timeline" 2> /dev/null || echo "$1 $(date +%s)" >> "$STATE/timeline"
}

people_count() { zf_sql "SELECT count(*) FROM people WHERE merged_into IS NULL"; }

# pending_streams: the pull streams that have not finished a first run yet, one per line.
pending_streams() {
  local file id streams
  for file in "${PULL_SOURCES[@]}"; do
    id=$(< "$KEYS/source-$file.id")
    streams=$(ws GET "/sources/$id/streams") || return 1
    as_array streams <<< "$streams" |
      jq -r --arg f "$file" '.[] | select(.enabled and .sync_mode != "stream" and .freshness.last_success_at == null) | "\($f)/\(.name)"'
  done
}

backfill_done() { local pending; pending=$(pending_streams) || return 1; [[ -z "$pending" ]]; }

destinations_ok() {
  local id out
  for id in $(ws GET /destinations | as_array destinations | jq -r '.[].id'); do
    out=$(ws POST "/destinations/$id/check" '{}')
    jq -e '.ok == true' <<< "$out" > /dev/null || return 1
  done
}

# ready_gate: people at 99% of the target, the sending domain verified, every destination
# checked, the Kafka streams running and the webhooks receiving.
ready_gate_met() {
  local want have
  want=$(env_value SIM_PEOPLE)
  have=$(people_count)
  [[ -n "$have" && $have -ge $((want * 99 / 100)) ]] || return 1
  ws GET /sending-domains | as_array domains | jq -e 'all(.[]; .status == "verified")' > /dev/null || return 1
  destinations_ok || return 1
  ws GET "/sources/$(< "$KEYS/source-kafka.id")/streams" | as_array streams |
    jq -e 'all(.[]; .enabled and .freshness.state != "failing")' > /dev/null || return 1
}

stage_sync_all() {
  step 'sync all'
  say "started $(start_runs all) runs"
}

stage_blast() {
  step 'blast'
  local segs flow
  segs=$(ws GET /segments | as_array segments)
  flow=$(ws GET /flows | as_array flows | jq -r '[.[] | select(.name == "Re-engagement blast")][0].id // empty')
  [[ -n "$flow" ]] || die "blast: the Re-engagement blast flow is not published yet"
  ws POST "/flows/$flow/start" "$(jq -n --arg s "$(name_id "$segs" 'Lapsed customers')" '{segment_id:$s}')" > /dev/null
  say "started the blast on the lapsed customers (enrollment begins in about five minutes)"
}

stage_gate() {
  step 'ready gate'
  local waited=0 limit=${SIM_GATE_TIMEOUT:-5400}
  until ready_gate_met; do
    ((waited < limit)) || die "ready gate: not met after ${limit}s (people $(people_count), pending: $(pending_streams | tr '\n' ' '))"
    sleep 15
    waited=$((waited + 15))
  done
  stamp gate
  say "ready gate met: $(people_count) people; backfill pending: $(pending_streams | tr '\n' ' ')"
  bash "$SIM_DIR/verify.sh" --provision || die "ready gate: the provisioning checks failed"
}

# stage_flows: flows that fire on events, on a segment change or on a schedule are published only
# once the first syncs are complete, because a backfilled record arrives as an ordinary event
# (design 9.5, B findings). The ones a form post starts, and the manual ones, go out at once.
stage_flows() {
  publish_flows early
  step 'waiting for the backfill before the event-driven flows'
  local waited=0 limit=${SIM_BACKFILL_TIMEOUT:-14400}
  until backfill_done; do
    ((waited < limit)) || die "backfill: not complete after ${limit}s (pending: $(pending_streams | tr '\n' ' '))"
    start_runs missing > /dev/null
    sleep 30
    waited=$((waited + 30))
  done
  stamp backfill
  stage_segments
  publish_flows late
  stamp flows
}

zf_sql() { compose exec -T zf-db psql -XqAt -F ' ' -U zirv -d zirv_flow -c "$1" 2> /dev/null | tr -d '\r'; }

# dest_ensure <name> <existing-array> <body>: the id of the destination, created when missing.
dest_ensure() {
  local id
  id=$(name_id "$2" "$1")
  [[ -n "$id" ]] || id=$(ws POST /destinations "$3" | jq -er .id)
  printf '%s' "$id"
}

main() {
  local stages=("$@")
  [[ ${#stages[@]} -gt 0 ]] || stages=(base registry)
  [[ -f "$STATE/profile" ]] || die 'run sim.sh up first'
  [[ ! -f "$KEYS/workspace.id" ]] || sign_in
  local stage
  for stage in "${stages[@]}"; do
    "stage_${stage//-/_}"
  done
}

main "$@"
