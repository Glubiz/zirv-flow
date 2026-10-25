#!/usr/bin/env bash
# The bench SMTP sink on the host (tools/loadgen `sendbench sink`), standing in for a provider.
# Containers reach it as host.docker.internal:$M6_SINK_PORT (the host gateway), under the egress
# allowlist of compose.m6.yaml. It offers STARTTLS with a generated authority; its PEM is
# $M6_OUT/sink/ca.pem and goes into the destination's `ca_certificate` (setup.sh does that).
# It appends one JSON line per accepted message to $M6_OUT/sink/arrivals.ndjson; run.sh cuts
# the lines of one run by time.
#
# Usage: scripts/bench/m6/sink.sh start|stop|status
#   M6_SINK_DELAY_MS   milliseconds added to every final reply, default 0

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
pidfile="$M6_SINK_DIR/sink.pid"
case "${1:-}" in
  start)
    [ -x "$SENDBENCH" ] || die "no sendbench at $SENDBENCH; cargo build --release -p zirv-flow-loadgen"
    if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then log "sink already running"; exit 0; fi
    nohup "$SENDBENCH" sink --listen "0.0.0.0:$M6_SINK_PORT" --dir "$M6_SINK_DIR" \
      --log "$M6_SINK_DIR/arrivals.ndjson" --name "$M6_SINK_HOST" --name mail.bench.test \
      --response-delay-ms "${M6_SINK_DELAY_MS:-0}" >"$M6_SINK_DIR/sink.out" 2>"$M6_SINK_DIR/sink.err" &
    echo "$!" >"$pidfile"
    for _ in $(seq 1 50); do [ -s "$M6_SINK_DIR/ca.pem" ] && break; sleep 0.2; done
    [ -s "$M6_SINK_DIR/ca.pem" ] || die "the sink did not start; see $M6_SINK_DIR/sink.err"
    log "sink up on port $M6_SINK_PORT (pid $(cat "$pidfile"))" ;;
  stop)
    [ -f "$pidfile" ] && kill "$(cat "$pidfile")" 2>/dev/null || true
    rm -f "$pidfile" ;;
  status)
    if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
      echo "running pid $(cat "$pidfile"), $(wc -l <"$M6_SINK_DIR/arrivals.ndjson" 2>/dev/null || echo 0) arrivals logged"
    else echo stopped; fi ;;
  *) echo "usage: sink.sh start|stop|status" >&2; exit 2 ;;
esac
