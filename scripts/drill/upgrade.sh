#!/usr/bin/env bash
# CI-only populated upgrade. Diagnostics contain counts and redacted logs, never row values.
# --self-test checks counts and checkpoint diagnostics without Docker or PostgreSQL.
# --self-test-postgres additionally checks real constraints using DATABASE_URL and psql.
set +x
set -euo pipefail
umask 077
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
# shellcheck source=scripts/semver-compare.sh
source "$ROOT/scripts/semver-compare.sh"
# shellcheck source=scripts/drill/lib.sh
source "$ROOT/scripts/drill/lib.sh"

die() { echo "::error::upgrade drill: $*" >&2; exit 1; }

compare_counts() {
  jq -en --slurpfile before "$1" --slurpfile after "$2" '
    def counts: type == "object" and length > 0 and
      all(.[]; type == "number" and . >= 0 and . == floor);
    if ($before | length) != 1 or ($after | length) != 1 or
       ($before[0] | counts | not) or ($after[0] | counts | not) then
      error("integrity check=count_snapshot: invalid or empty snapshot")
    else
      all($before[0] | to_entries[]; . as $row |
        if ($after[0] | has($row.key) | not) then
          error("integrity check=table_present table=\($row.key)")
        elif $after[0][$row.key] < $row.value then
          error("integrity check=row_count table=\($row.key) before=\($row.value) after=\($after[0][$row.key])")
        else true end)
    end
  ' >/dev/null
}

count_self_test() {
  local scratch
  scratch=$(mktemp -d)
  printf '%s\n' '{"people":3000,"events":3000}' > "$scratch/before"
  printf '%s\n' '{"people":3000,"events":3001,"new_table":0}' > "$scratch/after"
  compare_counts "$scratch/before" "$scratch/after" || die 'self-test rejected intact rows'
  printf '%s\n' '{"people":2999,"events":3000}' > "$scratch/after"
  if compare_counts "$scratch/before" "$scratch/after" 2> "$scratch/error"; then die 'self-test missed a lost row'; fi
  grep -Fq 'check=row_count table=people before=3000 after=2999' "$scratch/error" || die 'self-test missing row diagnostic'
  printf '%s\n' '{"people":3000}' > "$scratch/after"
  if compare_counts "$scratch/before" "$scratch/after" 2> "$scratch/error"; then die 'self-test missed a lost table'; fi
  grep -Fq 'check=table_present table=events' "$scratch/error" || die 'self-test missing table diagnostic'
  printf '%s\n' '{}' > "$scratch/after"
  if compare_counts "$scratch/before" "$scratch/after" 2> "$scratch/error"; then die 'self-test accepted empty counts'; fi
  grep -Fq 'check=count_snapshot' "$scratch/error" || die 'self-test missing snapshot diagnostic'
  rm -rf "$scratch"
  echo 'upgrade count self-test OK (intact, missing row, missing table, empty snapshot)'
}

integrity_error() {
  # Only our metadata-only exceptions are printable; PostgreSQL errors may carry row values.
  grep 'ERROR:  integrity check=' "$1" >&2 || echo 'integrity check=script table=unknown constraint=unknown (no safe SQL diagnostic)' >&2
}

