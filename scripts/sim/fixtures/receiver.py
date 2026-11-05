#!/usr/bin/env python3
"""The signed-webhook receiver: verifies X-Zirv-Signature, counts, optionally answers 503.

Secrets come from /sim/keys/receiver.json as {"secrets": ["..."]}; the call is accepted when any
signature header matches any listed secret. `fail_fraction` in /sim/receiver.json (or the
RECEIVER_FAIL_FRACTION variable) makes that share of verified calls answer 503, to exercise retries.
"""

import hashlib
import hmac
import json
import os
import random
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from common import Counters, JsonFile

TOLERANCE_SECONDS = 300
counters = Counters("sim_receiver")
for name, text in (("accepted_total", "Verified calls answered 2xx."),
                   ("bad_signature_total", "Calls refused for a missing or wrong signature."),
                   ("stale_total", "Calls refused for a timestamp outside the tolerance."),
                   ("duplicate_total", "Verified calls repeating an idempotency key."),
                   ("failed_total", "Verified calls answered 503 on purpose.")):
    counters.declare(name, text)

seen = set()
seen_lock = threading.Lock()
secrets = JsonFile("/sim/keys/receiver.json", {"secrets": []})
control = JsonFile("/sim/receiver.json", {})


def expected(secret, timestamp, body):
    message = timestamp.encode() + b"." + body
    return hmac.new(secret.encode(), message, hashlib.sha256).hexdigest()


def verify(headers, body):
    """Returns None when the call verifies, else a counter name."""
    timestamp = headers.get("X-Zirv-Timestamp", "")
    values = headers.get_all("X-Zirv-Signature") or []
    keys = secrets.get().get("secrets", [])
    if not timestamp.isdigit() or not values or not keys:
        return "bad_signature_total"
    for key in keys:
        want = "sha256=" + expected(key, timestamp, body)
        if any(hmac.compare_digest(want.lower(), value.strip().lower()) for value in values):
            if abs(time.time() - int(timestamp)) > TOLERANCE_SECONDS:
                return "stale_total"
            return None
    return "bad_signature_total"


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
        else:
            self.reply(404)

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        problem = verify(self.headers, body)
        if problem:
            counters.inc(problem)
            self.reply(401, json.dumps({"error": problem}).encode())
            return
        fraction = float(control.get().get(
            "fail_fraction", os.environ.get("RECEIVER_FAIL_FRACTION", "0")))
        if fraction > 0 and random.random() < fraction:
            counters.inc("failed_total")
            self.reply(503, b'{"error":"unavailable"}')
            return
        key = self.headers.get("X-Zirv-Idempotency-Key")
        if key:
            with seen_lock:
                repeat = key in seen
                seen.add(key)
            if repeat:
                counters.inc("duplicate_total")
        counters.inc("accepted_total")
        self.reply(200, b'{"ok":true}')


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", int(sys.argv[1]) if len(sys.argv) > 1 else 8000),
                        Handler).serve_forever()
