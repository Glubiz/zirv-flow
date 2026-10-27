#!/usr/bin/env bash
# Uses the exact oasdiff binary and policy installed for the CI gate.
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT
export GITHUB_EVENT_PATH="$workdir/event.json"
export GITHUB_SERVER_URL=https://github.com GITHUB_REPOSITORY=example/gate
printf '{"pull_request":{"labels":[],"body":""}}\n' > "$GITHUB_EVENT_PATH"

expect() {
  local expected="$1" name="$2" status=0
  shift 2
  "$@" > "$workdir/output" 2>&1 || status=$?
  if [[ "$status" != "$expected" ]]; then
    echo "FAIL $name: expected exit $expected, got $status" >&2
    cat "$workdir/output" >&2
    exit 1
  fi
}
base="$script_dir/base.json"
head="$workdir/head.json"
gate="$script_dir/check.sh"
expect 0 unchanged bash "$gate" "$base" "$base"
expect 1 removed-response-field bash "$gate" "$base" "$script_dir/head.json"
jq '.paths["/api/v1/people"].post.requestBody.content["application/json"].schema.properties.nickname = {"type":"string"}' "$base" > "$head"
expect 0 optional-request-field bash "$gate" "$base" "$head"
jq '.paths["/api/v1/people"].post.requestBody.content["application/json"].schema.required = ["nickname"]' "$head" > "$workdir/required.json"
expect 1 required-request-field bash "$gate" "$base" "$workdir/required.json"
jq '.paths["/api/v1/people"].post.responses["200"].content["application/json"].schema.properties.nickname = {"type":"string"}' "$base" > "$head"
expect 0 optional-response-field bash "$gate" "$base" "$head"
jq '.paths["/api/v1/people"].post.responses["200"].content["application/json"].schema.properties.status.enum += ["archived"]' "$base" > "$head"
expect 0 response-enum-added bash "$gate" "$base" "$head"
jq '.paths["/api/v1/people"].post.responses["200"].content["application/json"].schema = {"type":"string","enum":["active"]}' "$base" > "$workdir/body-enum.json"
jq '.paths["/api/v1/people"].post.responses["200"].content["application/json"].schema.enum += ["archived"]' "$workdir/body-enum.json" > "$head"
expect 0 response-body-enum-added bash "$gate" "$workdir/body-enum.json" "$head"
jq '.paths["/api/v1/people"].post.requestBody.content["application/json"].schema.properties.name.enum = ["active","paused"]' "$base" > "$workdir/request-enum.json"
jq '.paths["/api/v1/people"].post.requestBody.content["application/json"].schema.properties.name.enum = ["active"]' "$workdir/request-enum.json" > "$head"
expect 1 request-enum-removed bash "$gate" "$workdir/request-enum.json" "$head"
jq '.paths["/api/v1/people"].post.requestBody.content["application/json"].schema.properties.count = {"type":"number"}' "$base" > "$workdir/number.json"
jq '.paths["/api/v1/people"].post.requestBody.content["application/json"].schema.properties.count.type = "integer"' "$workdir/number.json" > "$head"
expect 1 narrowed-request-type bash "$gate" "$workdir/number.json" "$head"
jq '.paths["/api/v1/people"].post.responses |= {"201": .["200"]}' "$base" > "$head"
expect 1 changed-status bash "$gate" "$base" "$head"
jq '.components.securitySchemes.key = {"type":"apiKey","in":"header","name":"X-Key"} | .paths["/api/v1/people"].post.security = [{"key":[]}]' "$base" > "$head"
expect 1 new-security-requirement bash "$gate" "$base" "$head"
jq 'del(.paths["/api/v1/people"].post)' "$base" > "$head"
expect 1 removed-method bash "$gate" "$base" "$head"
jq '.paths["/api/v1/new"] = {"get":{"responses":{"200":{"description":"OK"}}}}' "$base" > "$head"
expect 0 added-route bash "$gate" "$base" "$head"
jq '.paths = {}' "$base" > "$head"
expect 1 removed-route bash "$gate" "$base" "$head"
# The branch baseline can pass while the immutable freeze baseline fails.
expect 1 frozen-route bash "$gate" "$head" "$head" "$base"

cd "$workdir"
git init -q
git config user.name test
git config user.email test@example.com
mkdir -p docs .zirv/work/decision
printf 'Accepted API change\n' > docs/architecture.md
printf 'Decision record\n' > .zirv/work/decision/decision.md
git add docs .zirv
git commit -qm 'docs: record API decision'
sha=$(git rev-parse HEAD)
link="$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/commit/$sha"
jq --arg body "[Decision]($link)" '.pull_request.body = $body' "$GITHUB_EVENT_PATH" > next.json
mv next.json "$GITHUB_EVENT_PATH"
expect 1 decision-without-label bash "$gate" "$base" "$head"
jq '.pull_request.labels = [{"name":"api-breaking-approved"}] | .pull_request.body = ""' "$GITHUB_EVENT_PATH" > next.json
mv next.json "$GITHUB_EVENT_PATH"
expect 1 label-without-decision bash "$gate" "$base" "$head"
for body in '[unrelated](https://example.com/decision)' "[decision](https://github.com/other/repo/commit/$sha)"; do
  jq --arg body "$body" '.pull_request.body = $body' "$GITHUB_EVENT_PATH" > next.json
  mv next.json "$GITHUB_EVENT_PATH"
  expect 1 unrelated-link bash "$gate" "$base" "$head"
done
for link in "$link" "$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$sha/.zirv/work/decision/decision.md"; do
  jq --arg body "[Decision]($link)" '.pull_request.body = $body' "$GITHUB_EVENT_PATH" > next.json
  mv next.json "$GITHUB_EVENT_PATH"
  expect 0 approved-removal bash "$gate" "$base" "$head" "$base"
done
echo 'openapi-gate self-test OK (schema changes, route PRs, freeze and override)'