sql_self_test() (
  : "${DATABASE_URL:?set DATABASE_URL for the PostgreSQL self-test}"
  local schema scratch
  schema="upgrade_drill_test_${RANDOM}_$$"
  scratch=$(mktemp -d)
  trap 'psql -Xq "$DATABASE_URL" -c "DROP SCHEMA IF EXISTS $schema, ${schema}_unrelated CASCADE" > /dev/null 2>&1; rm -rf "$scratch"' EXIT
  self_sql() {
    psql -Xq -v ON_ERROR_STOP=1 "$DATABASE_URL" > "$scratch/setup" 2>&1 || die "SQL self-test check=$1 schema=$schema"
  }
  self_sql setup <<SQL
CREATE SCHEMA $schema;
CREATE SCHEMA ${schema}_unrelated;
CREATE TABLE ${schema}_unrelated.child (n int);
INSERT INTO ${schema}_unrelated.child VALUES (0);
ALTER TABLE ${schema}_unrelated.child ADD CONSTRAINT unrelated CHECK (n > 0) NOT VALID;
CREATE DOMAIN $schema.positive AS int CHECK (VALUE > 0);
CREATE TABLE $schema.parent (id int PRIMARY KEY);
CREATE TABLE $schema.child (id int CONSTRAINT child_fk REFERENCES $schema.parent,
                           n int CONSTRAINT child_positive CHECK (n > 0));
INSERT INTO $schema.parent VALUES (1);
INSERT INTO $schema.child VALUES (1, 1);
CREATE TABLE $schema.pair_parent (a int, b int, PRIMARY KEY (a, b));
CREATE TABLE $schema.pair_child (a int, b int, CONSTRAINT pair_fk
    FOREIGN KEY (a, b) REFERENCES $schema.pair_parent MATCH FULL);
INSERT INTO $schema.pair_parent VALUES (1, 1);
INSERT INTO $schema.pair_child VALUES (1, 1), (NULL, NULL);
CREATE TABLE $schema.events (source text, name text);
INSERT INTO $schema.events VALUES ('api', 'drill_event'), ('segments', 'segment.entered');
SQL
  integrity_test() {
    psql -XqAt -v ON_ERROR_STOP=1 -v schema="$schema" -v verify=true "$DATABASE_URL" \
      -f "$ROOT/scripts/drill/integrity.sql" > "$scratch/result" 2> "$scratch/error"
  }
  expect_integrity_failure() {
    if integrity_test; then die "SQL self-test missed $1"; fi
    if ! grep -Fq "$1" "$scratch/error"; then
      integrity_error "$scratch/error"
      die "SQL self-test expected $1"
    fi
  }
  integrity_test || { integrity_error "$scratch/error"; die 'SQL self-test rejected intact data'; }
  # A segment refresh adds system events; checkpoints count only the seeded API events.
  local seeded
  seeded=$(psql -XqAt -v ON_ERROR_STOP=1 "$DATABASE_URL" \
    -c "SET search_path TO $schema; $SEED_EVENT_COUNT" 2> "$scratch/error") || die 'SQL self-test check=seed_events query failed'
  [[ "$seeded" == 1 ]] || die "SQL self-test check=seed_events table=events expected=1 last=$seeded"
  self_sql plant_check <<SQL
ALTER TABLE $schema.child ADD CONSTRAINT planted CHECK (n > 2) NOT VALID;
SQL
  expect_integrity_failure "check=validate_constraint table=$schema.child constraint=planted"
  self_sql plant_fk <<SQL
ALTER TABLE $schema.child DROP CONSTRAINT planted;
SET session_replication_role = replica;
INSERT INTO $schema.child VALUES (2, 1);
SQL
  expect_integrity_failure "check=foreign_key_rows table=$schema.child constraint=child_fk"
  self_sql plant_full_fk <<SQL
DELETE FROM $schema.child WHERE id = 2;
SET session_replication_role = replica;
INSERT INTO $schema.pair_child VALUES (1, NULL);
SQL
  expect_integrity_failure "check=foreign_key_rows table=$schema.pair_child constraint=pair_fk"
  self_sql repair <<SQL
DELETE FROM $schema.pair_child WHERE b IS NULL AND a IS NOT NULL;
SQL
  integrity_test || { integrity_error "$scratch/error"; die 'SQL self-test rejected repaired data'; }
  echo 'upgrade SQL self-test OK (isolated tables, domain, CHECK, validated FK, MATCH FULL, seeded event count)'
)

# Identify changes profiles; track writes one API event. Segment refresh also writes events.
SEED_EVENT_COUNT="SELECT count(*) FROM events WHERE source = 'api' AND name = 'drill_event'"

redact() { python3 -I "$ROOT/scripts/drill/upgrade-diagnostics.py"; }

checkpoint_failure() {
  printf 'upgrade drill: checkpoint failed stage=%s label=%s expected=%s last=%s\n' \
    "$STAGE" "$1" "$2" "$3" | redact >&2
  {
    echo 'Ingest counts and reject reasons:'
    sql -c "SELECT 'inbox', count(*) FROM ingest_inbox;
      SELECT 'rejects', count(*) FROM ingest_rejects;
      SELECT 'reject_reason', reason, count(*) FROM ingest_rejects GROUP BY reason ORDER BY reason;
      SELECT 'drain_jobs', state, count(*), max(attempts) FROM jobs
        WHERE kind = 'ingest.drain' GROUP BY state ORDER BY state;" || echo 'Ingest diagnostics unavailable'
    echo 'Compose service state:'
    compose ps -a || echo 'Compose state unavailable'
    echo 'zirv-flow log tail:'
    compose logs --no-color --tail 100 zirv-flow || echo 'Service logs unavailable'
  } 2>&1 | redact >&2
  die "checkpoint failed during $STAGE"
}

