# Release checklist

Fill the empty proof links in the release pull request and keep this checklist as
evidence for [#408](https://github.com/Glubiz/zirv-flow/issues/408). Preparation does
not authorize publishing or close that issue. Run `bash scripts/release-check.sh`
with GitHub issue read access; CI runs it for workspace versions beginning with
`1.` (release candidates included). The static shipped-input check reuses the
bench-feature gate; its compilation guards and default-binary check remain in CI.
Manual review is still required for the evidence and decisions below.

- [ ] No open `severity:blocker` issues in the M8 milestone (criterion 8). [Proof]()
- [ ] No open `severity:major` issues in the M8 milestone (criterion 8). [Proof]()
- [ ] Every earlier exit criterion has its passing test or report. [Proof]()
- [ ] RBAC matrix (#367) passes. [Proof]()
- [ ] Erasure scans (#373, #374) pass. [Proof]()
- [ ] Trace matrix (#382–#385) passes. [Proof]()
- [ ] Assistant security (#386) passes. [Proof]()
- [ ] Restore drill (#398) passes. [Proof]()
- [ ] Upgrade drill (#399) passes. [Proof]()
- [ ] Benchmark report (#397) covers every envelope dimension with evidence and a verdict. [Proof]()
- [ ] Operator guides are complete: secrets, backup and restore, upgrade, sizing, runbooks. [Proof]()
- [ ] User guides, `docs/api.md` and `docs/threat-model.md` are complete. [Proof]()
- [ ] API is frozen: `docs/api-audit.md` has no open exception, `info.version` is `"1"`, and the #405 gate is required. [Proof]()
- [ ] `SECURITY.md` states the supported versions for 1.0. [Proof]()
- [ ] Dependency audit (#390) is green. [Proof]()
- [ ] Every Dockerfile base image is pinned by digest; shipped builds exclude benchmark features. [Proof]()
- [ ] Roadmap and README status match the release. [Proof]()
- [ ] Release publishing support (#407) and the checklist's engineering work are merged; dry runs for `1.0.0-rc.1` and `1.0.0` flags pass. [Proof]()
- [ ] The release-candidate PR sets `Cargo.toml` to `1.0.0-rc.1`; CD publishes a GitHub prerelease and the version and `:rc` image tags, without changing Latest or `:latest`. [Proof]()
- [ ] The rc image pulls and starts; the container smoke test reports `1.0.0-rc.1` from `GET /api/v1/version`, and `:latest` still identifies the previous stable image. [Proof]()
- [ ] Upgrade and restore drills run against the rc image, with their evidence attached. [Proof]()
- [ ] The operator sets the beta-period length; fixes land on main as subsequent rc versions, with no API-breaking change after rc1. [Proof]()
- [ ] Each subsequent rc repeats the drills and every checklist item affected by its changes. [Proof]()
- [ ] The stable release PR sets `version = "1.0.0"` and links the completed checklist and evidence in its body. [Proof]()
- [ ] The operator explicitly confirms before merging the stable PR, which publishes the release, sets Latest and pushes `:latest`. [Proof]()
- [ ] After publication, the image pulls and starts, the version endpoint reports `1.0.0`, and the GitHub release is stable and marked Latest. [Proof]()
- [ ] GHCR has `:1.0.0` and matching `:latest`, `:rc` is unchanged, and the release attestation verifies. [Proof]()
- [ ] All nine M8 roadmap exit criteria are ticked and its status, the README status and `SECURITY.md` are updated. [Proof]()
- [ ] Close the M8 milestone after verifying the published release. [Proof]()
- [ ] Create post-1.0 follow-up issues for deferred work, including invitations and a run on actual reference hardware. [Proof]()
