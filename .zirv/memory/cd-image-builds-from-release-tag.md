## Memory
- Key: cd-image-builds-from-release-tag
- Written-by: claude
- Written: 1791707404
- Verified: 1791707404
- Source: explicit

The CD image job checks out the release tag (v<version>), not the merge commit, so a Dockerfile-only fix reaches the published image only with a workspace version bump; re-running CD for an already-published version rebuilds the old tagged Dockerfile. Found when the sharing=locked cache fix (PR #32) still failed on v0.15.0 and needed 0.15.1 (PR #33).
