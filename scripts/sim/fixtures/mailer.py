#!/usr/bin/env python3
"""The HTTP mail API the provider destination sends through.

POST /v1/messages (bearer token, Idempotency-Key) stores the message and decides one outcome per
recipient from a hash of the message id: delivered, soft bounce, hard bounce or complaint. The
signed feedback goes to the destination's feedback route through the load balancer. For
delivered mail the fixture then fetches the open pixel and one link out of the HTML after a
log-normal delay. Recipients whose local part starts with "bounce" always hard bounce.

/sim/keys/mailer.json holds {"destination_id", "feedback_secret", "token"}; it is read again
whenever it changes, so the fixture can start before provisioning.
Manifest names: signature header X-Mailer-Signature (prefix sha256=), timestamp header
X-Mailer-Timestamp, event type at /event, message id at /meta/message_id, event id at /id,
bounce kind at /bounce/kind (permanent or transient).
"""

import hashlib
import hmac
import heapq
import json
import math
import os
import random
import re
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid
from collections import deque
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from common import Counters, JsonFile

LB = os.environ.get("MAILER_LB", "http://lb:8080")
PUBLIC = os.environ.get("MAILER_PUBLIC_URL", "http://localhost:8080")
SEED = os.environ.get("MAILER_SEED", "sim")
# Cumulative thresholds out of 1000: delivered 965, soft 20, hard 12, complaint 3.
SOFT = float(os.environ.get("MAILER_SOFT", "0.02"))
HARD = float(os.environ.get("MAILER_HARD", "0.012"))
COMPLAINT = float(os.environ.get("MAILER_COMPLAINT", "0.003"))
OPEN_RATE = float(os.environ.get("MAILER_OPEN_RATE", "0.35"))
CLICK_RATE = float(os.environ.get("MAILER_CLICK_RATE", "0.08"))
MEDIAN_DELAY = float(os.environ.get("MAILER_MEDIAN_DELAY", "90"))
DAY_SECONDS = float(os.environ.get("SIM_DAY_SECONDS", "86400"))
BROWSER = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/126.0 Safari/537.36"

counters = Counters("sim_mailer")
for name, text in (("messages_total", "Messages accepted."),
                   ("duplicates_total", "Messages repeating an idempotency key."),
                   ("unauthorized_total", "Sends refused for a missing or wrong token."),
                   ("feedback_sent_total", "Feedback events the load balancer accepted."),
                   ("feedback_failed_total", "Feedback events that failed."),
                   ("opens_total", "Open pixel fetches that succeeded."),
                   ("clicks_total", "Link fetches that succeeded.")):
    counters.declare(name, text)

keys = JsonFile("/sim/keys/mailer.json", {})
lock = threading.Lock()
by_key = {}
recent = deque(maxlen=200)
queue = []
wake = threading.Condition()


def recipient_of(value):
    """The first recipient's address; the manifest maps /to/0/email, so a list of objects arrives."""
    if isinstance(value, list) and value:
        value = value[0]
    if isinstance(value, dict):
        value = value.get("email", "")
    return str(value or "")


def outcome_of(message_id, recipient):
    if recipient.split("@")[0].lower().startswith("bounce"):
        return "hard"
    digest = hashlib.sha256(f"{SEED}:{message_id}".encode()).digest()
    roll = int.from_bytes(digest[:8], "big") / 2**64
    if roll < COMPLAINT:
        return "complaint"
    if roll < COMPLAINT + HARD:
        return "hard"
    if roll < COMPLAINT + HARD + SOFT:
        return "soft"
    return "delivered"


def schedule(delay, action):
    with wake:
        heapq.heappush(queue, (time.time() + delay, id(action), action))
        wake.notify()


def worker():
    while True:
        with wake:
            while not queue or queue[0][0] > time.time():
                wake.wait(timeout=1.0 if not queue else max(0.0, queue[0][0] - time.time()))
            _, _, action = heapq.heappop(queue)
        try:
            action()
        except Exception:
            counters.inc("feedback_failed_total")


