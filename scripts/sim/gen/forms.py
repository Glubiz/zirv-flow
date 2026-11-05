#!/usr/bin/env python3
"""Signed form posts to the three webhooks (design 4.2); hook ids and secrets from /sim/keys/hooks.json.

hooks.json: {"newsletter": {"id": "...", "secret": "zfwh_..."}, "demo": {...}, "contact": {...}}
"""

import hashlib
import hmac
import http.client
import json
import random
import threading
import time
import uuid
from pathlib import Path

import pace
import world

HOOKS = Path("/sim/keys/hooks.json")
FORMS = [("newsletter", 60), ("demo", 15), ("contact", 25)]
SOURCES = ["search", "social", "referral", "event", "direct"]
local = threading.local()


def payload(form):
    if random.random() < 0.8:
        n = random.randrange(1, world.PEOPLE + 1)
        body = {"customer_id": world.customer_id(n), "email": world.email(n),
                "name": world.FIRST[world.h(n, 8) % len(world.FIRST)]}
    else:
        body = {"email": f"lead{uuid.uuid4().hex[:10]}@example.com", "name": random.choice(world.FIRST)}
    body.update({"id": uuid.uuid4().hex, "form": form, "utm_source": random.choice(SOURCES),
                 "consent": {"newsletter": form == "newsletter" or random.random() < 0.2}})
    return body


def send(_count):
    hooks = json.loads(HOOKS.read_text())
    form = world.pick_weighted(FORMS, random.randrange(100))[0]
    hook = hooks[form]
    raw = json.dumps(payload(form)).encode()
    stamp = str(int(time.time()))
    sig = hmac.new(hook["secret"].encode(), stamp.encode() + b"." + raw, hashlib.sha256).hexdigest()
    for _ in range(3):
        try:
            if not hasattr(local, "conn"):
                local.conn = http.client.HTTPConnection("lb", 8080, timeout=30)
            local.conn.request("POST", f"/hooks/{hook['id']}", raw,
                               {"Content-Type": "application/json", "X-Zirv-Timestamp": stamp,
                                "X-Zirv-Signature": "sha256=" + sig})
            response = local.conn.getresponse()
            response.read()
            if response.status == 202:
                return
            if response.status == 429:
                time.sleep(float(response.getheader("Retry-After") or 1))
                continue
            raise RuntimeError(f"hook answered {response.status}")
        except (OSError, http.client.HTTPException):
            del local.conn
            time.sleep(0.5)
    raise RuntimeError("hook unreachable")


if __name__ == "__main__":
    while not HOOKS.exists():
        time.sleep(2)
    pace.run("forms", send, chunk_items=1, workers=4)
