## Memory
- Key: sqlx-test-db-name-collision
- Written-by: claude
- Written: 1792885063
- Verified: 1792885063
- Source: explicit

sqlx::test names each test database _sqlx_test_<sha512 of the test path> (sqlx-core 0.9.0 testing/mod.rs:49), with nothing per worktree or process. So the same DB test run at the same time from two worktrees or checkouts against one PostgreSQL shares one database: the second run panics at setup with 55006 'database ... is being accessed by other users', or fails mid-setup. Parallel workers must not run the same DB test at the same time on one server; read a 55006 setup failure as a collision and re-run alone. Verified 2026-10-25 with the jobs simulation tests (#225 vs #267 worktrees).
