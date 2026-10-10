# Plan

Docs and agent-context change on PR #1 (`chore/repo-setup`), written by a Codex
worker per ruling 8c36f93b; the AI-tool ownership row corrected afterwards so
tool implementations live in `api` (5.1 forbids `ai` depending on validators).

1. `docs/architecture.md`: principle 12; section 5.1-5.5 (allowed dependencies,
   feature modules, five registries, frontend modules, enforcement); ADR-009.
2. `docs/roadmap.md`: every-milestone modularity rule; M1 enforcement scope and
   exit criteria.
3. `.zirv/context/common.md` + regenerated `CLAUDE.md`/`AGENTS.md`;
   `CONTRIBUTING.md` pointer to section 5.
4. GitHub M1 milestone description regenerated.
Verify: references and anchors resolve; `zirv context sync --check`; CI green.
