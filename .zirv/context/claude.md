# zirv-flow -- Claude-specific working instructions

## Repo additions to the injected orchestrator rules

- For the cross-harness review worker on a substantial diff, codex-cli is
  installed at `~/AppData/Local/Programs/OpenAI/Codex/bin` even when a roster
  line claims it is not.

## This Windows dev machine

- NEVER `taskkill` a `zirv*.exe`: this session itself runs under zirv and you
  will kill it. To clear an LNK1104 build lock, rename the locked exe aside.
