#!/usr/bin/env bash
# Creates what the M6 send benchmark needs (#233), as the bench owner (stack.sh up, sink.sh start
# first): a workspace, a verified fixture sending domain and sender identity, an `smtp`
# credential and destination on the host sink (STARTTLS, the sink's authority as the destination's
# `ca_certificate`), a published block template, a published HTML template, an ingest API key and
# the consent state of the people (seed.sh). Everything goes through the HTTP API EXCEPT the
# domain verification, which is SQL because the API path needs the DNS records of a real domain
# to be published: the statements are the ones the verify job runs (store/sending/verify.rs:
# each record `status = 'ok'`, `observed_text = {expected_text}`, `checked_at = now()`; the
# domain `status = 'verified'`, `verified_at = now()`, `last_checked_at = now()`), which the e2e
# test of the same path shows leaving exactly these values. The re-check that would undo it is
# pushed out to 720 hours by compose.m6.yaml.
#
# State goes to $M4_STATE/state.env (M6_*); the ingest key to $M4_STATE/m6-ingest.key (mode 600,
# never printed). Safe to re-run: what exists is kept; the destination's CA is refreshed.
#
# Usage: scripts/bench/m6/setup.sh

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

login || die "cannot sign in; run stack.sh up"
[ -s "$M6_SINK_DIR/ca.pem" ] || die "no sink authority at $M6_SINK_DIR/ca.pem; run sink.sh start"

ws="$(state_get M6_WS)"
if [ -z "$ws" ] || ! api GET "/workspaces/$ws" >/dev/null 2>&1; then
  ws="$(api_json POST /workspaces "$(jq -nc '{name: "m6-send-bench"}')" | jq -r .id)"
  [ -n "$ws" ] && [ "$ws" != null ] || die "the workspace was not created"
  state_put M6_WS "$ws"
  log "created workspace $ws"
fi

# Sending domain and sender identity.
domain_id="$(state_get M6_DOMAIN_ID)"
if [ -z "$domain_id" ] || ! api GET "/workspaces/$ws/sending-domains/$domain_id" >/dev/null 2>&1; then
  domain_id="$(api_json POST "/workspaces/$ws/sending-domains" "$(jq -nc --arg d "$M6_DOMAIN" '{domain: $d}')" | jq -r .id)"
  [ -n "$domain_id" ] && [ "$domain_id" != null ] || die "the sending domain was not created"
  state_put M6_DOMAIN_ID "$domain_id"
  # The check queued at creation must have run before the SQL, or it would overwrite it.
  for _ in $(seq 1 60); do
    [ "$(pgc -At -v d="$domain_id" -c "SELECT count(*) FROM sending_domains WHERE id = :'d' AND last_checked_at IS NOT NULL")" = 1 ] && break
    sleep 1
  done
  pg -v d="$domain_id" -f - <<'SQL'
UPDATE sending_domain_records SET status = 'ok', observed_text = ARRAY[expected_text], checked_at = now()
 WHERE domain_id = :'d';
UPDATE sending_domains SET status = 'verified', verified_at = now(), last_checked_at = now(), updated_at = now()
 WHERE id = :'d';
SQL
  log "domain $M6_DOMAIN verified by SQL"
fi
[ "$(api GET "/workspaces/$ws/sending-domains/$domain_id" | jq -r .status)" = verified ] || die "the domain is not verified"

identity="$(state_get M6_IDENTITY)"
if [ -z "$identity" ]; then
  identity="$(api_json POST "/workspaces/$ws/sender-identities" \
    "$(jq -nc --arg d "$domain_id" --arg a "news@$M6_DOMAIN" '{domain_id: $d, from_name: "News", from_address: $a}')" | jq -r .id)"
  [ -n "$identity" ] && [ "$identity" != null ] || die "the sender identity was not created"
  state_put M6_IDENTITY "$identity"
fi

