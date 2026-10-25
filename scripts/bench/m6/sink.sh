#!/usr/bin/env bash
# The bench SMTP sink (tools/loadgen `sendbench sink`), standing in for a provider, as a container
# on the bench project's network: the workers reach it as $M6_SINK_HOST:$M6_SINK_PORT (default
# zirv-m6-sink:2525), inside the egress allowlist of compose.m6.yaml. A sink on the host reached
# through host.docker.internal was tried first and rejected: through Docker Desktop's host path
# new connections stalled for 10 s or more at concurrency 2 and above (see
# docs/benchmarks/m6-send.md), which would have measured the host path, not the product.
#
# The container runs a Linux build of sendbench (no image is pulled or built: the local
# rust:1-bookworm image compiles it, debian:bookworm-slim runs it). Build it once:
#
#   MSYS_NO_PATHCONV=1 docker run --rm -v '<worktree>:/src:ro' -v m6-sendbench-target:/target \
#     -e CARGO_TARGET_DIR=/target -w /src rust:1-bookworm \
#     cargo build --release --locked -p zirv-flow-loadgen --bin sendbench
#   (then copy /target/release/sendbench to $M6_OUT/linux/sendbench; `sink.sh build` does both)
#
# It offers STARTTLS with a generated authority; its PEM is $M6_SINK_DIR/ca.pem (a bind mount) and
# goes into the destination's `ca_certificate` (setup.sh and run.sh do that). It appends one JSON
# line per accepted message to /var/arrivals.ndjson inside the container (run.sh reads it with
# `docker exec`); a container's own filesystem keeps a slow host file system out of the path.
# The container is pinned to the CPUs $M6_SINK_CPUSET (default 4-7, outside the app's 0-3 and the
# database's 8-31) and its CPU is sampled with `docker stats`.
#
# Usage: scripts/bench/m6/sink.sh build|start|stop|status
#   M6_SINK_DELAY_MS   milliseconds added to every final reply, default 0
#   M6_SINK_CPUSET     default 4-7

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
name="$M6_SINK_CONTAINER"
linux_dir="$M6_OUT/linux"
win() { cygpath -w "$1" 2>/dev/null || echo "$1"; }

case "${1:-}" in
  build)
    mkdir -p "$linux_dir"
    MSYS_NO_PATHCONV=1 docker run --rm -v "$(win "$m6_root"):/src:ro" -v m6-sendbench-target:/target \
      -e CARGO_TARGET_DIR=/target -w /src rust:1-bookworm \
      cargo build --release --locked -p zirv-flow-loadgen --bin sendbench >&2
    MSYS_NO_PATHCONV=1 docker run --rm -v m6-sendbench-target:/target:ro -v "$(win "$linux_dir"):/out" \
      debian:bookworm-slim cp /target/release/sendbench /out/sendbench ;;
  start)
    [ -s "$linux_dir/sendbench" ] || die "no Linux sendbench in $linux_dir; run sink.sh build"
    if [ "$(docker inspect -f '{{.State.Running}}' "$name" 2>/dev/null)" = true ]; then log "sink already running"; exit 0; fi
    docker rm -f "$name" >/dev/null 2>&1 || true
    rm -f "$M6_SINK_DIR/ca.pem"
    MSYS_NO_PATHCONV=1 docker run -d --name "$name" --network "${M4_PROJECT}_default" \
      --cpuset-cpus "${M6_SINK_CPUSET:-4-7}" -v "$(win "$linux_dir"):/opt/sink:ro" -v "$(win "$M6_SINK_DIR"):/data" \
      debian:bookworm-slim /opt/sink/sendbench sink --listen "0.0.0.0:$M6_SINK_PORT" --dir /data \
      --log /var/arrivals.ndjson --name "$M6_SINK_HOST" --name mail.bench.test \
      --response-delay-ms "${M6_SINK_DELAY_MS:-0}" >/dev/null
    for _ in $(seq 1 50); do [ -s "$M6_SINK_DIR/ca.pem" ] && break; sleep 0.2; done
    [ -s "$M6_SINK_DIR/ca.pem" ] || die "the sink did not start; docker logs $name"
    log "sink up as $name:$M6_SINK_PORT" ;;
  stop)
    docker rm -f "$name" >/dev/null 2>&1 || true ;;
  status)
    if [ "$(docker inspect -f '{{.State.Running}}' "$name" 2>/dev/null)" = true ]; then
      echo "running, $(MSYS_NO_PATHCONV=1 docker exec "$name" wc -l /var/arrivals.ndjson 2>/dev/null | awk '{print $1}') arrivals logged"
    else echo stopped; fi ;;
  *) echo "usage: sink.sh build|start|stop|status" >&2; exit 2 ;;
esac
