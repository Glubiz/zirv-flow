#!/usr/bin/env bash
# The burst arm (section 19): batch load at 10,000 items/s for M8_BURST_SECONDS (600), then the time the inbox
# takes to drain to empty from the end of the burst, capped at 30 minutes; the target is 15 minutes.
# run_bench raised the per-key limit first (raise_serve_limits) and marks a shortened burst as smoke.
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
raw="$1"; pool="$2"
host_pg_setup
loadgen="${LOADGEN:-$root/target/release/loadgen}"
[ -x "$loadgen" ] || die "no prebuilt loadgen at $loadgen"
seconds="$(burst_seconds)"
cap="${M8_BURST_DRAIN_CAP_SECONDS:-1800}"
positive_integer M8_BURST_DRAIN_CAP_SECONDS "$cap"
read -r rate_limit burst_limit <<<"$(serve_limits)"
[ "$rate_limit" = 20000 ] && [ "$burst_limit" = 40000 ] ||
  die "serve per-key limits are ${rate_limit:-unset}/${burst_limit:-unset}, not the raised 20000/40000"
shortened=false; ! burst_shortened || shortened=true
start_inbox_sampler "$raw/inbox-samples.csv"
rc=0
(exec "$loadgen" --url http://127.0.0.1:8080 --key-file "$M4_STATE/loadgen.key" --mode batch --batch-size 100 \
  --burst-rate 10000 --burst-duration "$seconds" --people "$pool" --out "$raw/burst-load.json") || rc=$?
if [ "$rc" != 0 ]; then
  stop_inbox_sampler
  die "loadgen exited $rc"
fi
drained=0
export M8_INBOX_PEAK_FILE="$raw/inbox-peak"
drain="$(drain_inbox "$cap")" || drained=$?
stop_inbox_sampler
python3 -I - "$raw" "$drained" "${drain:-}" "$cap" "$seconds" "$shortened" "$rate_limit" "$burst_limit" <<'PY' || die "inbox did not drain within ${cap}s of the burst, or the drain count could not be read"
import json, os, pathlib, sys
raw, drained, drain, cap, seconds, shortened, rate_limit, burst_limit = sys.argv[1:]
raw = pathlib.Path(raw)
drained, cap, seconds = int(drained), int(cap), int(seconds)
total = json.loads((raw / 'burst-load.json').read_text())['total']
peaks = [0]
for name in ('inbox-peak',):
    if (raw / name).exists():
        peaks.append(int((raw / name).read_text().strip() or 0))
if (raw / 'inbox-samples.csv').exists():
    for line in (raw / 'inbox-samples.csv').read_text().splitlines():
        parts = line.split(',')
        if len(parts) == 2 and parts[1].isdigit():
            peaks.append(int(parts[1]))
status = total['status']
bad = (status['rate_limited_429'] + status['server_5xx'] + status['other'] + status['transport_errors']
       + total['gave_up_after_retries'] + total['abandoned_requests'])
timed_out = drained == 1
out = dict(measure='burst', target_rate=10000, target_burst_seconds=600, burst_seconds=seconds,
           shortened=shortened == 'true', batch_size=100, rate_per_key=int(rate_limit), burst_per_key=int(burst_limit),
           achieved_rate=total['achieved_rate'], items_offered=total['items_offered'], items_accepted=total['items_accepted'],
           api_latency_ms={k: total['latency_from_schedule'][k] for k in ('p50_ms', 'p95_ms', 'p99_ms', 'max_ms')},
           rejected_or_failed=dict(status, gave_up_after_retries=total['gave_up_after_retries'],
                                   abandoned_requests=total['abandoned_requests'], total=bad),
           inbox_backlog_peak_rows=max(peaks), drain_seconds=None if drained else int(drain),
           drain_cap_seconds=cap, drain_timed_out=timed_out, drain_target_seconds=900)
out['rate_held'] = total['achieved_rate'] >= 0.98 * out['target_rate'] and bad == 0
out['drain_within_15_min'] = drained == 0 and int(drain) <= 900
out['passed'] = out['rate_held'] and out['drain_within_15_min']
(raw / 'burst.json').write_text(json.dumps(out, indent=2) + '\n')
sys.exit(0 if drained == 0 else 1)
PY
