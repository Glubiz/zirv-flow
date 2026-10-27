# M7 render benchmark

Issue #317, criterion 8: architecture section 19, row "Template render,
typical block email (about 20 blocks, conditions pre-evaluated)", target **p95 < 50 ms per message**.
Measured at commit `28f5b67c` (workspace 0.133.0). Raw report: [`data/m7/render.json`](data/m7/render.json);
tool `tools/renderbench`; script `scripts/bench/m7/render.sh` (runbook: `scripts/bench/README.md` section 12).

**This is a development machine, not reference hardware; the result is provisional until the M8 hardware run.**

## Result

```
Target:  p95 < 50 ms per message (architecture 19)
Result:  p95 1.573 ms through the production render pool (pool size 2, 5000 renders)
Verdict: met
```

| Run | Pool size | Renders | p50 | p95 | p99 | max |
|---|---|---|---|---|---|---|
| through the real pool | 1 | 5000 | 1.412 ms | 1.581 ms | 1.698 ms | 17.371 ms |
| through the real pool (production default) | 2 | 5000 | 1.404 ms | 1.573 ms | 1.669 ms | 4.734 ms |
| in-process, same render, one at a time | n/a | 5000 | 1.011 ms | 1.473 ms | 5.230 ms | 31.292 ms |

The whole `render_email_with` call is timed (request framing, the pipe round trip to the child, the render
in the child, the response and the click-tracking step in the caller). The host boundary adds about 0.4 ms
at p50 over the in-process render. The in-process p99 and max are a few outliers of one thread sharing the
machine with the rest of the session, not a cost of the renderer; with only 5000 samples, p99 and max
differ between runs (an earlier run of the same commit gave pool p99 values of 1.9 to 2.5 ms and a pool-2
max of 11 ms). Pool size 2 did not show a gain over 1 here: each call is one request, the child renders in
about a millisecond, and the caller's share is small, so a pool of 2 only pays off when callers are
concurrent beyond what one child serves.

## Method

- **Tool.** `renderbench`, a small tool under `tools/renderbench`, not a `loadgen` mode: `loadgen` is
  deliberately free of zirv-flow crates (architecture 5.1), and this tool must link `zirv-flow-render` and
  `zirv-flow-templates`; it has its own 5.1 row. The render children are the tool's own executable run as
  `render-host`, started by `ChildCommand::render_host` with the default 512 MiB limit (not enforced on
  Windows). No database; no Docker.
- **Workload.** The `newsletter` fixture of the block renderer (10 blocks) grown to 20: an image from an
  asset URL, one snippet block, one section and two blocks with a `visible_if` (results passed in: two
  true, one false, so one block is hidden), a spacer, raw HTML, a divider and a URL image. Marketing
  category, the default theme with a logo asset and a footer snippet (closing the email with the
  unsubscribe link), 21,034 bytes of HTML. The render inputs go through #307's pure `render_extras`
  conversion, so they have the shape a send gives them. The person has the traits the layout uses
  (`name`, `plan`) and an address. Click tracking is on, as for a marketing send. The pool's output is
  compared with the in-process render's before measuring and must be byte-equal.
- **Pool.** `RenderPool::render_email_with`, `PoolConfig::default()` as the production settings (size 2,
  deadline 5 s; pinned by the config test `the_render_defaults_are_two_children_five_seconds_and_512_mib`).
  As many calls in flight as the pool has children, so every child is always busy and no call waits for a
  slot. Each call is timed from before it to after it.
- **Runs.** Per pool size (1, then 2): 200 warm-up renders (excluded from the percentiles), then 5000
  measured. Percentiles are nearest-rank. Then the same message rendered in-process one at a time, with the
  same 200 warm-up and 5000 measured.

## Machine

Intel Core i9-13900K (24 cores, 32 threads), Windows 11, release build. The session tree was pinned to the
16 E-core threads (affinity mask `0xFFFF0000`) and cargo builds ran with `jobs = 4`; the benchmark ran as
it was, under that pin (the script changes no affinity, power plan or system setting). The tool's own
`logical_cpus` field reads 32 because the standard library counts the machine, not the mask. E-cores are
slower than P-cores, so the figures are conservative for this CPU, and other work from the session ran on
the same cores.
