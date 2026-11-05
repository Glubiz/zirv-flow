#!/usr/bin/env python3
"""Seeds the simulated world (design 3.1).

  seed.py sources [--people N]        company PostgreSQL, MySQL, billing SQLite, warehouse CSV, Kafka backlog
  seed.py ingest-people [--people N]  identify items through /ingest/batch, 500 per request, 6 connections
  seed.py all [--people N]

Both steps are repeatable: sources are dropped and rebuilt (the Kafka topics are recreated).
"""

import argparse
import csv
import http.client
import json
import os
import sqlite3
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
from pathlib import Path

import world
from world import customer_id, ts

SEED = Path("/sim/seed")
EXPORTS = Path("/sim/exports")
T0 = time.time()


def log(text):
    print(f"[{time.time() - T0:7.1f}s] {text}", flush=True)


def stamp(value):
    return value.strftime("%Y-%m-%d %H:%M:%S+00") if value else ""


def stamp_my(value):
    return value.strftime("%Y-%m-%d %H:%M:%S") if value else ""


def write_files(people, base):
    """One pass over person(n) writes every CSV the loads need."""
    SEED.mkdir(parents=True, exist_ok=True)
    EXPORTS.mkdir(parents=True, exist_ok=True)
    now = datetime.now(timezone.utc)
    handles = {}

    def opener(name, path):
        handles[name] = path.open("w", newline="")
        return csv.writer(handles[name], lineterminator="\n")

    customers = opener("customers", SEED / "customers.csv")
    subs = opener("subscriptions", SEED / "subscriptions.csv")
    loyalty = opener("loyalty", SEED / "loyalty.csv")
    accounts = opener("accounts", SEED / "accounts.csv")
    warehouse = opener("warehouse", EXPORTS / "warehouse.csv.tmp")
    warehouse.writerow(["customer_id", "email", "newsletter_opt_in", "ltv_band",
                        "preferred_language", "rfm_segment"])
    for n in range(1, people + 1):
        p = world.person(n, base)
        cid = p["customer_id"]
        customers.writerow([cid, p["email"], p["first_name"], p["last_name"], p["country"], p["city"],
                            stamp(p["signup_at"]), stamp(p["last_order_at"]), p["lifetime_orders"],
                            stamp(now)])
        subs.writerow([f"S{n:07d}", cid, p["plan"], p["plan_status"], p["mrr"],
                       stamp(p["renewal_at"]), stamp(now)])
        loyalty.writerow([cid, p["loyalty_tier"], p["loyalty_points"], stamp_my(now)])
        accounts.writerow([f"A{n:07d}", cid, p["billing_status"], p["balance_due"],
                           p["payment_method"], ts(p["last_payment_at"]), ts(now)])
        warehouse.writerow([cid, p["email"], "true" if p["newsletter_opt_in"] else "false",
                            p["ltv_band"], p["preferred_language"], p["rfm_segment"]])
    orders = opener("orders", SEED / "orders.csv")
    for i in range(1, people + 1):
        o = world.order_row(i, people, base)
        orders.writerow([o["order_id"], o["customer_id"], o["amount"], o["currency"], o["category"],
                         o["items"], stamp(o["placed_at"]), stamp(o["placed_at"])])
    tickets = opener("tickets", SEED / "tickets.csv")
    ticket_count = int(people * 0.35)
    for i in range(1, ticket_count + 1):
        t = world.ticket_row(i, people, base)
        tickets.writerow([t["ticket_id"], t["customer_id"], t["priority"], t["channel"], t["status"],
                          stamp_my(t["opened_at"]), stamp_my(t["opened_at"])])
    payments = opener("payments", SEED / "payments.csv")
    payment_count = int(people * 0.5)
    for i in range(1, payment_count + 1):
        q = world.payment_row(i, people, base)
        payments.writerow([q["payment_id"], q["customer_id"], q["amount"], q["currency"],
                           ts(q["received_at"]), ts(q["received_at"])])
    for handle in handles.values():
        handle.close()
    os.replace(EXPORTS / "warehouse.csv.tmp", EXPORTS / "warehouse.csv")
    return {"orders": people, "tickets": ticket_count, "payments": payment_count}


