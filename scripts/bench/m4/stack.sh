#!/usr/bin/env bash
# The M4 benchmark stack (issue #127): PostgreSQL 16, serve, worker and scheduler from the
# published image, in the compose project zirv-flow-m4bench, at app tier M by default. It layers
# docker-compose.yml, compose.bench.yaml, compose.tiers.yaml and m4/compose.m4.yaml (the image
# override), starts with --no-build and never builds. It never touches the `zirv-flow-bench`
# project or any other container.
#
#   up       start the stack on its volumes, wait for /readyz, create the bench owner on the
#            first start (the password is generated into $M4_STATE/owner.pass, mode 600, and is
#            never printed), sign in to prove it works, and write the stack facts to
#            $M4_OUT/stack.txt (image digest, cpusets, memory, PostgreSQL settings, versions)
#   stop     stop the containers and keep the volumes (phase 2 reuses the seeded database)
#   down     remove the containers and keep the volumes
#   reset    remove the containers AND the volumes of this project (asks for M4_CONFIRM_RESET=1)
#   status   compose ps
#
# Environment: see common.sh (M4_PROJECT, M4_STATE, M4_OUT, M4_TIER, BENCH_IMAGE_TAG, ...). The
# image must be pulled first: docker pull ghcr.io/glubiz/zirv-flow:$BENCH_IMAGE_TAG
#
# Usage: scripts/bench/m4/stack.sh up|stop|down|reset|status

# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"

record_stack() {
  local out="$M4_OUT/stack.txt" image
  image="ghcr.io/glubiz/zirv-flow:$BENCH_IMAGE_TAG"
  {
    echo "project=$M4_PROJECT tier=$M4_TIER app_cpuset=$BENCH_APP_CPUSET pg_cpuset=${BENCH_PG_CPUSET:-8-31}"
    echo "recorded_at=$(date -u +%FT%TZ)"
    echo "image=$image id=$(docker image inspect "$image" --format '{{.Id}}') digest=$(docker image inspect "$image" --format '{{index .RepoDigests 0}}') created=$(docker image inspect "$image" --format '{{.Created}}')"
    # shellcheck disable=SC2046
    docker inspect --format '{{.Name}} image={{.Config.Image}} cpuset={{.HostConfig.CpusetCpus}} mem_limit_bytes={{.HostConfig.Memory}} shm_bytes={{.HostConfig.ShmSize}}' \
      $(compose ps -q) | sort
    for s in server_version shared_buffers effective_cache_size maintenance_work_mem max_connections \
      synchronous_commit wal_level fsync work_mem max_wal_size checkpoint_timeout max_parallel_workers_per_gather \
      max_parallel_workers max_worker_processes random_page_cost; do
      echo "pg.$s=$(pg -At -c "show $s")"
    done
    for service in zirv-flow zirv-flow-worker zirv-flow-scheduler; do
      docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' "$(compose ps -q "$service")" \
        | grep '^ZIRV_FLOW_' | grep -v -E 'URL|SECRET|MASTER' | sed "s/^/$service /" | sort
    done
    echo "app.version=$(curl -fsS "$api_base/api/v1/version" 2>/dev/null | jq -c . 2>/dev/null || echo unknown)"
    echo "docker=$(docker version --format '{{.Server.Version}}' 2>/dev/null) desktop_cpus=$(docker info --format '{{.NCPU}}') desktop_mem_bytes=$(docker info --format '{{.MemTotal}}')"
    echo "host_cpu=$(powershell.exe -NoProfile -Command '(Get-CimInstance Win32_Processor).Name' 2>/dev/null | tr -d '\r' | head -n 1 || true)"
    echo "wsl=$(wsl.exe --version 2>/dev/null | tr -d '\0\r' | head -n 3 | tr '\n' ' ' || true)"
  } >"$out" 2>&1
  log "stack facts in $out"
}

up() {
  docker image inspect "ghcr.io/glubiz/zirv-flow:$BENCH_IMAGE_TAG" >/dev/null 2>&1 \
    || die "image ghcr.io/glubiz/zirv-flow:$BENCH_IMAGE_TAG is not pulled; pull it, never build it"
  compose up -d --no-build --wait --wait-timeout 300 >&2
  until curl -fsS "$api_base/readyz" >/dev/null 2>&1; do sleep 1; done
  if [ ! -f "$M4_STATE/owner.pass" ]; then
    ( umask 077; head -c 24 /dev/urandom | base64 | tr -d '/+=\n' >"$M4_STATE/owner.pass" )
  fi
  if ! login; then
    log "creating the bench owner $M4_EMAIL"
    compose exec -T zirv-flow zirv-flow admin create-owner --email "$M4_EMAIL" <"$M4_STATE/owner.pass" >/dev/null
    login || die "the bench owner cannot sign in"
  fi
  log "signed in as the bench owner (session headers in $M4_STATE/auth.hdr)"
  record_stack
}

case "${1:-}" in
  up) up ;;
  stop) compose stop >&2 ;;
  down) compose down >&2 ;;
  reset)
    [ "${M4_CONFIRM_RESET:-0}" = 1 ] || die "reset removes this project's volumes; set M4_CONFIRM_RESET=1"
    compose down -v >&2
    rm -f "$M4_STATE/owner.pass" "$M4_STATE/auth.hdr" "$M4_STATE/state.env" "$M4_STATE/loadgen.key"
    ;;
  status) compose ps ;;
  *) echo "usage: stack.sh up|stop|down|reset|status" >&2; exit 2 ;;
esac
