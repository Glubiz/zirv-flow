#!/usr/bin/env python3
"""The "billing API": cursor-paginated accounts and payments out of SQLite.

GET /v1/accounts and /v1/payments take `limit`, `cursor` and `updated_since` and answer
{"data": [...], "next_cursor": <string or null>}. Every call needs the X-API-Key header when
/sim/keys/billing.json holds {"api_key": "..."}; without that file the API is open. The world
generator (gen/seed.py) fills /data/billing.db and gen/changes.py keeps writing to it; closed accounts stay
in the feed with status "closed" (the source maps that to a delete).
"""

import json
import sqlite3
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

from common import Counters, JsonFile

DB_PATH = "/data/billing.db"
counters = Counters("sim_billing")
counters.declare("requests_total", "Authorized list calls.")
counters.declare("refused_total", "Calls refused for a missing or wrong key.")
keys = JsonFile("/sim/keys/billing.json", {})

SCHEMA = """
CREATE TABLE IF NOT EXISTS accounts (
  account_id TEXT PRIMARY KEY, customer_id TEXT NOT NULL, status TEXT NOT NULL,
  balance_due REAL NOT NULL DEFAULT 0, payment_method TEXT, last_payment_at TEXT,
  updated_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS accounts_updated ON accounts (updated_at, account_id);
CREATE TABLE IF NOT EXISTS payments (
  payment_id TEXT PRIMARY KEY, customer_id TEXT NOT NULL, amount REAL NOT NULL,
  currency TEXT NOT NULL, received_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS payments_updated ON payments (updated_at, payment_id);
"""
TABLES = {"accounts": "account_id", "payments": "payment_id"}


def connect():
    db = sqlite3.connect(DB_PATH, timeout=10)
    db.row_factory = sqlite3.Row
    return db


def page(table, query):
    pk = TABLES[table]
    limit = max(1, min(int(query.get("limit", ["1000"])[0]), 5000))
    since = query.get("updated_since", [""])[0]
    after = query.get("cursor", [""])[0]
    # The cursor is the last (updated_at, pk) pair, so a page never skips or repeats a row.
    last_updated, _, last_pk = after.partition("|")
    sql = f"SELECT * FROM {table} WHERE updated_at >= ?"
    args = [since]
    if after:
        sql += f" AND (updated_at > ? OR (updated_at = ? AND {pk} > ?))"
        args += [last_updated, last_updated, last_pk]
    sql += f" ORDER BY updated_at, {pk} LIMIT ?"
    args.append(limit + 1)
    with connect() as db:
        rows = [dict(row) for row in db.execute(sql, args)]
    more = len(rows) > limit
    rows = rows[:limit]
    cursor = f"{rows[-1]['updated_at']}|{rows[-1][pk]}" if more else None
    return {"data": rows, "next_cursor": cursor}


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
        url = urlparse(self.path)
        if url.path == "/healthz":
            self.reply(200, b"ok\n", "text/plain")
            return
        if url.path == "/metrics":
            self.reply(200, counters.render().encode(), "text/plain; version=0.0.4")
            return
        wanted = keys.get().get("api_key")
        if wanted and self.headers.get("X-API-Key") != wanted:
            counters.inc("refused_total")
            self.reply(401, b'{"error":"unauthorized"}')
            return
        table = url.path.removeprefix("/v1/")
        if not url.path.startswith("/v1/") or table not in TABLES:
            self.reply(404, b'{"error":"not_found"}')
            return
        try:
            body = page(table, parse_qs(url.query))
        except ValueError:
            self.reply(400, b'{"error":"bad_request"}')
            return
        counters.inc("requests_total")
        self.reply(200, json.dumps(body).encode())


if __name__ == "__main__":
    with connect() as setup:
        setup.executescript(SCHEMA)
    ThreadingHTTPServer(("0.0.0.0", int(sys.argv[1]) if len(sys.argv) > 1 else 8000),
                        Handler).serve_forever()
