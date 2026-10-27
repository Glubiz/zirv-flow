#!/usr/bin/env bash
# Emit GitHub step outputs; the workflow consumes these exact flags and tags.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/semver-compare.sh"

release_flags() {
  local version="$1" image="$2" prerelease=false alias=latest
  semver_valid "$version" || { echo 'invalid release SemVer' >&2; return 2; }
  if [[ "${version%%+*}" == *-* ]]; then
    prerelease=true
    alias=rc
    echo 'publish_flags=--draft=false --prerelease --latest=false'
  else
    echo 'publish_flags=--draft=false --latest'
  fi
  echo "prerelease=$prerelease"
  echo "image_tags=$image:$version,$image:$alias"
}

if [[ "${1:-}" == --self-test && $# == 1 ]]; then
  rc=$(release_flags 1.0.0-rc.1 ghcr.io/example/zirv-flow)
  stable=$(release_flags 1.0.0 ghcr.io/example/zirv-flow)
  [[ "$rc" == $'publish_flags=--draft=false --prerelease --latest=false\nprerelease=true\nimage_tags=ghcr.io/example/zirv-flow:1.0.0-rc.1,ghcr.io/example/zirv-flow:rc' ]]
  [[ "$stable" == $'publish_flags=--draft=false --latest\nprerelease=false\nimage_tags=ghcr.io/example/zirv-flow:1.0.0,ghcr.io/example/zirv-flow:latest' ]]
  if release_flags 1.0 ghcr.io/example/zirv-flow >/dev/null 2>&1; then
    echo 'FAIL: accepted malformed release version' >&2; exit 1
  fi
  echo 'release-flags self-test OK (rc and stable publish flags/image tags)'
elif [[ $# == 2 ]]; then
  release_flags "$1" "$2"
else
  echo "usage: $0 <version> <image> | --self-test" >&2
  exit 2
fi
