#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"

seed() {
  local target="$1" directory="$2" pattern="$3" file name
  mkdir -p "$repo/fuzz/corpus/$target"
  while IFS= read -r -d '' file; do
    name="${file#"$repo/"}"
    name="${name//\//_}"
    cp "$file" "$repo/fuzz/corpus/$target/$name"
  done < <(find "$repo/$directory" -type f -name "$pattern" -print0)
}

seed segment_ast crates/zirv-flow-segments/tests/fixtures '*.json'
# Core flow envelopes also seed the segment parser's structural rejection path.
seed segment_ast crates/zirv-flow-core/tests/fixtures '*.json'
seed manifest crates/zirv-flow-connectors/tests/fixtures/manifests '*.json'
seed manifest crates/zirv-flow-connectors/tests/fixtures/providers '*.json'
seed template_document crates/zirv-flow-templates/tests/fixtures '*.json'
seed sanitize_source crates/zirv-flow-templates/tests/fixtures '*.html'
