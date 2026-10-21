## Memory
- Key: bench-db-schema-0-76
- Written-by: claude
- Written: 1792610653
- Verified: 1792610653
- Source: explicit

The local 10M-people bench database (compose project zirv-flow-m4bench10m) was migrated to the 0.76.0 schema by the M5 timer bench (#169, 2026-10-21). zirv-flow refuses a newer schema (SchemaState::Newer), so images before 0.76.0 cannot run against it, and no pre-migration backup exists: a version A/B against older images needs a fresh database (#151, #209). M4 ran with BENCH_PG_SHM=8gb; scripts/bench/m4/common.sh defaults to 1gb, and compose up with a different value or image recreates the containers on the same volumes.
