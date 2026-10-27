#!/usr/bin/env bash
# Usage: check.sh base.json head.json [frozen.json ...]
# PR metadata is data from GITHUB_EVENT_PATH, never interpolated shell source.
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

has_decision_link() {
  local body="$1" url relative sha path
  local root="${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:?}"
  while IFS= read -r url; do
    while [[ "$url" == *[.,\;:!] ]]; do url=${url%?}; done
    url=${url%%#*}
    [[ "$url" == "$root/"* ]] || continue
    relative=${url#"$root/"}
    if [[ "$relative" =~ ^commit/([0-9a-f]{40})$ ]]; then
      sha=${BASH_REMATCH[1]}
      if git diff-tree --root --no-commit-id --name-only -r "$sha" 2>/dev/null |
        grep -E '^docs/(architecture|roadmap)\.md$|^\.zirv/work/.+\.md$' >/dev/null; then return 0; fi
    elif [[ "$relative" =~ ^blob/([0-9a-f]{40})/(docs/(architecture|roadmap)\.md|\.zirv/work/.+\.md)$ ]]; then
      sha=${BASH_REMATCH[1]}; path=${BASH_REMATCH[2]}
      if [[ "$(git cat-file -t "$sha:$path" 2>/dev/null)" == blob ]]; then return 0; fi
    fi
  done < <(printf '%s\n' "$body" | grep -Eo 'https?://[^[:space:]<>)]+')
  return 1
}

if (( $# < 2 )); then echo "usage: $0 base.json head.json [frozen.json ...]" >&2; exit 2; fi
base=$1; head=$2; shift 2
if [[ -n "${GITHUB_EVENT_PATH:-}" ]] && jq -e \
  'any(.pull_request.labels[]?; .name == "api-breaking-approved")' "$GITHUB_EVENT_PATH" >/dev/null; then
  body=$(jq -r '.pull_request.body // ""' "$GITHUB_EVENT_PATH")
  if ! has_decision_link "$body"; then
    echo '::error::api-breaking-approved requires a decision link to an architecture/roadmap commit or a decision file under .zirv/work/ (full commit SHA, this repository).'
    exit 1
  fi
  echo '::notice::API breaking change approved with a decision record; after rc1 the operator must approve the label.'
  exit 0
fi

for baseline in "$base" "$@"; do
  "${OASDIFF:-oasdiff}" breaking "$baseline" "$head" --fail-on ERR \
    --severity-levels "$script_dir/severity.txt"
done
