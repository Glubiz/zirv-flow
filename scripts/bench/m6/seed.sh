#!/usr/bin/env bash
# Seeds the people of the send bench through the product path: `sendbench people` identifies them
# with an email address through the ingest batch endpoint (names sb-0 .. sb-(N-1) on the
# recipient domain), then the consent import (POST .../consent/import, 10,000 rows per call)
# subscribes each to email. No SQL writes. Safe to re-run: identify is idempotent per message id
# and an unchanged consent row is counted unchanged.
#
# Usage: SEED_PEOPLE=100000 scripts/bench/m6/seed.sh        (default 10000)

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

ws="$(m6_ws)"
n="${SEED_PEOPLE:-10000}"
prefix="${SEED_PREFIX:-sb}"
login || die "cannot sign in; run stack.sh up"
[ -x "$SENDBENCH" ] || die "no sendbench at $SENDBENCH; cargo build --release -p zirv-flow-loadgen"
[ -s "$M4_STATE/m6-ingest.key" ] || die "no ingest key; run setup.sh"

"$SENDBENCH" people --url "$api_base" --key-file "$M4_STATE/m6-ingest.key" --count "$n" \
  --prefix "$prefix" --domain "$M6_RCPT_DOMAIN"

count_people() {
  pgc -At -v ws="$ws" -v pat="$prefix-%@$M6_RCPT_DOMAIN" -c \
    "SELECT count(*) FROM identities WHERE workspace_id = :'ws' AND kind = 'email' AND value LIKE :'pat'"
}
deadline=$(( $(date +%s) + 1800 ))
until [ "$(count_people)" -ge "$n" ]; do
  [ "$(date +%s)" -lt "$deadline" ] || die "only $(count_people) of $n people arrived after 30 minutes"
  sleep 3
done
log "$n people exist"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
pgc -At -v ws="$ws" -v pat="$prefix-%@$M6_RCPT_DOMAIN" -c \
  "SELECT person_id || ',email,,subscribed' FROM identities WHERE workspace_id = :'ws' AND kind = 'email' AND value LIKE :'pat' ORDER BY value" \
  | split -l 10000 - "$tmp/part-"
for part in "$tmp"/part-*; do
  { echo 'person_id,channel,topic,status'; cat "$part"; } >"$part.csv"
  res="$(curl -sS -X POST "$api_base/api/v1/workspaces/$ws/consent/import" -H "@$M4_STATE/auth.hdr" \
    -H 'content-type: text/csv' --data-binary "@$part.csv")"
  jq -e '.invalid | length == 0' >/dev/null <<<"$res" || die "consent import refused rows: $(head -c 400 <<<"$res")"
  log "consent: $(jq -c '{changed, unchanged}' <<<"$res")"
done
