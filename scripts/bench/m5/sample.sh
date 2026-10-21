#!/usr/bin/env bash
# Enrolls a small sample through the product path (the 0.76.0 image: POST .../flows/{id}/start for
# a list, the flow.enroll job, the flow.step start job) and captures the rows the runtime leaves
# behind, as the shape seed.sh copies (supervisor ruling 46fb269a): per flow, the enrollments,
# step_executions, jobs, flow_entries and decision_records of the sample people, as JSON in
# $M4_OUT/sample/<flow>.json. m5-probe's 20 s waits also wake, and probe-lateness.json holds their
# lateness, which proves the lateness query on rows the product wrote itself.
#
# Run setup.sh first. The sample people are the first SAMPLE_PEOPLE people of the workspace by id
# (seed.sh takes everyone after the first 100). Safe to re-run: a person already enrolled in a
# flow is not enrolled again (re-entry once), and the capture is rewritten.
#
# Usage: scripts/bench/m5/sample.sh

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

ws="$(state_get M4_WS)"
[ -n "$ws" ] || die "no workspace in the state file"
login || die "cannot sign in; run stack.sh up"
n="${SAMPLE_PEOPLE:-3}"
mkdir -p "$M4_OUT/sample"

people_ids() { # OFFSET COUNT -> JSON array of person ids
  pg -At -v ws="$ws" -v o="$1" -v c="$2" -f - <<'SQL'
SELECT coalesce(json_agg(id), '[]') FROM (
  SELECT id FROM people WHERE workspace_id = :'ws' AND merged_into IS NULL ORDER BY id OFFSET :o LIMIT :c) t;
SQL
}

enroll_sample() { # NAME FLOW_KEY LIST_KEY OFFSET COUNT
  local name="$1" flow list ids
  flow="$(state_get "$2")"
  list="$(state_get "$3")"
  [ -n "$flow" ] && [ -n "$list" ] || die "run setup.sh first"
  ids="$(people_ids "$4" "$5")"
  api_json POST "/workspaces/$ws/lists/$list/members" "$(jq -nc --argjson p "$ids" '{person_ids: $p}')" >/dev/null
  api_json POST "/workspaces/$ws/flows/$flow/start" "$(jq -nc --arg l "$list" '{list_id: $l}')" >/dev/null
  log "$name: started for $5 people"
}

enroll_sample spread M5_FLOW_SPREAD M5_LIST_SPREAD 0 "$n"
enroll_sample burst M5_FLOW_BURST M5_LIST_BURST 10 "$n"
enroll_sample probe M5_FLOW_PROBE M5_LIST_PROBE 20 5

# Wait for every sample enrollment to be waiting (or the probes already finished).
deadline=$(( $(date +%s) + 300 ))
while :; do
  open="$(pg -At -v ws="$ws" -f - <<'SQL'
SELECT count(*) FROM enrollments WHERE workspace_id = :'ws' AND state = 'active';
SQL
)"
  [ "$open" = 0 ] && break
  [ "$(date +%s)" -lt "$deadline" ] || die "$open sample enrollments are still active after 300 s"
  sleep 2
done

# capture NAME FLOW_KEY: one JSON document per flow with the rows of the first enrollments.
capture() {
  local name="$1" flow
  flow="$(state_get "$2")"
  pg -At -v flow="$flow" -f - <<'SQL' | jq . >"$M4_OUT/sample/$name.json"
WITH e AS (SELECT * FROM enrollments WHERE flow_id = :'flow' ORDER BY created_at LIMIT 3)
SELECT json_build_object(
  'enrollments', (SELECT coalesce(json_agg(to_jsonb(e) ORDER BY created_at), '[]') FROM e),
  'step_executions', (SELECT coalesce(json_agg(to_jsonb(s) ORDER BY started_at), '[]')
                      FROM step_executions s WHERE s.enrollment_id IN (SELECT id FROM e)),
  'jobs', (SELECT coalesce(json_agg(to_jsonb(j) - 'lease_token' ORDER BY created_at), '[]')
           FROM jobs j WHERE j.kind LIKE 'flow.%' AND j.payload ->> 'enrollment_id' IN (SELECT id::text FROM e)),
  'flow_entries', (SELECT coalesce(json_agg(to_jsonb(f)), '[]') FROM flow_entries f
                   WHERE f.flow_id = :'flow' AND f.person_id IN (SELECT person_id FROM e)),
  'decision_records', (SELECT coalesce(json_agg(to_jsonb(d) ORDER BY recorded_at), '[]')
                       FROM decision_records d WHERE d.trace_id IN (SELECT trace_id FROM e))
);
SQL
  jq -c '{flow: $n, enrollments: (.enrollments | length), steps: (.step_executions | length), jobs: (.jobs | length), decisions: (.decision_records | length)}' --arg n "$name" "$M4_OUT/sample/$name.json"
}
capture spread M5_FLOW_SPREAD
capture burst M5_FLOW_BURST
capture probe M5_FLOW_PROBE
