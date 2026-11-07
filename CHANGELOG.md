# Changelog

Generated from conventional-commit subjects (`feat`, `fix`, `perf`, `docs`,
plus `chore`/`refactor`/`test`/`style`/`ci`/`build`/`revert` folded into
Chores) by `scripts/generate-changelog.sh`. Never hand-edit this file --
regenerate a section instead:

    scripts/generate-changelog.sh --section --from <prev-tag> --to <new-tag>

`.github/workflows/cd.yaml` runs that command for every release and commits
the new top section here. A commit subject that does not match
`type(scope)!: summary` (most of the history before this file existed) is
listed under "Other" rather than dropped or guessed at. A `!` right after
the type/scope is the only breaking-change signal this script reads -- a
`BREAKING CHANGE:` footer in the commit body is not parsed. Full-rebuild
command: `scripts/generate-changelog.sh --full`.

## v0.253.0 (2026-11-07)

### Fixes

- true unknown_trait wording in flows and the segment assistant; test the retry on a 500 (#551)
- explorer axis shows dates for day buckets and readable numbers (#551)
- narrow tables, a sidebar scroll hint and the current entry in view (#551)
- request sending domains only for people who can edit the UTM section (#551)
- shell control names start with the text they show (#551)
- the assistant panel says when the deployment offers no AI (#551)
- true wording for archived fields, the signing secret and unlisted authors (#551)
- tie the sign-in error to its fields and match page titles to headings (#551)
- name profile sources and share one not-found state for unknown ids (#551)
- opaque list cursor and row id; document erasure gaps (#547, #548, #549)
- erase leaves no value on other people's records or assistant drafts (#548)
- hide the contact key from callers without PII access (#547)

### Documentation

- record the key masking decision for callers without PII access
- make the scope table say who can call a route (#549)

### Chores

- use a non-hex key in the masked-list test
- satisfy lint in the shell scroll and chart tests (#551)
- build the erased group once in erase_person

