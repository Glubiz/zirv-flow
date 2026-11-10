#!/usr/bin/env python3
"""Web and app traffic through /ingest/batch (design 4.2): sessions with anonymous-to-known stitching."""

import http.client
import json
import multiprocessing
import multiprocessing.connection
import os
import random
import sys
import threading
import time
import uuid
from pathlib import Path

import pace
import world

KEY_FILE = Path("/sim/keys/ingest.key")
CHUNK = 60
EVENTS = [("page_viewed", 45), ("product_viewed", 20), ("search_performed", 8),
          ("cart_item_added", 8), ("checkout_started", 3), ("app_opened", 16)]
PAGES = ["/", "/pricing", "/products", "/blog", "/support", "/account"]
local = threading.local()
sessions = []
lock = threading.Lock()


def properties(name):
    if name == "page_viewed":
        return {"path": random.choice(PAGES)}
    if name == "product_viewed":
        return {"sku": f"SKU{random.randrange(5000)}", "category": random.choice(world.CATEGORIES)}
    if name == "search_performed":
        return {"query_length": random.randrange(2, 30), "results": random.randrange(0, 200)}
    if name == "cart_item_added":
        return {"sku": f"SKU{random.randrange(5000)}", "quantity": random.randrange(1, 4)}
    if name == "checkout_started":
        return {"items": random.randrange(1, 6), "value": round(random.uniform(10, 400), 2)}
    return {"platform": random.choice(world.DEVICES)}


def session_item(people):
    """One item of a random live session; may also be a login identify."""
    with lock:
        if not sessions or random.random() < 0.05:
            sessions.append({"anon": "a-" + uuid.uuid4().hex, "user": None,
                             "left": random.randrange(3, 15)})
            if len(sessions) > 3000:
                sessions.pop(random.randrange(len(sessions)))
        s = random.choice(sessions)
        s["left"] -= 1
        if s["left"] <= 0:
            sessions.remove(s)
        login = s["user"] is None and random.random() < 0.06
        if login:
            s["user"] = random.randrange(1, people + 1)
    now = world.now_ts()
    if login:
        n = s["user"]
        return {"type": "identify", "anonymousId": s["anon"], "userId": world.customer_id(n),
                "identifiers": {"email": world.email(n)}, "messageId": uuid.uuid4().hex,
                "timestamp": now,
                "traits": {"last_seen_at": now, "device": random.choice(world.DEVICES)}}
    name = world.pick_weighted(EVENTS, random.randrange(sum(w for _, w in EVENTS)))[0]
    item = {"type": "track", "event": name, "anonymousId": s["anon"],
            "properties": properties(name), "messageId": uuid.uuid4().hex, "timestamp": now}
    if s["user"]:
        item["userId"] = world.customer_id(s["user"])
    return item


def post(body):
    for _ in range(4):
        try:
            if not hasattr(local, "conn"):
                local.conn = http.client.HTTPConnection("lb", 8080, timeout=30)
            local.conn.request("POST", "/api/v1/ingest/batch", body,
                               {"Authorization": f"Bearer {KEY_FILE.read_text().strip()}",
                                "Content-Type": "application/json"})
            response = local.conn.getresponse()
            response.read()
            if response.status == 202:
                return
            if response.status == 429:
                time.sleep(float(response.getheader("Retry-After") or 1))
                continue
            raise RuntimeError(f"ingest answered {response.status}")
        except (OSError, http.client.HTTPException):
            del local.conn
            time.sleep(0.5)
    raise RuntimeError("ingest unreachable")


def send(count):
    items = [session_item(world.PEOPLE) for _ in range(count)]
    post(json.dumps({"batch": items}))


if __name__ == "__main__":
    while not KEY_FILE.exists():
        time.sleep(2)
    # One Python process tops out near 7,700 events/s on one core, so the x5 profile splits the
    # generator over SIM_GEN_INGEST_PROCS processes; each paces 1/procs of the rate with its share
    # of the sender threads, and writes its own stats part (see pace.Stats.write).
    procs = max(1, int(os.environ.get("SIM_GEN_INGEST_PROCS", "1")))
    threads = max(1, -(-int(os.environ.get("SIM_GEN_INGEST_WORKERS", "8")) // procs))

    def run_part(part):
        pace.run("ingest", send, chunk_items=CHUNK, workers=threads, part=part, parts=procs)

    if procs == 1:
        run_part(0)
    else:
        children = [multiprocessing.Process(target=run_part, args=(part,), daemon=True) for part in range(procs)]
        for child in children:
            child.start()
        # A dead part would silently drop its share of the rate: exit so the container restarts
        # and the failure shows.
        multiprocessing.connection.wait([child.sentinel for child in children])
        print("gen-ingest: a part exited; exiting", flush=True)
        sys.exit(1)
