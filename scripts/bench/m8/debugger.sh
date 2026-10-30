#!/usr/bin/env bash
# Watch before offering load; SQL must run on this host for its clock/hostname gates.
# shellcheck source=../m4/common.sh
. "$(dirname "${BASH_SOURCE[0]}")/../m4/common.sh"
# shellcheck source=common.sh
. "$root/scripts/bench/m8/common.sh"

raw="$1"; pool="$2"
host_pg_setup
clients="${M8_DEBUGGER_CLIENTS:-1}"
seconds="$M8_SECONDS"
connect="${M8_DEBUGGER_CONNECT_SECONDS:-15}"
drain="${M8_DRAIN_SECONDS:-60}"
if [ -n "${M8_SMOKE:-}" ]; then clients=1; seconds=5; connect=5; drain=5; fi
for name in clients seconds connect drain; do positive_integer "$name" "${!name}"; done
[ "$clients" -le 20 ] || die 'M8_DEBUGGER_CLIENTS must be in 1..20 (per-workspace stream cap)'
loadgen="${LOADGEN:-$root/target/release/loadgen}"
[ -x "$loadgen" ] || die "no prebuilt loadgen at $loadgen"
ws="$(state_get M4_WS)"
[ -n "$ws" ] || die 'no workspace; seed people first'
login || die 'cannot sign in to create the debugger read key'
if [ ! -s "$M4_STATE/debugger.key" ]; then
  (umask 077; api_json POST "/workspaces/$ws/api-keys" '{"name":"m8-debugger","scopes":["read"]}' \
    | jq -er .key >"$M4_STATE/debugger.key")
fi

watcher=''
trap 'if [ -n "$watcher" ]; then kill "$watcher" 2>/dev/null || true; wait "$watcher" 2>/dev/null || true; fi' EXIT
client_count() {
  curl -fsS --connect-timeout 5 --max-time 10 http://127.0.0.1:8080/metrics \
    | awk '$1 == "zirv_flow_debugger_clients" {n = $2} END {print n+0}'
}
[ "$(client_count)" = 0 ] || die 'close existing debugger streams before the measurement'
wait_inbox "$drain"
"$loadgen" debugger --url http://127.0.0.1:8080 --key-file "$M4_STATE/debugger.key" \
  --workspace "$ws" --clients "$clients" --duration "$((connect + seconds + drain + 2))" \
  --out "$raw/receive.jsonl" >"$raw/watcher.log" 2>&1 &
watcher=$!
deadline=$((SECONDS + connect))
until [ "$(client_count)" = "$clients" ]; do
  kill -0 "$watcher" 2>/dev/null || die 'debugger watcher exited; see watcher.log'
  [ "$SECONDS" -lt "$deadline" ] || die 'debugger clients did not connect before the timeout'
  sleep 0.2
done
track_load "$seconds" "$pool" "$raw/ingest.json" >"$raw/ingest.log" 2>&1
wait_inbox "$drain"
kill -0 "$watcher" 2>/dev/null || die 'watchers ended before the inbox drained; increase M8_DRAIN_SECONDS'
# Await normal completion: killing loadgen would lose its buffered receipts and final records.
wait "$watcher"
watcher=''
scrape_metrics http://127.0.0.1:8080/metrics \
  zirv_flow_debugger_clients,zirv_flow_debugger_delivery_seconds "$raw/metrics"
host_pg --csv -v receive_file="$raw/receive.jsonl" -f "$m8_dir/debugger-latency.sql" >"$raw/latency.csv"
python3 -I - "$raw" "$seconds" "$clients" <<'PY'
import csv, json, pathlib, sys
path = pathlib.Path(sys.argv[1])
clock = None
latency = []
header = None
for row in csv.reader((path / 'latency.csv').open()):
    if not row:
        continue
    if row[0] in ('client_started_at', 'client'):
        header = row
        continue
    if header is None or len(row) != len(header):
        raise SystemExit('unexpected debugger SQL output')
    item = dict(zip(header, row))
    if header[0] == 'client_started_at':
        clock = {k: float(v) if k.endswith('_ms') else v for k, v in item.items()}
    else:
        latency.append({k: v if k == 'client' else float(v) if v else None for k, v in item.items()})
counts = []
with (path / 'receive.jsonl').open() as receipts:
    for line in receipts:
        record = json.loads(line)
        if record['kind'] == 'client':
            counts.append({'client': record['client'], **record['counts']})
if clock is None or len(latency) != int(sys.argv[3]) + 1 or len(counts) != int(sys.argv[3]):
    raise SystemExit('incomplete debugger report')
report = dict(load_seconds=int(sys.argv[2]), clients=int(sys.argv[3]), clock=clock,
              latency=latency, client_counts=counts,
              gap_frames=sum(c['gap_frames'] for c in counts), skipped=sum(c['skipped'] for c in counts))
(path / 'debugger.json').write_text(json.dumps(report, indent=2) + '\n')
PY