def post_feedback(message_id, event, bounce=None):
    config = keys.get()
    destination, secret = config.get("destination_id"), config.get("feedback_secret")
    if not destination or not secret:
        counters.inc("feedback_failed_total")
        return
    body = {"event": event, "id": str(uuid.uuid4()), "meta": {"message_id": message_id}}
    if bounce:
        body["bounce"] = {"kind": bounce}
    raw = json.dumps(body).encode()
    stamp = str(int(time.time()))
    signature = hmac.new(secret.encode(), stamp.encode() + b"." + raw, hashlib.sha256).hexdigest()
    request = urllib.request.Request(
        f"{LB}/api/v1/destinations/{destination}/feedback", data=raw, method="POST",
        headers={"Content-Type": "application/json", "X-Mailer-Timestamp": stamp,
                 "X-Mailer-Signature": f"sha256={signature}"})
    try:
        with urllib.request.urlopen(request, timeout=10):
            counters.inc("feedback_sent_total", event=event)
    except (urllib.error.URLError, OSError):
        counters.inc("feedback_failed_total")


def fetch(url, counter):
    request = urllib.request.Request(url.replace(PUBLIC, LB, 1), headers={"User-Agent": BROWSER})
    try:
        with urllib.request.urlopen(request, timeout=10):
            counters.inc(counter)
    except (urllib.error.URLError, OSError):
        pass


def delay():
    return random.lognormvariate(math.log(MEDIAN_DELAY), 0.8) * DAY_SECONDS / 86400.0


def engage(html_body):
    pixels = re.findall(r'src="([^"]*?/t/o/[^"]+)"', html_body)
    links = re.findall(r'href="([^"]*?/t/c/[^"]+)"', html_body)
    if pixels and random.random() < OPEN_RATE:
        schedule(delay(), lambda url=pixels[0]: fetch(url, "opens_total"))
    if links and random.random() < CLICK_RATE:
        schedule(delay(), lambda url=random.choice(links): fetch(url, "clicks_total"))


def accept(payload):
    # The manifest places the message id at /custom/message_id.
    message_id = (payload.get("custom") or {}).get("message_id") or str(uuid.uuid4())
    recipient = recipient_of(payload.get("to"))
    outcome = outcome_of(message_id, recipient)
    recent.append({"id": message_id, "to": recipient, "subject": payload.get("subject", ""),
                   "outcome": outcome})
    counters.inc("messages_total", outcome=outcome)
    if outcome == "delivered":
        schedule(random.uniform(0.2, 2.0), lambda: post_feedback(message_id, "delivered"))
        engage(payload.get("html") or "")
    elif outcome == "complaint":
        schedule(random.uniform(0.2, 2.0), lambda: post_feedback(message_id, "complained"))
    else:
        kind = "permanent" if outcome == "hard" else "transient"
        schedule(random.uniform(0.2, 2.0), lambda: post_feedback(message_id, "bounced", kind))
    return message_id


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, status, body=b"", kind="application/json"):
        self.send_response(status)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/healthz":
            self.reply(200, b"ok\n", "text/plain")
        elif self.path == "/metrics":
            self.reply(200, counters.render().encode(), "text/plain; version=0.0.4")
        elif self.path == "/messages.json":
            self.reply(200, json.dumps(list(recent)).encode())
        elif self.path.startswith("/v1/domains"):
            self.reply(200, b'{"records":[]}')
        else:
            self.reply(404)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        token = keys.get().get("token")
        if token and self.headers.get("Authorization") != f"Bearer {token}":
            counters.inc("unauthorized_total")
            self.reply(401, b'{"error":"unauthorized"}')
            return
        if self.path != "/v1/messages":
            self.reply(404)
            return
        try:
            payload = json.loads(raw)
        except ValueError:
            self.reply(400, b'{"error":"bad_json"}')
            return
        key = self.headers.get("Idempotency-Key")
        with lock:
            known = by_key.get(key) if key else None
            if known is None:
                known = accept(payload)
                if key:
                    by_key[key] = known
            else:
                counters.inc("duplicates_total")
        self.reply(200, json.dumps({"id": known}).encode())


if __name__ == "__main__":
    threading.Thread(target=worker, daemon=True).start()
    ThreadingHTTPServer(("0.0.0.0", int(sys.argv[1]) if len(sys.argv) > 1 else 8000),
                        Handler).serve_forever()