wait_sql() {
  local query=$1 want=$2 last='not observed'
  for _ in $(seq 1 180); do
    if ! last=$(sql -c "$query"); then last='SQL query failed'; fi
    [[ "$last" != "$want" ]] || return 0
    sleep 1
  done
  checkpoint_failure "SQL timeout: $query" "$want" "$last"
}

wait_api() {
  local path=$1 predicate=$2 last=false
  for _ in $(seq 1 180); do
    # Only expose the predicate result, never the API response body.
    if api GET "$path" 200 | jq -e "$predicate" > /dev/null; then return 0; fi
    sleep 1
  done
  checkpoint_failure "API timeout: $path predicate=$predicate" true "$last"
}

checkpoint_self_test() (
  local DRILL_WORK STAGE=population_ingest status
  DRILL_WORK=$(mktemp -d)
  trap 'rm -rf "$DRILL_WORK"' EXIT
  sql() { echo 2999; }
  compose() {
    case "$1" in
      ps) echo 'zirv-flow running' ;;
      logs) echo 'owner@example.com +45-20304050 drill-user-0 person-0 event-0'; echo 'job attempt failed reason=apply_failed' ;;
      *) return 1 ;;
    esac
  }
  seq() { echo 1; }
  sleep() { :; }
  api() { echo '{"ready":false,"email":"owner@example.com"}'; }
  wait_sql 'SELECT count(*) FROM people' 2999
  status=0
  (wait_sql 'SELECT count(*) FROM people' 3000) > "$DRILL_WORK/result" 2>&1 || status=$?
  [[ "$status" == 1 ]] || die 'checkpoint self-test missed SQL timeout'
  grep -Fq 'stage=population_ingest label=SQL timeout: SELECT count(*) FROM people expected=3000 last=2999' "$DRILL_WORK/result" || die 'checkpoint self-test missing SQL diagnostic'
  grep -Fq 'zirv-flow running' "$DRILL_WORK/result" || die 'checkpoint self-test missing compose state'
  grep -Fq 'job attempt failed reason=apply_failed' "$DRILL_WORK/result" || die 'checkpoint self-test missing log tail'
  if grep -Eq 'owner@example.com|20304050|drill-user-0|person-0|event-0' "$DRILL_WORK/result"; then die 'checkpoint self-test exposed identifiers'; fi
  status=0
  (wait_api /health '.ready') > "$DRILL_WORK/result" 2>&1 || status=$?
  [[ "$status" == 1 ]] || die 'checkpoint self-test missed API timeout'
  grep -Fq 'label=API timeout: /health predicate=.ready expected=true last=false' "$DRILL_WORK/result" || die 'checkpoint self-test missing API diagnostic'
  if grep -Fq 'owner@example.com' "$DRILL_WORK/result"; then die 'checkpoint self-test exposed API body'; fi
  api() { echo '{"ready":true}'; }
  wait_api /health '.ready'
  echo 'upgrade checkpoint self-test OK (success, SQL/API timeouts, redacted diagnostics)'
)