# SMTP credential and destination on the sink.
cred="$(state_get M6_CRED)"
if [ -z "$cred" ] || ! api GET "/workspaces/$ws/credentials/$cred" >/dev/null 2>&1; then
  cred="$(api_json POST "/workspaces/$ws/credentials" "$(jq -nc --arg n "Bench sink $M6_SINK_HOST" --arg h "$M6_SINK_HOST" --argjson p "$M6_SINK_PORT" \
    '{kind: "smtp", name: $n, fields: {host: $h, port: $p, tls: "starttls", username: "bench"}, secret: {password: "bench"}}')" | jq -r .id)"
  [ -n "$cred" ] && [ "$cred" != null ] || die "the credential was not created"
  state_put M6_CRED "$cred"
fi
config="$(jq -nc --rawfile ca "$M6_SINK_DIR/ca.pem" '{ca_certificate: $ca}')"
dest="$(state_get M6_DEST)"
if [ -z "$dest" ] || ! api GET "/workspaces/$ws/destinations/$dest" >/dev/null 2>&1; then
  dest="$(api_json POST "/workspaces/$ws/destinations" "$(jq -nc --arg n "Bench sink $M6_SINK_HOST" --argjson c "$config" --arg cr "$cred" \
    '{kind: "smtp", name: $n, config: $c, credential_id: $cr, rate: 1000, burst: 1000}')" | jq -r .id)"
  [ -n "$dest" ] && [ "$dest" != null ] || die "the destination was not created"
  state_put M6_DEST "$dest"
else
  api_json PATCH "/workspaces/$ws/destinations/$dest" "$(jq -nc --argjson c "$config" '{config: $c}')" >/dev/null
fi

# Templates: a block email of the v1 set and an HTML email, both marketing with the unsubscribe link.
ensure_template() { # NAME STATE_KEY DOCUMENT_JSON
  local id
  id="$(state_get "$2")"
  if [ -n "$id" ] && api GET "/workspaces/$ws/templates/$id" >/dev/null 2>&1; then return; fi
  id="$(api_json POST "/workspaces/$ws/templates" "$(jq -nc --arg n "$1" --argjson d "$3" '{name: $n, channel: "email", document: $d}')" | jq -r .id)"
  [ -n "$id" ] && [ "$id" != null ] || die "template $1 was not created"
  api POST "/workspaces/$ws/templates/$id/publish" >/dev/null
  state_put "$2" "$id"
  log "published template $1 ($id)"
}
ensure_template "Bench blocks" M6_TPL_BLOCKS "$(jq -nc --arg s "$identity" '{schema: 1, channel: "email", mode: "blocks", blocks: {
  subject: "Bench blocks", sender: $s, lang: "en", category: "marketing",
  sections: [
    {id: "intro", columns: [[
      {type: "heading", id: "title", level: 1, text: "Hello"},
      {type: "text", id: "body", content: "<p>This is a benchmark message with some body text to render.</p>"},
      {type: "button", id: "cta", label: "Open", url: "https://shop.example/blocks?b=1&c=2"}]]},
    {id: "foot", columns: [[
      {type: "text", id: "legal", content: "<p><a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a></p>"}]]}]}}')"
ensure_template "Bench html" M6_TPL_HTML "$(jq -nc --arg s "$identity" '{schema: 1, channel: "email", mode: "code", email: {
  subject: "Bench html", sender: $s, lang: "en", category: "marketing",
  html: "<p>Hello</p><a href=\"https://shop.example/offer?x=1\">Offer</a> <a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a>",
  text: "Hello {{ system.unsubscribe_url }}"}}')"

# An ingest key for the people seeder.
if [ ! -s "$M4_STATE/m6-ingest.key" ]; then
  key="$(api_json POST "/workspaces/$ws/api-keys" "$(jq -nc '{name: "m6 ingest", scopes: ["ingest"]}')" | jq -r '.plaintext // .key // empty')"
  [ -n "$key" ] || die "no ingest key was returned"
  ( umask 077; printf '%s\n' "$key" >"$M4_STATE/m6-ingest.key" )
fi
echo "workspace=$ws destination=$dest"
