## Memory
- Key: css-merge-lost-brace
- Written-by: claude
- Written: 1793522344
- Verified: 1793522344
- Source: explicit

web/src/ui/styles.css is a single large file that parallel branches all append to, so merges often conflict in it. When both sides of a conflict end with the same closing brace, git keeps that brace as shared context after the >>>>>>> marker. Deleting the markers then leaves the HEAD side's last rule unclosed. CSS nesting makes this valid syntax, so postcss parses, lint and tests pass, and every later rule silently nests inside the unclosed one and loses its styles. Found on PR #484 (2026-11-01) in .daybars-table table. After every resolution check that postcss walkRules finds no rule whose parent is a rule, and that the counts of '{' and '}' are equal.