case "${1:-}" in
  --self-test)
    [[ $# == 1 ]] || die 'unexpected self-test argument'
    count_self_test
    python3 -I "$ROOT/scripts/drill/upgrade-diagnostics.py" --self-test
    checkpoint_self_test
    exit ;;
  --self-test-postgres) [[ $# == 1 ]] || die 'unexpected self-test argument'; sql_self_test; exit ;;
esac

IMAGE_TAG=upgrade-drill
PREVIOUS=
SELECT_ONLY=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --image) [[ $# -ge 2 ]] || die 'missing image tag'; IMAGE_TAG=$2; shift 2 ;;
    --previous) [[ $# -ge 2 ]] || die 'missing previous tag'; PREVIOUS=${2#v}; shift 2 ;;
    --select-previous) SELECT_ONLY=true; shift ;;
    *) die 'usage: upgrade.sh [--image TAG] [--previous TAG] [--select-previous] | --self-test | --self-test-postgres' ;;
  esac
done
[[ "$IMAGE_TAG" =~ ^[a-zA-Z0-9_][a-zA-Z0-9_.-]*$ ]] || die 'invalid image tag'
CANDIDATE_VERSION=$IMAGE_TAG
if [[ "$IMAGE_TAG" == upgrade-drill ]]; then
  CANDIDATE_VERSION=$(sed -nE 's/^version *= *"([^"]+)".*/\1/p' "$ROOT/Cargo.toml" | head -1)
fi
semver_valid "$CANDIDATE_VERSION" || die 'candidate must be a version tag or upgrade-drill'

DRILL_WORK=$(mktemp -d)
export DRILL_WORK
PROJECT="zf-upgrade-$(date +%s)-$$"
COMPOSE_FILES=(-f "$ROOT/scripts/drill/compose.upgrade.yaml")
STARTED=false
STAGE=selection
CLEANUP_DIAGNOSTICS=after
CLEANUP_ERROR='::error::upgrade drill cleanup failed'
cleanup_diagnostics() {
  echo "::error::upgrade drill failed during $STAGE; raw output withheld" >&2
}
install_cleanup_traps

if [[ -z "$PREVIOUS" ]]; then
  # CD publishes v<version> releases and GHCR :<version>, never :v<version>.
  gh api --paginate "repos/${GITHUB_REPOSITORY:-Glubiz/zirv-flow}/releases?per_page=100" \
    > "$DRILL_WORK/releases" 2> "$DRILL_WORK/release-error"
  jq -rs '[.[][] | select(.draft == false and .prerelease == false) |
    select(.tag_name | test("^v[0-9]+\\.[0-9]+\\.[0-9]+$"))] |
    sort_by(.published_at) | reverse | .[].tag_name' "$DRILL_WORK/releases" > "$DRILL_WORK/tags"
  while IFS= read -r tag; do
    semver_compare "${tag#v}" "$CANDIDATE_VERSION"
    if [[ "$semver_result" == -1 ]]; then PREVIOUS=${tag#v}; break; fi
  done < "$DRILL_WORK/tags"
fi
if [[ -z "$PREVIOUS" ]]; then
  echo '::notice::upgrade drill skipped: no published previous non-prerelease release'
  [[ -z "${GITHUB_OUTPUT:-}" ]] || echo 'previous=' >> "$GITHUB_OUTPUT"
  exit 0
fi
[[ "$PREVIOUS" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die 'previous must be a stable version'
semver_compare "$PREVIOUS" "$CANDIDATE_VERSION"
[[ "$semver_result" == -1 ]] || die 'previous release must precede the candidate'
export DRILL_IMAGE="ghcr.io/glubiz/zirv-flow:$PREVIOUS"
if ! docker pull "$DRILL_IMAGE" > "$DRILL_WORK/pull" 2>&1; then
  if grep -Eq 'manifest unknown|manifest.*not found' "$DRILL_WORK/pull"; then
    echo '::notice::upgrade drill skipped: previous release image is not published'
    [[ -z "${GITHUB_OUTPUT:-}" ]] || echo 'previous=' >> "$GITHUB_OUTPUT"
    exit 0
  fi
  die 'previous image pull failed (not an absent manifest)'
fi
if [[ "$SELECT_ONLY" == true ]]; then
  [[ -z "${GITHUB_OUTPUT:-}" ]] || echo "previous=$PREVIOUS" >> "$GITHUB_OUTPUT"
  echo "Selected previous release $PREVIOUS"
  exit 0
fi
NEW_IMAGE="ghcr.io/glubiz/zirv-flow:$IMAGE_TAG"
if ! docker image inspect "$NEW_IMAGE" > /dev/null 2>&1; then
  docker pull "$NEW_IMAGE" > "$DRILL_WORK/pull" 2>&1
fi

STAGE=fixtures
fixture_tls upgrade-drill-ca DNS:fixture
STARTED=true
quiet_compose up -d --wait --wait-timeout 120 postgres fixture
wait_fixture '::error::upgrade drill: DNS/SMTP fixture did not start'
quiet_compose run --rm --no-deps zirv-flow migrate
quiet_compose up -d --no-build --wait --wait-timeout 120 zirv-flow
BASE="http://$(compose port zirv-flow 8080)"
CSRF=
KEY=
api() {
  local method=$1 path=$2 want=$3 body=${4:-} status
  local args=(-sS --max-time 30 -X "$method" -b "$DRILL_WORK/cookies" -c "$DRILL_WORK/cookies"
    -H "x-csrf-token: $CSRF" -H 'content-type: application/json')
  if [[ "$path" == /ingest/* ]]; then args+=(-H "authorization: Bearer $KEY"); fi
  if [[ -n "$body" ]]; then args+=(--data-binary "$body"); fi
  status=$(curl "${args[@]}" "$BASE/api/v1$path" -o "$DRILL_WORK/body" -w '%{http_code}' 2> "$DRILL_WORK/http-error")
  [[ "$status" == "$want" ]] || die "API status $status, expected $want during $STAGE"
  cat "$DRILL_WORK/body"
}
sql() { compose exec -T postgres psql -XqAt -v ON_ERROR_STOP=1 -U zirv -d zirv_flow "$@" 2> "$DRILL_WORK/sql-error"; }
snapshot() {
  sql -v schema=public -v verify="$1" < "$ROOT/scripts/drill/integrity.sql" || {
    integrity_error "$DRILL_WORK/sql-error"
    die "integrity check failed during $STAGE"
  }
}

STAGE=population_ingest
TOKEN=$(setup_token)
[[ -n "$TOKEN" ]] || die 'no setup token'
PASSWORD=$(openssl rand -hex 24)
SESSION=$(api POST /setup 200 "$(jq -n --arg token "$TOKEN" --arg password "$PASSWORD" \
  '{token:$token,email:"owner@example.com",password:$password}')")
CSRF=$(jq -er .csrf_token <<< "$SESSION")
OWNER=$(jq -er .user.id <<< "$SESSION")
WS=$(api POST /workspaces 201 '{"name":"Upgrade drill"}' | jq -er .id)
P="/workspaces/$WS"
api PUT "$P/members/$OWNER" 200 '{"role":"admin"}' > /dev/null
SOURCE=$(api POST "$P/sources" 201 \
  '{"name":"Drill file","kind":"file","config":{"url":"https://example.com/people.csv","format":"csv"}}' | jq -er .id)
KEY=$(api POST "$P/api-keys" 201 '{"name":"Drill ingestion","scopes":["ingest"]}' | jq -er .key)

# Twelve requests of 250 identify/track pairs: 6,000 inbox items, 3,000 people and API events.
for month in 1 2 3; do
  stamp=$(date -u -d "$(date -u +%Y-%m-15) -$month month" +%Y-%m-%dT12:00:00Z)
  for batch in $(seq 0 3); do
    body=$(jq -n --arg stamp "$stamp" --argjson start "$(((month - 1) * 1000 + batch * 250))" '
      {batch:[range($start; $start+250) as $n |
        {type:"identify",userId:("drill-user-"+($n|tostring)),messageId:("person-"+($n|tostring)),traits:{plan:"drill"}},
        {type:"track",userId:("drill-user-"+($n|tostring)),messageId:("event-"+($n|tostring)),
         event:"drill_event",timestamp:$stamp,properties:{quantity:1}}]}')
    api POST /ingest/batch 202 "$body" > /dev/null
  done
done
wait_sql 'SELECT count(*) FROM ingest_inbox' 0
if ! REJECTS=$(sql -c 'SELECT count(*) FROM ingest_rejects'); then
  checkpoint_failure 'SELECT count(*) FROM ingest_rejects' 0 'SQL query failed'
fi
[[ "$REJECTS" == 0 ]] || checkpoint_failure 'SELECT count(*) FROM ingest_rejects' 0 "$REJECTS"
wait_sql 'SELECT count(*) FROM people' 3000
wait_sql "$SEED_EVENT_COUNT" 3000
[[ "$(sql -c "SELECT count(DISTINCT tableoid) FROM events WHERE source = 'api' AND name = 'drill_event'")" == 3 ]] || die 'seed events did not span three partitions'
SURVIVOR=$(api POST "$P/people/search" 200 '{"identifier":{"kind":"external:user_id","value":"drill-user-0"}}' | jq -er .people[0].id)
ERASED=$(api POST "$P/people/search" 200 '{"identifier":{"kind":"external:user_id","value":"drill-user-1"}}' | jq -er .people[0].id)
LIST=$(api POST "$P/lists" 201 '{"name":"Drill audience"}' | jq -er .id)
api POST "$P/lists/$LIST/members" 200 "$(jq -n --arg id "$SURVIVOR" '{person_ids:[$id]}')" > /dev/null
api PUT "$P/people/$SURVIVOR/consent" 200 '{"channel":"email","status":"subscribed"}' > /dev/null
api PUT "$P/people/$SURVIVOR/consent" 200 '{"channel":"email","status":"unsubscribed"}' > /dev/null
STAGE=population_segment_refresh
SEGMENT=$(api POST "$P/segments" 201 '{"name":"Drill segment","definition":{"v":1,"all":[]}}' | jq -er .id)
api POST "$P/segments/$SEGMENT/refresh" 202 > /dev/null
wait_api "$P/segments/$SEGMENT" '.computed_version == .current_version and .member_count == 3000'
STAGE=population_erasure
api POST "$P/privacy/erasures" 202 "$(jq -n --arg id "$ERASED" '{person_id:$id}')" > /dev/null
wait_api "$P/privacy/erasures" '.erasures | length == 1 and all(.[]; .status == "completed")'
wait_sql 'SELECT count(*) FROM people' 2999
wait_sql "$SEED_EVENT_COUNT" 2999
api GET "$P/people/$ERASED" 404 > /dev/null

STAGE=sending_setup
DOMAIN_BODY=$(api POST "$P/sending-domains" 201 '{"domain":"drill.example.com"}')
DOMAIN=$(jq -er .id <<< "$DOMAIN_BODY")
jq '[.records[] | {key:.host,value:.expected_text}] | from_entries' <<< "$DOMAIN_BODY" > "$DRILL_WORK/records.new"
mv "$DRILL_WORK/records.new" "$DRILL_WORK/records.json"
api POST "$P/sending-domains/$DOMAIN/verify" 202 > /dev/null
wait_api "$P/sending-domains/$DOMAIN" '.status == "verified"'
SENDER=$(api POST "$P/sender-identities" 201 "$(jq -n --arg domain "$DOMAIN" \
  '{domain_id:$domain,from_name:"Drill",from_address:"sender@drill.example.com"}')" | jq -er .id)
CREDENTIAL=$(api POST "$P/credentials" 201 \
  '{"name":"Drill SMTP","kind":"smtp","fields":{"host":"fixture","port":2525,"tls":"tls","username":"drill"},"secret":{"password":"drill"}}' | jq -er .id)
DESTINATION=$(api POST "$P/destinations" 201 "$(jq -n --arg credential "$CREDENTIAL" --rawfile ca "$DRILL_WORK/ca.pem" \
  '{name:"Drill SMTP",kind:"smtp",credential_id:$credential,config:{ca_certificate:$ca}}')" | jq -er .id)
TEMPLATE=$(api POST "$P/templates" 201 "$(jq -n --arg sender "$SENDER" \
  '{name:"Drill template",channel:"email",category:"transactional",document:{schema:1,channel:"email",mode:"code",
    email:{subject:"Upgrade drill",sender:$sender,lang:"en",category:"transactional",html:"<p>Upgrade drill</p>",text:"Upgrade drill"}}}')" | jq -er .id)
api POST "$P/templates/$TEMPLATE/publish" 201 > /dev/null

# Last mutation before stopping: the enrollment must still be waiting in the old database.
STAGE=waiting_enrollment
FLOW=$(api POST "$P/flows" 201 '{"name":"Drill wait","graph":{"v":1,"nodes":[
  {"id":"wait","type":"wait.duration","seconds":120},{"id":"end","type":"exit"}],
  "edges":[{"id":"next","from":"wait","port":"next","to":"end"}]},
  "settings":{"trigger":{"type":"manual"},"reentry":{"mode":"once"}}}' | jq -er .id)
api POST "$P/flows/$FLOW/publish" 201 > /dev/null
api POST "$P/flows/$FLOW/start" 202 "$(jq -n --arg list "$LIST" '{list_id:$list}')" > /dev/null
wait_api "$P/flows/$FLOW/enrollments" '.enrollments | length == 1 and all(.[]; .state == "waiting")'
ENROLLMENT=$(api GET "$P/flows/$FLOW/enrollments" 200 | jq -er .enrollments[0].id)
quiet_compose stop zirv-flow
[[ "$(sql -c "SELECT count(*) FROM enrollments WHERE state = 'waiting'")" == 1 ]] || die 'old enrollment was not waiting at shutdown'
snapshot false > "$DRILL_WORK/before.json"
BASELINE_SCHEMA=$(sql -c 'SELECT max(version) FROM _sqlx_migrations')
[[ "$BASELINE_SCHEMA" =~ ^[0-9]+$ ]] || die 'invalid baseline schema version'

STAGE=migration
export DRILL_IMAGE=$NEW_IMAGE
MIGRATE_START=$(date +%s)
quiet_compose run --rm --no-deps zirv-flow migrate
MIGRATE_SECONDS=$(($(date +%s) - MIGRATE_START))
quiet_compose run --rm --no-deps zirv-flow migrate
grep -Fxq 'database schema is up to date' "$DRILL_WORK/compose.log" || die 'second migrate did not report up to date'
snapshot true > "$DRILL_WORK/after.json"
compare_counts "$DRILL_WORK/before.json" "$DRILL_WORK/after.json" || die 'migration lost rows or tables'
[[ "$(sql -v erased="$ERASED" <<'SQL'
SELECT count(*) FROM people WHERE id = :'erased'::uuid;
SQL
)" == 0 ]] || die 'erased person reappeared'

{
  printf "## Upgrade drill\n\nPrevious: \`%s\`; candidate: \`%s\`.\n\n" "$PREVIOUS" "$CANDIDATE_VERSION"
  printf 'Migration wall time: %ss. Seed: 3,000 people and 3,000 events in three partitions; one erased before the baseline.\n\n' "$MIGRATE_SECONDS"
  printf '| Table | Before migration | After migration |\n| --- | ---: | ---: |\n'
  jq -r --slurpfile before "$DRILL_WORK/before.json" 'to_entries[] |
    "| \(.key) | \($before[0][.key] // "new") | \(.value) |"' "$DRILL_WORK/after.json"
  printf '\n| Migration applied during upgrade | Seconds recorded by sqlx |\n| --- | ---: |\n'
  sql -c "SELECT '| ' || version || ' | ' || round(execution_time::numeric / 1000000000, 6) || ' |' FROM _sqlx_migrations WHERE version > $BASELINE_SCHEMA ORDER BY version"
} >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"

STAGE=post_upgrade
quiet_compose up -d --no-build --wait --wait-timeout 120 zirv-flow
BASE="http://$(compose port zirv-flow 8080)"
curl -fsS "$BASE/readyz" > /dev/null 2> "$DRILL_WORK/http-error"
curl -fsS "$BASE/" 2> "$DRILL_WORK/http-error" | grep -q 'id="root"'
api GET /version 200 | jq -e --arg version "$CANDIDATE_VERSION" '.version == $version' > /dev/null
wait_sql 'SELECT count(*) FROM people' 2999
wait_sql "$SEED_EVENT_COUNT" 2999
api GET "$P/people/$ERASED" 404 > /dev/null
api GET "$P/people/$SURVIVOR" 200 > /dev/null
wait_api "$P/privacy/erasures" '.erasures | length == 1 and all(.[]; .status == "completed")'
api GET "$P/sources/$SOURCE" 200 > /dev/null
api GET "$P/lists/$LIST/members" 200 | jq -e --arg person "$SURVIVOR" \
  '.members | length == 1 and .[0].person_id == $person' > /dev/null
api GET "$P/people/$SURVIVOR/consent" 200 | jq -e \
  '.states | any(.channel == "email" and .topic_key == null and .status == "unsubscribed")' > /dev/null
api GET "$P/templates/$TEMPLATE/versions/1" 200 > /dev/null
api GET "$P/sending-domains/$DOMAIN" 200 | jq -e '.status == "verified"' > /dev/null
BEFORE_REFRESH=$(api GET "$P/segments/$SEGMENT" 200 | jq -r .computed_at)
api POST "$P/segments/$SEGMENT/refresh" 202 > /dev/null
wait_api "$P/segments/$SEGMENT" ".computed_at != \"$BEFORE_REFRESH\" and .computed_version == .current_version and .member_count == 2999"
wait_api "$P/flows/$FLOW/enrollments/$ENROLLMENT" '.enrollment.state == "completed"'
api POST "$P/templates/$TEMPLATE/test-send" 202 "$(jq -n --arg destination "$DESTINATION" --arg sender "$SENDER" --arg member "$OWNER" \
  '{version:1,destination_id:$destination,sender_identity_id:$sender,to:{member:$member}}')" > /dev/null
wait_api "$P/test-sends" '.test_sends | length == 1 and all(.[]; .outcome == "sent")'
[[ "$(cat "$DRILL_WORK/arrivals")" == 1 ]] || die 'SMTP sink did not accept exactly one test send'
printf '\nIntegrity, erasure, segment refresh, timer wake, SMTP delivery, UI and version: PASS.\n' >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"
echo 'Upgrade drill passed'
