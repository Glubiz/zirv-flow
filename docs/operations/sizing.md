# Sizing and PostgreSQL tuning

For operators choosing a deployment size, connection budget and maintenance policy.
The figures here come from development-machine and reference-resource runs, **not
reference hardware** (ruling ef99d178). The [M8 run (#397)](https://github.com/Glubiz/zirv-flow/issues/397)
will refresh them. Historical measurements below are workload-specific observations,
not a capacity guarantee for the current release. Targets, configured limits and
calculated budgets are labelled separately from measurements.

## Resource tiers and measured capacity

The app budgets come from [tier.sh](../../scripts/bench/tier.sh) and
[compose.tiers.yaml](../../scripts/bench/compose.tiers.yaml), recorded in
[M2 §2.1](../benchmarks/m2-ingestion.md#21-stack-and-tiers). The app roles share the
CPU budget; it is not a budget per process. PostgreSQL is outside that budget.

| Tier | App vCPU / RAM | Worker / serve / scheduler RAM | PostgreSQL in these runs |
| --- | --- | --- | --- |
| S | 2 / 4 GiB | 2048 / 1638 / 410 MiB | 24 logical CPUs, no memory limit |
| M | 4 / 8 GiB | 4096 / 3277 / 819 MiB | 24 logical CPUs, no memory limit |
| L | 8 / 16 GiB | 8192 / 6554 / 1638 MiB | 24 logical CPUs, no memory limit |

All rows: [M2 §2.1](../benchmarks/m2-ingestion.md#21-stack-and-tiers) and
[§3.1](../benchmarks/m2-ingestion.md#31-what-saturates); the recorded container
limits, for example [M stack](../benchmarks/data/M/sustained-stack.txt), are binary
memory units even though the report labels them GB/MB. A minimum PostgreSQL RAM
requirement per tier is **not measured** ([#397](https://github.com/Glubiz/zirv-flow/issues/397)).
The separate [section 19 envelope](../architecture.md#19-scalability-envelope)
targets PostgreSQL at 8 vCPU / 32 GB and two app hosts at 4 vCPU each
([M4 §2.1](../benchmarks/m4-segments.md#21-hardware-spec-d1)); app RAM for that
reference layout is **not measured** (#397). Do not treat the tier PostgreSQL
allocation as evidence that the envelope fits.

| Workload | S | M | L | Evidence and limits |
| --- | --- | --- | --- | --- |
| Sustained ingest | 2,000 items/s | 2,000 items/s | 2,000 items/s | [M2 §5](../benchmarks/m2-ingestion.md#5-sustained-burst-and-batch-results), track into one workspace |
| Drain after burst | 4,400 items/s | 4,000 items/s | 3,970 items/s | [M2 §5.1](../benchmarks/m2-ingestion.md#51-after-74-each-drain-shard-on-its-own-task), mixed identify/track; backlog divided by drain time, not sustainable intake |
| Wake jobs, peak claim rate | not measured (#397) | 1,108/s and 1,162/s unloaded; 832/s with ingest | not measured (#397) | [M5 §3](../benchmarks/m5-flows.md#3-results), short peak windows, not a general job-throughput guarantee |
| Sends to benchmark sink | not measured (#397) | 82.2 messages/s with one worker; 184.2 with four | not measured (#397) | [M6 §1](../benchmarks/m6-send.md#1-summary), block template, same app CPU budget; missed the report's send target |
| Full segment refresh | not measured (#397) | median 45.7 s trait-only; 14.5 s aggregate | not measured (#397) | [M4 §6.2](../benchmarks/m4-segments.md#62-results-against-the-section-19-targets), 10 million people |
| Timer lateness | not measured (#397) | p99 52.4 s / 51.8 s unloaded; 136.4 s with ingest | not measured (#397) | [M5 §1](../benchmarks/m5-flows.md#1-summary), 1 million waiting enrollments, loaded case at 2,000 events/s; loaded target missed |

The jobs and timers in M5 were seeded; they do not measure enrollment throughput.
A current-release throughput matrix and the larger timer population are **not
measured** ([#397](https://github.com/Glubiz/zirv-flow/issues/397)).

For ingestion, S was app-CPU-bound across `serve` and `worker`. In M and L,
`serve` was busiest but the host-to-container request path limited the experiment;
neither tier established an app CPU ceiling. Scheduler CPU was not measured
([#397](https://github.com/Glubiz/zirv-flow/issues/397)).
For sending, more worker processes improved throughput while database work became
the constraint, with all workers sharing the same app cores
([M6 §4](../benchmarks/m6-send.md#4-the-limiting-resource)).

Add workers when ready jobs and claim lag grow, job slots are occupied, and the
database still has CPU, I/O and connection headroom. More workers cannot repair a
slow query, a lock bottleneck or provider throttling. Follow
[ZirvFlowJobsBacklog](runbooks.md#zirvflowjobsbacklog),
[ZirvFlowJobClaimLag](runbooks.md#zirvflowjobclaimlag),
[ZirvFlowTimerLate](runbooks.md#zirvflowtimerlate) and
[ZirvFlowDestinationQueue](runbooks.md#zirvflowdestinationqueue) before scaling.

## Connection arithmetic

`database.max_connections` is the main pool limit **per process**, not per
deployment. A process running a worker also opens separate jobs and heartbeat
pools. Let D be its main pool limit, C be `worker.concurrency` and S be
`ingest.drain_shards`. The maximum is:

- `serve` or `scheduler`: D.
- `worker` or `all`: D + C × (S + 1) + 1 for the jobs pool + C for heartbeats,
  or D + (2 + S) × C + 1.

This is configured pool arithmetic from the
[worker implementation](../../crates/zirv-flow-jobs/src/worker.rs), not measured
connection demand. The jobs pool covers each slot's transaction, its concurrent
drain transactions and a failure connection; the heartbeat pool protects lease
renewal. The worker's `LISTEN` connection and scheduler's leader lock each occupy
a connection **inside** their main pool. HTTP and admin work also use the main
pool. Keep capacity available there for ordinary queries.

Worked example: separate `serve`, `worker` and `scheduler`, each using the current
[configured default](../../crates/zirv-flow/src/config.rs) D = 10, with C = 4 and
S = 8 as in [M2 §2.1](../benchmarks/m2-ingestion.md#21-stack-and-tiers). The extra
worker budget is (2 + 8) × 4 + 1 = **41** connections; the deployment total is
3 × 10 + 41 = **71**. Against the benchmark PostgreSQL `max_connections` of **100**
([M stack](../benchmarks/data/M/sustained-stack.txt)), that leaves **29** before
subtracting PostgreSQL reserved slots, monitoring clients, migrations, and the
admin CLI's own pool. These are calculated ceilings, not a recommended spare
capacity or observed usage. Count every additional process and concurrent admin
command before choosing PostgreSQL's limit; preserve operational headroom instead
of allocating the whole remainder to workers.

Use [ZirvFlowDbPoolSaturated](runbooks.md#zirvflowdbpoolsaturated) for the procedure.
An idle pool need not open its full allocation, but the server must accommodate
the planned worst case. Raising C or S changes that budget even if the main pool
limit stays unchanged.

## Application knobs

| Key | How to choose |
| --- | --- |
| `ingest.drain_shards` | Parallel drain loops within a workspace. Historical M2 used 8 ([§2.1](../benchmarks/m2-ingestion.md#21-stack-and-tiers)); the current batch-endpoint/shard tuning result is **not measured**, [#76](https://github.com/Glubiz/zirv-flow/issues/76), carried to [#397](https://github.com/Glubiz/zirv-flow/issues/397). Observe [inbox age](runbooks.md#zirvflowinboxage) and recalculate connections before changing it. |
| `worker.concurrency` | Concurrent job slots per worker. The tier runs used 4 ([M2 §2.1](../benchmarks/m2-ingestion.md#21-stack-and-tiers)); match increases to database headroom and claim lag. A general optimum is **not measured** (#397). |
| `worker.max_running_per_workspace` | Limits running jobs for a busy workspace. The OQ-8 recommendation is **not measured**, [#396](https://github.com/Glubiz/zirv-flow/issues/396): the sampler and decision table exist, the run does not. Do not infer an optimum from the cap's correctness tests. |
| `explorer.interactive_timeout_ms` | The shipped 5 s versus envelope 15 s choice is **not measured**, [#395](https://github.com/Glubiz/zirv-flow/issues/395). These are competing limits from [config](../../crates/zirv-flow/src/config.rs) and [architecture](../architecture.md#19-scalability-envelope), not benchmark timings. Keep the shipped setting pending that decision. |
| `render.pool_size` | Children per `serve` and `worker` process. Pool size 2 gave render p95 1.573 ms ([M7 Result](../benchmarks/m7-render.md#result), [data](../benchmarks/data/m7/render.json)); the pool-size comparison did not establish a latency gain. Increase only for concurrent render demand and available memory; use [render restarts](runbooks.md#zirvflowrenderrestarts) to diagnose failures first. |
| Segment list "used by" | The list parses the live version of every non-archived flow in the workspace on each request, uncapped, with no measured ceiling ([#397](https://github.com/Glubiz/zirv-flow/issues/397)). Drafts are not read there. |

## PostgreSQL settings

The following is the **recorded benchmark configuration**, not a universal tuning
preset. Every value is in [M2 §2.1](../benchmarks/m2-ingestion.md#21-stack-and-tiers)
and [M stack](../benchmarks/data/M/sustained-stack.txt). A smaller PostgreSQL host
needs its own memory budget; these runs did not measure smaller database tiers.

| Setting | Recorded value | Operational meaning |
| --- | --- | --- |
| `shared_buffers` | 4 GB | PostgreSQL's shared page cache; leave RAM for backend work and the OS cache. |
| `effective_cache_size` | 16 GB | Planner estimate of available cache, not a memory allocation. |
| `maintenance_work_mem` | 1 GB | Budget maintenance work alongside concurrent queries and vacuum workers. |
| `max_wal_size` | 8 GB | Checkpoint pressure setting, not a hard limit on WAL disk usage. |
| `checkpoint_timeout` | 15 min | Timed checkpoint interval; WAL pressure can cause earlier checkpoints. |
| `synchronous_commit` | on | The measured baseline waits for commit durability. |

The performance effect of disabling `synchronous_commit` or increasing
`wal_buffers` is **not measured** ([#151](https://github.com/Glubiz/zirv-flow/issues/151)).
[M4 §6.5.1](../benchmarks/m4-segments.md#651-ingest-p99-with-and-without-segment-re-evaluation-on-0760-151)
found WAL flush contention but explicitly did not run those changes. The files in
`docs/benchmarks/data/ab/` measure decision-write overhead ([M2 §6](../benchmarks/m2-ingestion.md#6-decision-log-overhead)),
not this A/B. Turning synchronous commit off can acknowledge transactions whose
recent changes are then lost in a crash; faster acknowledgements would weaken the
durable-inbox promise. Increasing WAL buffers does not waive the commit durability
wait. Keep the measured durability setting until a deliberate, measured decision.

## Maintenance and disk growth

Keep autovacuum enabled. `jobs` and `ingest_inbox` already have table-specific
vacuum/analyze thresholds and vacuum cost delay in the
[job migration](../../crates/zirv-flow-store/migrations/0003_jobs.sql) and
[inbox migration](../../crates/zirv-flow-store/migrations/0011_ingest.sql).
Inspect each table's `reloptions` before overriding them. `person_changes` and
`debugger_feed` also churn; being unlogged does not remove `debugger_feed`'s vacuum
needs. Inspect the event child partitions as well as the parent. A workload-tested
autovacuum preset for these tables is **not measured** (#397); lower per-table
`autovacuum_vacuum_scale_factor` or `autovacuum_analyze_scale_factor` only against
observed cleanup/statistics lag, and budget the maintenance I/O.

Monitor `pg_stat_user_tables`: `n_dead_tup`, `n_live_tup`, `last_autovacuum`,
`last_autoanalyze`, and `n_mod_since_analyze`. Persistent dead-tuple growth calls for
checking long transactions and blocked vacuum before adding workers. Refresh
statistics with `ANALYZE` after restoring or importing a large dataset; stale
statistics harmed send plans ([M6 §5](../benchmarks/m6-send.md#5-misses-and-causes),
[#297](https://github.com/Glubiz/zirv-flow/issues/297)). Use the
[restore guide](backup-restore.md) for the full procedure.

The scheduler creates current and next-month event and decision partitions.
Keep it running and inspect its partition-creation warnings. The retention jobs
remove expired event/decision partitions and use batched deletes for partial
windows; messages have their own cleanup. Set `events.retention_days`,
`retention.messages_days` and `retention.decision_days` to the required history,
including any shorter workspace windows. See [retention windows](../people.md#events)
and [#376](https://github.com/Glubiz/zirv-flow/issues/376),
[#377](https://github.com/Glubiz/zirv-flow/issues/377),
[#378](https://github.com/Glubiz/zirv-flow/issues/378).
A shorter window permanently removes history; increasing it cannot restore rows.

| Retained data | Disk growth basis |
| --- | --- |
| Million events | About **443 MB**, table plus indexes, calculated from the byte totals divided by the event count in [M4 seed data](../benchmarks/data/m4/seed.json). This is the stored seed shape, excluding WAL, decisions and other writes induced by ingestion. |
| Million messages | **not measured** in the available benchmark evidence; [#397](https://github.com/Glubiz/zirv-flow/issues/397). |
| Million decision records | **not measured** in the permitted benchmark files/reports; [#378](https://github.com/Glubiz/zirv-flow/issues/378) supplies the measurement tool and [#397](https://github.com/Glubiz/zirv-flow/issues/397) must record its output. The figures in the people reference lack a supporting file here and are not used for sizing. |

Multiply measured bytes per record by retained records for your payloads and
indexes, then allow separately for WAL, temporary queries, index maintenance,
backups and free disk. Watch [ZirvFlowDecisionLogSize](runbooks.md#zirvflowdecisionlogsize),
[ZirvFlowSegmentDirtyBacklog](runbooks.md#zirvflowsegmentdirtybacklog) and
[ZirvFlowJobsDead](runbooks.md#zirvflowjobsdead) so failed cleanup is visible.

## Topology

Place a load balancer in front of multiple `serve` processes, using `/readyz` for
readiness. Keep every role on the same writable PostgreSQL database and include
all processes in the connection budget. Set `ops.bind` on worker-only and
scheduler-only processes to expose their health and metrics endpoints; see
[runtime](runtime.md#background-jobs) and [ZirvFlowNotReady](runbooks.md#zirvflownotready).

A read replica for preview and explain is **deferred**
([architecture §8.3](../architecture.md#83-preview-explain-membership)); those
queries currently share the primary. A caching proxy for `/a/` assets can keep
repeated email asset downloads away from the app and database
([asset serving](../architecture.md#11-templates-and-the-builder)).
