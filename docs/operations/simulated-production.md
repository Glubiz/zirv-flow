# Simulated production

A local environment that looks like a small company's production: one million people spread over
a PostgreSQL database, a MySQL database, an HTTP billing API, a daily CSV export, a Kafka topic pair
and three signed web forms, with web traffic, a daily curve, bursts, real mail delivery (with
bounces, opens and clicks) and five flows running on top. Use it to see Zirv Flow under a
believable load, to try a scenario by hand, or to check a change against a populated system.

It is not a benchmark. The CPU quotas are capped for a developer machine, so the numbers below say
what a laptop-class host does, not what the product can do; see [sizing](sizing.md) and the
[benchmark reports](../benchmarks/) for capacity figures. It also does not model real mail
providers: the mail API and the SMTP sink are fixtures.

Everything lives in `scripts/sim/` and runs as the Docker Compose project `zirv-flow-sim`. The
only host requirements are Docker, `bash`, `curl`, `jq` and `openssl`. The Zirv Flow image comes from
the container registry (`SIM_IMAGE_TAG` in `scripts/sim/profiles/*.env`, currently `0.235.0`); no
host build is needed.

## Requirements

| | Full profile (1 M people) | Small profile (100 k people) |
|---|---|---|
| Docker memory | 33 GiB | 16 GiB |
| Free disk | 130 GiB | 40 GiB |
| CPU | quotas total about 8 cores, mostly idle | about 6 cores |

Stop the benchmark stack (`zirv-flow-m8`) first: both want the memory. Check the disk Docker
has with `docker system df` and `docker run --rm alpine df -h /`. The event table costs about 400
bytes per event, so a day at the default rate (21.6 M events) is about 8.6 GB; retention is five days
by default (`SIM_RETENTION_DAYS`), and `SIM_MAX_DB_GB` pauses the generators when the database
passes it.

## Start, stop, reset

```sh
scripts/sim/sim.sh start --profile full     # or --profile small
scripts/sim/sim.sh status
scripts/sim/sim.sh verify
scripts/sim/sim.sh down                     # stop, keep the volumes
scripts/sim/sim.sh reset                    # remove the stack, its volumes and .state/ (this project only)
```

`start` runs `up` (TLS material, the tools image, every service, the company databases' seed, the
first time only), then
`provision` (owner, users, workspace, registry, the people backbone, credentials, sources, sending,
templates, the first syncs), waits for the ready gate, turns the generators on, and finally waits
for the backfill before it creates the segments and publishes the flows that react to events. It waits
up to `SIM_BACKFILL_TIMEOUT` seconds (4 h in the small profile, 18 h in the full one). Every stage is
idempotent, so `sim.sh provision <stage>` can be repeated (stages: `base registry people connections
sending segments templates syncs gate flows`; with no stage it runs all but `segments`, which the
`flows` stage runs after the backfill).

Measured fresh starts, with the CPU quotas of the profiles (A2 capped them on 2026-11-05):

| Phase | Small (100 k) | Full (1 M) |
|---|---|---|
| `up` including the source seed | about 1 min (images cached) | about 4 min (seed 160 s) |
| People backbone (ingest, applied) | 127 s | 655 s |
| Ready gate (from `start`) | 233 s | 958 s (16 min) |
| Traffic on | 255 s | 982 s |
| Backfill complete (all first syncs) | 4 263 s (71 min; 25 to 30 min in a run with fewer deadlocks) | not complete after 4 h: 43% of the records read, 9 to 15 h projected |
| Event flows published | 4 300 s | after the backfill |

The ready gate holds at least 99% of the people, a verified sending domain, destinations that pass
their check, running Kafka streams and the provisioning checks of `verify.sh --provision`. Sync
throughput under the capped quotas is about 300 records per second per stream at 100 k and 200 to 290 records
per second in total at 1 M (the eight first syncs share one PostgreSQL and lock the same people), so the full
backfill takes most of a working day. Four hours in, the ingest inbox held 1.6 M items and drained about 30 per
second: at 1 M, start with `SIM_TRAFFIC_AFTER_BACKFILL=1 sim.sh start --profile full` so the generators begin
after the first syncs, or run `sim.sh traffic off` meanwhile.

## Signing in and addresses

