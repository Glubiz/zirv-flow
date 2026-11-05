#!/usr/bin/env bash
# The PostgreSQL container's cgroup io.stat and swap, once per M8_IO_DIAG_TICK_S (1) seconds (M8_IO_DIAG, #397).
# Read INSIDE the container with docker exec: under Docker Desktop on WSL2 the VM's cgroup tree is not mounted on
# the host, and a container sees its own cgroup at /sys/fs/cgroup (v2 io.stat; v1 blkio as the fallback).
# /proc/meminfo inside the container is the VM's, so SwapTotal/SwapFree are VM-wide. Output blocks:
#   @@t <UTC time>, the io lines, @@mem, memory.swap.current (if any) and the Swap lines, @@end
export MSYS_NO_PATHCONV=1
container="${M4_PROJECT:-zirv-flow-m8}-postgres-1"
echo "@@source docker exec $container: /sys/fs/cgroup/io.stat (cgroup v2) or blkio.throttle.io_service_bytes (v1); swap from /proc/meminfo"
while :; do
  echo "@@t $(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)"
  docker exec "$container" sh -c 'cat /sys/fs/cgroup/io.stat 2>/dev/null || cat /sys/fs/cgroup/blkio/blkio.throttle.io_service_bytes 2>/dev/null; echo @@mem; cat /sys/fs/cgroup/memory.swap.current 2>/dev/null; grep -E "^Swap(Total|Free)" /proc/meminfo; echo @@end' 2>/dev/null || echo '@@end'
  sleep "${M8_IO_DIAG_TICK_S:-1}"
done
