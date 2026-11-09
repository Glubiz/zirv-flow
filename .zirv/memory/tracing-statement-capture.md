## Memory
- Key: tracing-statement-capture
- Written-by: claude
- Written: 1794206622
- Verified: 1794206622
- Source: explicit

Counting sqlx statements in a test with a scoped tracing subscriber (`WithSubscriber::with_subscriber`) can silently see nothing. With only one live scoped `Dispatch`, tracing-core takes its one-dispatcher fast path: a callsite registered after that dispatch was created gets its interest from the current thread's default. If another thread, or a future outside the scope (a lock probe, a polling loop on the pool), is the first to hit sqlx's `sqlx::query` event callsite, that callsite is cached as never and the scoped subscriber gets no events for the rest of the process. Keep a second `Dispatch` alive while capturing (for example `tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default())`), or assert on database state instead (`pg_stat_xact_user_tables` gives the rows a transaction inserted and deleted). Verified 2026-11-09 with sqlx 0.9 (perf P, `people/batch.rs` tests and the apply bench).