| What | Where |
|---|---|
| App | <http://localhost:8080> (a load balancer in front of two `serve` processes) |
| Prometheus | <http://localhost:9090> (the shipped alert rules are loaded) |
| Mailbox viewer (transactional mail) | <http://localhost:8025> |

`sim.sh credentials` prints every generated secret. The accounts are `owner@sim.example.com`
(instance owner and workspace admin), `ada@` (admin), `pat@` (publisher), `ed@` (editor) and `vic@`
(viewer, without personal-data access), all at `sim.example.com`; the workspace is **Sim Co**
(`sim-co`). The owner's password is `SIM_OWNER_PASSWORD` in `scripts/sim/.state/env`; the others
are `scripts/sim/.state/keys/user-<name>.password`.

## What is provisioned

- **Sources**: Company PostgreSQL (`customers`, `subscriptions`, `orders`), Company MySQL
  (`loyalty_accounts`, `support_tickets`), a Billing API (HTTP manifest: `accounts`, `payments`),
  a Warehouse export (the CSV file source, daily), Product events (Kafka, two topics) and three
  inbound webhooks (newsletter, demo requests, contact).
- **Registry**: 24 traits, 12 event schemas, an opt-out `newsletter` topic and an opt-in
  `product_updates` topic, the lists `Newsletter subscribers` and `Win-back candidates`.
- **Segments**: Danish paying customers, Premium plans, Big spenders (aggregate), Active shoppers and
  Abandoned checkout (behavioural), Lapsed customers, Past due.
- **Sending**: the domain `mail.sim.example.com` (verified through a DNS fixture), two senders, an
  SMTP destination (transactional, to the mailbox viewer), a provider destination (marketing, to the
  mail fixture, which sends back signed delivered, bounced and complained events and opens and
  clicks the links) and a webhook destination with one forwarding rule (orders of 400 or more). A
  frequency cap of three marketing messages in seven days applies.
- **Flows**: Welcome series (a newsletter form post starts it), Re-engagement (daily at 10:00
  Copenhagen time on the lapsed segment), Re-engagement blast (manual), Order confirmation (an
  `order_placed` event, SMTP) and Past-due nudge (a segment entry). A "day" in the waits is
  `SIM_WAIT_DAY_SECONDS` seconds (120 by default), so a series finishes in minutes.

## Connecting by hand

| What | Value |
|---|---|
| Company PostgreSQL | host `company-pg`, database `shop`, role `flow_reader` (read-only), password `SIM_FLOW_READER_PASSWORD` |
| Company MySQL | host `company-mysql`, database `support`, the same role and password |
| Kafka | `kafka:9092`, plaintext, topics `app-feature-used` and `app-license-activated` |
| Billing API | `https://billing-api.sim.example.com/v1/accounts`, header `X-API-Key` (the `billing key` credential) |
| Warehouse CSV | `https://exports.sim.example.com/exports/warehouse.csv` |
| Ingest API key | `scripts/sim/.state/keys/ingest.key` (scope `ingest`) |
| Forms | `scripts/sim/.state/keys/hooks.json`: the hook ids and secrets |

The hosts resolve inside the `sim` network only. To post a form by hand, sign the body with the
hook secret:

```sh
HOOK=$(jq -r .newsletter.id scripts/sim/.state/keys/hooks.json)
SECRET=$(jq -r .newsletter.secret scripts/sim/.state/keys/hooks.json)
BODY='{"id":"manual-1","email":"p5@mail.example.com","customer_id":"C0000005","form":"newsletter","consent":{"newsletter":true}}'
TS=$(date +%s)
SIG=$(printf '%s' "$TS.$BODY" | openssl dgst -sha256 -hmac "$SECRET" | sed 's/^.* //')
curl -X POST "http://localhost:8080/hooks/$HOOK" -H 'Content-Type: application/json' \
  -H "X-Zirv-Timestamp: $TS" -H "X-Zirv-Signature: sha256=$SIG" -d "$BODY"
```

## Rates, the daily curve and bursts

`sim.sh rate N` sets the daily mean of total events per second across web ingest (68%), Kafka (22%),
changes in the company sources (8%) and form posts (2%). The default is 250, and the two-hump daily
curve swings it between about 75 and 450. `sim.sh burst [factor] [seconds]` multiplies it now;
`pause` and `resume` stop and start the generators; `traffic off` stops the containers.
`SIM_DAY_SECONDS=3600` compresses the curve for a quick look. `sim.sh status` shows the achieved
rate against the target, late and dropped dispatches, people, the inbox age, bytes per event and the
sink arrivals.