PG_DDL = """
DROP TABLE IF EXISTS orders, subscriptions, customers CASCADE;
CREATE TABLE customers (
  customer_id text PRIMARY KEY, email text NOT NULL, first_name text, last_name text,
  country text, city text, signup_at timestamptz NOT NULL, last_order_at timestamptz,
  lifetime_orders integer NOT NULL DEFAULT 0, deleted_at timestamptz,
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE subscriptions (
  subscription_id text PRIMARY KEY, customer_id text NOT NULL, plan text NOT NULL,
  plan_status text NOT NULL, mrr numeric(10,2) NOT NULL, renewal_at timestamptz,
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE orders (
  order_id text PRIMARY KEY, customer_id text NOT NULL, amount numeric(10,2) NOT NULL,
  currency text NOT NULL, category text NOT NULL, items integer NOT NULL,
  placed_at timestamptz NOT NULL, updated_at timestamptz NOT NULL DEFAULT now());
"""

PG_POST = """
CREATE INDEX customers_updated ON customers (updated_at, customer_id);
CREATE INDEX subscriptions_updated ON subscriptions (updated_at, subscription_id);
CREATE INDEX subscriptions_customer ON subscriptions (customer_id);
CREATE INDEX orders_updated ON orders (updated_at, order_id);
CREATE INDEX orders_customer ON orders (customer_id);
CREATE OR REPLACE FUNCTION touch_updated_at() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN NEW.updated_at := now(); RETURN NEW; END $$;
CREATE TRIGGER customers_touch BEFORE UPDATE ON customers FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER subscriptions_touch BEFORE UPDATE ON subscriptions FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER orders_touch BEFORE UPDATE ON orders FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE OR REPLACE FUNCTION order_to_customer() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  UPDATE customers SET last_order_at = NEW.placed_at, lifetime_orders = lifetime_orders + 1
   WHERE customer_id = NEW.customer_id;
  RETURN NEW;
END $$;
CREATE TRIGGER orders_to_customer AFTER INSERT ON orders FOR EACH ROW EXECUTE FUNCTION order_to_customer();
GRANT SELECT ON ALL TABLES IN SCHEMA public TO flow_reader;
ANALYZE;
"""


def load_pg():
    import psycopg

    url = f"postgresql://shop_admin:{os.environ['SIM_COMPANY_PG_PASSWORD']}@company-pg:5432/shop"
    with psycopg.connect(url, autocommit=True) as db:
        db.execute(PG_DDL)
        copies = [
            ("customers", "customer_id, email, first_name, last_name, country, city, signup_at, "
             "last_order_at, lifetime_orders, updated_at", "customers.csv"),
            ("subscriptions", "subscription_id, customer_id, plan, plan_status, mrr, renewal_at, "
             "updated_at", "subscriptions.csv"),
            ("orders", "order_id, customer_id, amount, currency, category, items, placed_at, "
             "updated_at", "orders.csv"),
        ]
        for table, columns, name in copies:
            db.execute(f"COPY {table} ({columns}) FROM '/seed/{name}' WITH (FORMAT csv)")
            log(f"postgres {table} loaded")
        db.execute(PG_POST)
        return {t: db.execute(f"SELECT count(*) FROM {t}").fetchone()[0]
                for t in ("customers", "subscriptions", "orders")}


MY_DDL = [
    "DROP TABLE IF EXISTS loyalty_accounts",
    "DROP TABLE IF EXISTS support_tickets",
    """CREATE TABLE loyalty_accounts (
      customer_id VARCHAR(16) NOT NULL PRIMARY KEY, loyalty_tier VARCHAR(16) NOT NULL,
      loyalty_points INT NOT NULL,
      updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
    ) ENGINE=InnoDB""",
    """CREATE TABLE support_tickets (
      ticket_id VARCHAR(16) NOT NULL PRIMARY KEY, customer_id VARCHAR(16) NOT NULL,
      priority VARCHAR(16) NOT NULL, channel VARCHAR(16) NOT NULL, status VARCHAR(16) NOT NULL,
      opened_at DATETIME NOT NULL,
      updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
    ) ENGINE=InnoDB""",
]


