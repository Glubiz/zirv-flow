#!/usr/bin/env python3
"""Ongoing changes in every data source (design 4.2), the disk guard and the daily CSV export.

Each paced item is one change: an insert or update in company PostgreSQL, company MySQL or the
billing SQLite file. Two background threads run beside it: the disk guard (every 60 s, pauses the
traffic generators above SIM_MAX_DB_GB) and the warehouse CSV regeneration (02:00 UTC, or when
/sim/export-now appears).
"""

import csv
import os
import random
import sqlite3
import threading
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

import psycopg
import pymysql

import pace
import world

PEOPLE = world.PEOPLE
local = threading.local()
state = {"db_bytes": 0, "signups": 0, "ops": {}}
counter_lock = threading.Lock()
next_signup = [PEOPLE]
EXPORT_FLAG = Path("/sim/export-now")

# (name, weight) of the change kinds; the order and ticket rates follow design section 2.
OPS = [("order", 15), ("ticket", 5), ("ticket_resolve", 5), ("payment", 5), ("plan_change", 12),
       ("address", 12), ("renewal", 8), ("cancel", 1), ("signup", 2), ("loyalty", 20),
       ("billing_status", 10)]


def pg():
    if not hasattr(local, "pg") or local.pg.closed:
        local.pg = psycopg.connect(
            f"postgresql://shop_admin:{os.environ['SIM_COMPANY_PG_PASSWORD']}@company-pg:5432/shop",
            autocommit=True)
    return local.pg


def my():
    if hasattr(local, "my"):
        try:
            local.my.ping(reconnect=False)
            return local.my
        except pymysql.Error:
            pass
    local.my = pymysql.connect(host="company-mysql", user="root", database="support",
                               password=os.environ["SIM_MYSQL_ROOT_PASSWORD"], autocommit=True)
    return local.my


def lite():
    db = sqlite3.connect("/data/billing.db", timeout=30)
    return db


def rnd_person():
    return random.randrange(1, PEOPLE + 1)


def iso():
    return world.now_ts()


def do(kind):
    n = rnd_person()
    cid = world.customer_id(n)
    if kind == "order":
        amount = round(random.uniform(5, 450), 2)
        pg().execute(
            "INSERT INTO orders (order_id, customer_id, amount, currency, category, items, placed_at) "
            "VALUES (%s, %s, %s, %s, %s, %s, now()) ON CONFLICT DO NOTHING",
            ("N" + uuid.uuid4().hex[:15], cid, amount, random.choice(["EUR", "DKK"]),
             random.choice(world.CATEGORIES), random.randrange(1, 7)))
    elif kind == "ticket":
        with my().cursor() as cur:
            cur.execute("INSERT IGNORE INTO support_tickets (ticket_id, customer_id, priority, channel, "
                        "status, opened_at) VALUES (%s, %s, %s, %s, 'open', UTC_TIMESTAMP())",
                        ("N" + uuid.uuid4().hex[:15], cid, random.choice(world.PRIORITIES),
                         random.choice(world.CHANNELS)))
    elif kind == "ticket_resolve":
        with my().cursor() as cur:
            cur.execute("UPDATE support_tickets SET status = 'resolved' WHERE ticket_id = %s",
                        (f"T{random.randrange(1, int(PEOPLE * 0.35) + 1):09d}",))
    elif kind == "payment":
        pid = "N" + uuid.uuid4().hex[:15]
        now = iso()
        db = lite()
        with db:
            db.execute("INSERT OR IGNORE INTO payments VALUES (?, ?, ?, 'EUR', ?, ?)",
                       (pid, cid, round(random.uniform(5, 300), 2), now, now))
            db.execute("UPDATE accounts SET last_payment_at = ?, status = 'current', balance_due = 0, "
                       "updated_at = ? WHERE account_id = ?", (now, now, f"A{n:07d}"))
        db.close()
    elif kind == "plan_change":
        plan = world.pick_weighted(world.PLANS, random.randrange(100))[0]
        pg().execute("UPDATE subscriptions SET plan = %s, mrr = %s WHERE subscription_id = %s",
                     (plan, world.MRR[plan], f"S{n:07d}"))
    elif kind == "address":
        pg().execute("UPDATE customers SET city = %s WHERE customer_id = %s",
                     (random.choice(random.choice(world.COUNTRIES)[2]), cid))
    elif kind == "renewal":
        pg().execute("UPDATE subscriptions SET renewal_at = now() + interval '30 days', "
                     "plan_status = 'active' WHERE subscription_id = %s", (f"S{n:07d}",))
    elif kind == "cancel":
        pg().execute("UPDATE customers SET deleted_at = now() WHERE customer_id = %s", (cid,))
        pg().execute("UPDATE subscriptions SET plan_status = 'canceled' WHERE subscription_id = %s",
                     (f"S{n:07d}",))
    elif kind == "signup":
        with counter_lock:
            next_signup[0] += 1
            k = next_signup[0]
        p = world.person(k, datetime.now(timezone.utc))
        pg().execute(
            "INSERT INTO customers (customer_id, email, first_name, last_name, country, city, "
            "signup_at, lifetime_orders) VALUES (%s, %s, %s, %s, %s, %s, now(), 0) "
            "ON CONFLICT DO NOTHING",
            (p["customer_id"], p["email"], p["first_name"], p["last_name"], p["country"], p["city"]))
        pg().execute("INSERT INTO subscriptions (subscription_id, customer_id, plan, plan_status, mrr, "
                     "renewal_at) VALUES (%s, %s, 'free', 'free', 0, now() + interval '30 days') "
                     "ON CONFLICT DO NOTHING", (f"S{k:07d}", p["customer_id"]))
        with my().cursor() as cur:
            cur.execute("INSERT IGNORE INTO loyalty_accounts (customer_id, loyalty_tier, loyalty_points) "
                        "VALUES (%s, 'bronze', 0)", (p["customer_id"],))
        db = lite()
        with db:
            db.execute("INSERT OR IGNORE INTO accounts VALUES (?, ?, 'current', 0, 'card', ?, ?)",
                       (f"A{k:07d}", p["customer_id"], iso(), iso()))
        db.close()
        state["signups"] += 1
    elif kind == "loyalty":
        with my().cursor() as cur:
            cur.execute("UPDATE loyalty_accounts SET loyalty_points = loyalty_points + %s "
                        "WHERE customer_id = %s", (random.randrange(1, 200), cid))
    elif kind == "billing_status":
        status = random.choice(["current", "current", "past_due", "closed"])
        due = round(random.uniform(5, 200), 2) if status == "past_due" else 0
        db = lite()
        with db:
            db.execute("UPDATE accounts SET status = ?, balance_due = ?, updated_at = ? "
                       "WHERE account_id = ?", (status, due, iso(), f"A{n:07d}"))
        db.close()
    state["ops"][kind] = state["ops"].get(kind, 0) + 1