## Scenarios

1. **First sync of each source.** Open **Sources**, pick one and read its runs: records read,
   applied, unchanged. `sim.sh sync-all` queues a run on every pull stream.
2. **A change arrives.** In `company-pg`, run
   `UPDATE customers SET country = 'DK' WHERE customer_id = 'C0000042'` (as `shop_admin`, through
   `docker compose -p zirv-flow-sim exec company-pg psql -U shop_admin shop`). The trigger moves
   `updated_at`; the next `*/10` run applies it, and the person's `country` trait changes.
3. **A burst.** `sim.sh burst 3 120`, then watch the inbox age and the ingest rate on
   <http://localhost:9090>.
4. **The welcome series.** Post the newsletter form above and open the person: the event
   `form_submitted`, list membership, the first message within a minute and the second two
   "days" later unless they order.
5. **The re-engagement blast and its delivery.** `sim.sh blast` starts the manual flow on the lapsed
   customers (enrollment begins about five minutes later; the warm-up ramp spreads the entries).
   The mail fixture bounces 1.2% of recipients, reports 0.3% complaints and opens about 35% and
   clicks about 8% of delivered mail within a few minutes; the Results tab and the counters in
   `sim.sh status` show it.
6. **Kill something.** `docker compose -p zirv-flow-sim stop receiver` and place an order of 400 or
   more: the forwarding queue depth rises (Prometheus: `zirv_flow_destination_forwarding_depth`) and
   drains after `start receiver`. `stop zf-worker-1` shows the other worker taking its jobs.

## Reading the system

`sim.sh status` is one screen. Prometheus has the serve, worker and scheduler targets and the fixture
counters. `sim.sh logs <service>` is `docker compose logs`. `sim.sh verify [group]` runs the named
checks: `sources`, `kafka`, `webhooks`, `ingest`, `consistency`, `destinations`, `delivery`,
`forwarding`, `prometheus`; it prints one PASS or FAIL line per step and exits non-zero on a failure.
The delivery group takes about eight minutes because it waits for real mail to be delivered, opened
and clicked.

## Troubleshooting

| Symptom | Cause and action |
|---|---|
| A first sync run shows `failed` with `job_failed` | Read the run's error and `sim.sh logs zf-worker-1`. A batch that keeps conflicting gives up after its attempts (PostgreSQL logs `deadlock detected`); this is rare since first-sync batches lock in one fixed order. The schedule starts the stream again within ten minutes, and `provision.sh` also restarts failed streams while it waits. |
| Event flows are not published | They wait for the backfill on purpose, to keep segments out of the first syncs: people who enter a segment because of first-sync data enroll in a segment-triggered flow. A first sync flags its own events `no_triggers`, so event flows would not enroll on the backfill itself. `provision.sh flows` publishes them once every pull stream has a first success. |
| `up` stalls on a service | `sim.sh status`, then `sim.sh logs <service>`. Memory is the usual cause: stop other stacks. |
| The generators stopped | `status` shows `paused true (disk)`: `disk` means `SIM_MAX_DB_GB` was passed. |
| Mail never arrives in the viewer | Only the transactional flows use SMTP; marketing mail goes to the mail fixture (`sim.sh status` shows both counters). |

## Cost

The event table costs 400 to 620 bytes per event with its indexes (398 in a ten-minute run, 620 at 2.1 M events). The
1 M people alone are 2.9 GB. Four hours into the full start the database was 15 GB. At the default rate (21.6 M
events a day) five days of retention settle near 45 to 65 GB; `SIM_MAX_DB_GB` (120) pauses the generators before
the disk fills.

## Reset

`sim.sh reset` removes this project's containers, volumes and `scripts/sim/.state/`, and nothing
else. `up` and `start` seed the company sources once: a marker, `scripts/sim/.state/seeded-sources`,
records it, so `down` followed by `up` or `start` resumes the same world and does not load the Kafka
backlog again. A Kafka offset cannot be reset from the app, so seeding on purpose (a fresh world)
needs `reset` and a new `start`, not a new topic. `sim.sh seed sources` still reloads the sources by
hand, and the Kafka backlog with them.