def load_mysql():
    import pymysql

    db = pymysql.connect(host="company-mysql", user="root", database="support", autocommit=True,
                         password=os.environ["SIM_MYSQL_ROOT_PASSWORD"], read_timeout=3600)
    with db.cursor() as cur:
        cur.execute("SET time_zone = '+00:00'")
        for ddl in MY_DDL:
            cur.execute(ddl)
        fmt = ("FIELDS TERMINATED BY ',' OPTIONALLY ENCLOSED BY '\"' "
               "LINES TERMINATED BY '\\n'")
        cur.execute(f"LOAD DATA INFILE '/seed/loyalty.csv' INTO TABLE loyalty_accounts {fmt} "
                    "(customer_id, loyalty_tier, loyalty_points, updated_at)")
        log("mysql loyalty_accounts loaded")
        cur.execute(f"LOAD DATA INFILE '/seed/tickets.csv' INTO TABLE support_tickets {fmt} "
                    "(ticket_id, customer_id, priority, channel, status, opened_at, updated_at)")
        log("mysql support_tickets loaded")
        cur.execute("CREATE INDEX loyalty_updated ON loyalty_accounts (updated_at, customer_id)")
        cur.execute("CREATE INDEX tickets_updated ON support_tickets (updated_at, ticket_id)")
        counts = {}
        for table in ("loyalty_accounts", "support_tickets"):
            cur.execute(f"SELECT count(*) FROM {table}")
            counts[table] = cur.fetchone()[0]
    return counts


SQLITE_DDL = """
DROP TABLE IF EXISTS accounts; DROP TABLE IF EXISTS payments;
CREATE TABLE accounts (
  account_id TEXT PRIMARY KEY, customer_id TEXT NOT NULL, status TEXT NOT NULL,
  balance_due REAL NOT NULL DEFAULT 0, payment_method TEXT, last_payment_at TEXT,
  updated_at TEXT NOT NULL);
CREATE TABLE payments (
  payment_id TEXT PRIMARY KEY, customer_id TEXT NOT NULL, amount REAL NOT NULL,
  currency TEXT NOT NULL, received_at TEXT NOT NULL, updated_at TEXT NOT NULL);
"""


def load_billing():
    db = sqlite3.connect("/data/billing.db", timeout=60)
    db.executescript(SQLITE_DDL)
    for table, name, width in (("accounts", "accounts.csv", 7), ("payments", "payments.csv", 6)):
        marks = ",".join("?" * width)
        with (SEED / name).open(newline="") as handle:
            reader = csv.reader(handle)
            while True:
                chunk = [row for _, row in zip(range(50000), reader)]
                if not chunk:
                    break
                db.executemany(f"INSERT INTO {table} VALUES ({marks})", chunk)
        db.commit()
        log(f"billing {table} loaded")
    db.executescript("""
      CREATE INDEX accounts_updated ON accounts (updated_at, account_id);
      CREATE INDEX payments_updated ON payments (updated_at, payment_id);""")
    counts = {t: db.execute(f"SELECT count(*) FROM {t}").fetchone()[0] for t in ("accounts", "payments")}
    db.close()
    return counts


def load_kafka(people, base, retention_days):
    from confluent_kafka import Producer
    from confluent_kafka.admin import AdminClient, NewTopic

    admin = AdminClient({"bootstrap.servers": "kafka:9092"})
    topics = ["app-feature-used", "app-license-activated"]
    existing = admin.list_topics(timeout=30).topics
    gone = [t for t in topics if t in existing]
    if gone:
        for future in admin.delete_topics(gone, operation_timeout=30).values():
            future.result()
        time.sleep(3)
    for _ in range(20):
        try:
            for future in admin.create_topics([NewTopic(t, num_partitions=3, replication_factor=1)
                                               for t in topics]).values():
                future.result()
            break
        except Exception:
            time.sleep(2)
    producer = Producer({"bootstrap.servers": "kafka:9092", "compression.type": "gzip",
                         "linger.ms": 50, "queue.buffering.max.messages": 200000})
    count = int(people * 0.3)
    window = max(1, retention_days - 1) * 86400
    per_topic = {t: 0 for t in topics}
    for i in range(1, count + 1):
        topic, key, value, when = world.kafka_value(i, people, base, window)
        per_topic[topic] += 1
        while True:
            try:
                producer.produce(topic, key=key, value=json.dumps(value).encode(),
                                 timestamp=int(when.timestamp() * 1000))
                break
            except BufferError:
                producer.poll(0.5)
        if i % 5000 == 0:
            producer.poll(0)
    producer.flush(120)
    return per_topic