def send(_count):
    do(world.pick_weighted(OPS, random.randrange(sum(w for _, w in OPS)))[0])


def init_signups():
    try:
        row = pg().execute("SELECT max(substr(customer_id, 2)::int) FROM customers").fetchone()
        next_signup[0] = max(PEOPLE, row[0] or 0)
    except Exception as exc:
        print(f"changes: could not read the last customer: {type(exc).__name__}", flush=True)


def guard():
    """Pause the traffic generators when zirv-flow's database passes SIM_MAX_DB_GB."""
    limit = float(os.environ.get("SIM_MAX_DB_GB", "120")) * (1 << 30)
    while True:
        try:
            with psycopg.connect("postgresql://sim_monitor:sim_monitor@zf-db:5432/zirv_flow",
                                 connect_timeout=10) as db:
                size = db.execute("SELECT pg_database_size('zirv_flow')").fetchone()[0]
            state["db_bytes"] = size
            if size > limit and not pace.Control().get().get("paused"):
                pace.write_control({"paused": True, "paused_reason": "disk"})
                print(f"changes: database {size >> 30} GiB over the limit; traffic paused", flush=True)
        except Exception as exc:
            print(f"changes: disk guard failed: {type(exc).__name__}", flush=True)
        time.sleep(60)


def regenerate_csv():
    """customers-YYYYMMDD.csv, then an atomic rename over warehouse.csv (design 2)."""
    today = datetime.now(timezone.utc)
    day = int(today.timestamp() // 86400)
    base = world.anchor()
    exports = Path("/sim/exports")
    target = exports / f"customers-{today:%Y%m%d}.csv"
    with target.open("w", newline="") as handle:
        out = csv.writer(handle, lineterminator="\n")
        out.writerow(["customer_id", "email", "newsletter_opt_in", "ltv_band", "preferred_language",
                      "rfm_segment"])
        for n in range(1, PEOPLE + 1):
            p = world.person(n, base)
            opt = p["newsletter_opt_in"] != (world.h(n, 100 + day) % 200 == 0)
            out.writerow([p["customer_id"], p["email"], "true" if opt else "false", p["ltv_band"],
                          p["preferred_language"], p["rfm_segment"]])
    os.replace(target, exports / "warehouse.csv")
    for old in exports.glob("customers-*.csv"):
        old.unlink(missing_ok=True)
    print(f"changes: warehouse.csv regenerated for {today:%Y-%m-%d}", flush=True)


def exporter():
    last_day = None
    while True:
        now = datetime.now(timezone.utc)
        try:
            if EXPORT_FLAG.exists():
                EXPORT_FLAG.unlink(missing_ok=True)
                regenerate_csv()
            elif now.hour == 2 and last_day != now.date():
                last_day = now.date()
                regenerate_csv()
        except Exception as exc:
            print(f"changes: export failed: {type(exc).__name__}", flush=True)
        time.sleep(2)


if __name__ == "__main__":
    init_signups()
    threading.Thread(target=guard, daemon=True).start()
    threading.Thread(target=exporter, daemon=True).start()
    pace.run("changes", send, chunk_items=1, workers=8,
             make_extra=lambda: {"db_bytes": state["db_bytes"], "signups": state["signups"],
                                 "ops": dict(state["ops"])})
