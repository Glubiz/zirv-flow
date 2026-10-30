#!/usr/bin/env bash
# Per-kind claims, completed attempts and lag while an existing run is active (#207, #396).
# Needs psql (standard PG* connection variables), curl, awk and jq; starts no services.
# Usage: PGDATABASE=zirv_flow scripts/bench/m8/sample-job-kinds.sh [WORKER_METRICS_URL ...] >job-kinds.csv
# Pass every worker endpoint once; default http://127.0.0.1:9100/metrics. Stop with Ctrl-C.
# JOB_KINDS_TICK_S defaults to 5 (sleep after each sample). Failed SQL/scrapes stop the sampler.
# Metrics are cumulative since worker start, summed across workers: start a new file on restart.
# Delta(claim_lag_seconds_sum) / delta(claims_total) gives interval mean claim lag; no claims = null.
set -euo pipefail

sampler_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tick="${JOB_KINDS_TICK_S:-5}"
: "${PGDATABASE:?set PGDATABASE and the other PG* connection variables for the benchmark database}"
[[ "$tick" =~ ^[0-9]+([.][0-9]+)?$ && "$tick" =~ [1-9] ]] || {
  echo "JOB_KINDS_TICK_S must be a positive number" >&2
  exit 2
}
if [ "$#" -eq 0 ]; then
  set -- http://127.0.0.1:9100/metrics
fi

printf '%s\n' 't,kind,claims_total,completed_total,claim_lag_seconds_sum,due_ready,running,ready_lag_mean_seconds,ready_lag_max_seconds'
while :; do
  queue="$(PGCONNECT_TIMEOUT=10 PGOPTIONS="${PGOPTIONS:-} -c statement_timeout=10000 -c default_transaction_read_only=on" \
    psql -XAt -v ON_ERROR_STOP=1 -f "$sampler_dir/job-kinds.sql")"
  # The claim-lag histogram records once per claim, including attempts that later retry.
  # The completion counter selects only outcome="completed". No scheduler scrape is needed.
  workers="$(
    for url in "$@"; do
      curl -fsS --connect-timeout 5 --max-time 10 "$url" || exit 1
      printf '\n'
    done | awk '
      /^zirv_flow_job_claim_lag_seconds_(count|sum)\{/ || /^zirv_flow_jobs_total\{/ {
        if (!match($0, /[{,]kind="[^"]+"/)) next
        kind = substr($0, RSTART + 7, RLENGTH - 8)
        value = $0
        sub(/^[^}]*}[[:space:]]+/, "", value)
        if ($0 ~ /^zirv_flow_job_claim_lag_seconds_count\{/) {
          claims[kind] += value
          seen[kind] = 1
        } else if ($0 ~ /^zirv_flow_job_claim_lag_seconds_sum\{/) {
          lag[kind] += value
          seen[kind] = 1
        } else if ($0 ~ /[{,]outcome="completed"[,}]/) {
          completed[kind] += value
          seen[kind] = 1
        }
      }
      END {
        for (kind in seen)
          printf "%s\t%.17g\t%.17g\t%.17g\n", kind, claims[kind], completed[kind], lag[kind]
      }
    ' | jq -Rsc 'split("\n")[:-1] | map(split("\t") | {
      kind: .[0], claims_total: (.[1] | tonumber), completed_total: (.[2] | tonumber),
      claim_lag_seconds_sum: (.[3] | tonumber)
    })'
  )"
  jq -nr --argjson queue "$queue" --argjson workers "$workers" '
    ($queue.kinds + $workers) | group_by(.kind)[] | add
    | [$queue.t, .kind, (.claims_total // 0), (.completed_total // 0),
       (.claim_lag_seconds_sum // 0), (.due_ready // 0), (.running // 0),
       .ready_lag_mean_seconds, .ready_lag_max_seconds] | @csv
  '
  sleep "$tick"
done