def sources(people):
    base = world.reset_anchor()
    log(f"anchor {base.isoformat()}, {people} people")
    extra = write_files(people, base)
    log(f"csv files written ({extra})")
    counts = {"postgres": load_pg(), "mysql": load_mysql(), "billing": load_billing(),
              "warehouse_csv_rows": people,
              "kafka": load_kafka(people, base, int(os.environ.get("SIM_RETENTION_DAYS", "5")))}
    for path in SEED.glob("*.csv"):
        path.unlink()
    log("kafka backlog produced")
    print(json.dumps(counts, indent=1), flush=True)
    return counts


class Poster:
    def __init__(self, key):
        self.key = key
        self.conn = None

    def post(self, body):
        for attempt in range(8):
            try:
                if self.conn is None:
                    self.conn = http.client.HTTPConnection("lb", 8080, timeout=60)
                self.conn.request("POST", "/api/v1/ingest/batch", body,
                                  {"Authorization": f"Bearer {self.key}",
                                   "Content-Type": "application/json"})
                response = self.conn.getresponse()
                response.read()
                if response.status == 202:
                    return
                if response.status == 429:
                    time.sleep(float(response.getheader("Retry-After") or 1))
                    continue
                raise RuntimeError(f"ingest answered {response.status}")
            except (OSError, http.client.HTTPException):
                self.conn = None
                time.sleep(1 + attempt)
        raise RuntimeError("ingest unreachable")


def people_item(n, base):
    p = world.person(n, base)
    return {"type": "identify", "messageId": f"seed-{p['customer_id']}",
            "identifiers": {"external:customer_id": p["customer_id"], "email": p["email"]},
            "traits": {"first_name": p["first_name"], "country": p["country"],
                       "signup_at": ts(p["signup_at"]), "plan": p["plan"]}}


def zf_db():
    import psycopg

    return psycopg.connect(
        f"postgresql://zirv:{os.environ['SIM_PG_PASSWORD']}@zf-db:5432/zirv_flow", autocommit=True)


def ingest_people(people):
    key = Path("/sim/keys/ingest.key").read_text().strip()
    base = world.anchor()
    starts = list(range(1, people + 1, 500))
    local = __import__("threading").local()

    def send(start):
        if not hasattr(local, "poster"):
            local.poster = Poster(key)
        items = [people_item(n, base) for n in range(start, min(start + 500, people + 1))]
        local.poster.post(json.dumps({"batch": items}))
        return len(items)

    done = 0
    began = time.time()
    with ThreadPoolExecutor(max_workers=6) as pool:
        for count in pool.map(send, starts):
            done += count
            if done % 50000 < 500:
                log(f"posted {done} identify items ({done / (time.time() - began):.0f}/s)")
    posted_at = time.time() - began
    log(f"all {done} items accepted in {posted_at:.1f}s; waiting for the inbox to drain")
    with zf_db() as db:
        while True:
            inbox = db.execute("SELECT count(*) FROM ingest_inbox").fetchone()[0]
            if inbox == 0:
                break
            time.sleep(2)
        count = db.execute("SELECT count(*) FROM people WHERE merged_into IS NULL").fetchone()[0]
    total = time.time() - began
    log(f"people in zirv-flow: {count}; items posted in {posted_at:.1f}s, applied in {total:.1f}s")
    print(json.dumps({"people": count, "post_seconds": round(posted_at, 1),
                      "total_seconds": round(total, 1)}), flush=True)
    return count


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=["sources", "ingest-people", "all"])
    parser.add_argument("--people", type=int, default=world.PEOPLE)
    args = parser.parse_args()
    if args.command in ("sources", "all"):
        sources(args.people)
    if args.command in ("ingest-people", "all"):
        ingest_people(args.people)


if __name__ == "__main__":
    sys.exit(main())
