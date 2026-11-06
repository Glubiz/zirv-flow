#!/usr/bin/env python3
"""SMTP over implicit TLS on :2525 plus a mailbox viewer and /metrics on :8025.

Derived from the SMTP half of scripts/drill/upgrade-fixture.py. Accepts the credential
sim/sim (SIM_SMTP_USER and SIM_SMTP_PASSWORD override). A recipient whose local part starts with
"bounce", and about 0.5% of all others by hash, get 550. The last 500 messages are kept.
"""

import base64
import hashlib
import html
import json
import os
import socketserver
import ssl
import sys
import threading
from collections import deque
from email import policy
from email.parser import BytesParser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from common import Counters

USER = os.environ.get("SIM_SMTP_USER", "sim")
PASSWORD = os.environ.get("SIM_SMTP_PASSWORD", "sim")
REFUSE_PER_MILLE = int(os.environ.get("SIM_SMTP_REFUSE_PER_MILLE", "5"))
KEEP = 500

counters = Counters("sim_smtp")
counters.declare("messages_total", "Messages accepted.")
counters.declare("recipients_refused_total", "Recipients answered 550.")
counters.declare("auth_failed_total", "Refused AUTH attempts.")
messages = deque(maxlen=KEEP)
sequence = [0]  # the number of the last captured message, so a reader can tell new from old
lock = threading.Lock()


def refuses(recipient):
    local = recipient.split("@")[0].lower().lstrip("<")
    if local.startswith("bounce"):
        return True
    digest = hashlib.sha256(recipient.lower().encode()).digest()
    return int.from_bytes(digest[:4], "big") % 1000 < REFUSE_PER_MILLE


def captured(data, recipients):
    message = BytesParser(policy=policy.default).parsebytes(data)
    body = message.get_body(preferencelist=("html", "plain"))
    return {
        "subject": str(message["Subject"]),
        "from": str(message["From"]),
        "recipients": recipients,
        "body": body.get_content() if body else "",
    }


def smtp_handler(context):
    class SMTP(socketserver.BaseRequestHandler):
        def handle(self):
            try:
                self.session()
            except (ssl.SSLError, OSError):
                return

        def session(self):
            with context.wrap_socket(self.request, server_side=True) as conn:
                conn.settimeout(30)
                with conn.makefile("rwb") as stream:
                    def reply(message):
                        stream.write(message + b"\r\n")
                        stream.flush()

                    reply(b"220 smtp-sink ESMTP")
                    accepted = []
                    while line := stream.readline(8192):
                        command = line.split(b" ", 1)[0].strip().upper()
                        if command == b"EHLO":
                            reply(b"250-smtp-sink\r\n250 AUTH PLAIN LOGIN")
                        elif command == b"AUTH":
                            parts = line.strip().split()
                            if len(parts) == 2 and parts[1].upper() == b"PLAIN":
                                reply(b"334 ")
                                parts.append(stream.readline(8192).strip())
                            try:
                                valid = (len(parts) == 3 and parts[1].upper() == b"PLAIN"
                                         and base64.b64decode(parts[2])
                                         == f"\0{USER}\0{PASSWORD}".encode())
                            except ValueError:
                                valid = False
                            if not valid:
                                counters.inc("auth_failed_total")
                            reply(b"235 authenticated" if valid else b"535 refused")
                        elif command in (b"MAIL", b"RSET"):
                            accepted = []
                            reply(b"250 ok")
                        elif command == b"RCPT":
                            address = line.decode("ascii", "replace").split(":", 1)[-1].strip()
                            address = address.strip("<> \r\n")
                            if refuses(address):
                                counters.inc("recipients_refused_total")
                                reply(b"550 mailbox unavailable")
                            else:
                                accepted.append(address)
                                reply(b"250 ok")
                        elif command == b"DATA":
                            if not accepted:
                                reply(b"554 no valid recipients")
                                continue
                            reply(b"354 end with dot")
                            body = bytearray()
                            while True:
                                data = stream.readline(65536)
                                if not data:
                                    return
                                if data == b".\r\n":
                                    break
                                body.extend(data[1:] if data.startswith(b"..") else data)
                            try:
                                entry = captured(bytes(body), list(accepted))
                            except (ValueError, KeyError, LookupError):
                                entry = {"subject": "", "from": "", "recipients": accepted,
                                         "body": ""}
                            with lock:
                                sequence[0] += 1
                                entry["seq"] = sequence[0]
                                messages.append(entry)
                            counters.inc("messages_total")
                            reply(b"250 accepted")
                        elif command == b"QUIT":
                            reply(b"221 bye")
                            return
                        else:
                            reply(b"500 unsupported")

    return SMTP


class Viewer(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, body, kind):
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        with lock:
            snapshot = list(messages)
        if self.path == "/healthz":
            self.reply("ok\n", "text/plain")
        elif self.path == "/metrics":
            self.reply(counters.render(), "text/plain; version=0.0.4")
        elif self.path == "/messages.json":
            self.reply(json.dumps(snapshot), "application/json")
        else:
            rows = "".join(
                f"<tr><td>{html.escape(', '.join(m['recipients']))}</td>"
                f"<td>{html.escape(m['subject'])}</td></tr>" for m in reversed(snapshot))
            self.reply(f"<!doctype html><title>smtp-sink</title><h1>{len(snapshot)} messages</h1>"
                       f"<table>{rows}</table>", "text/html")


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == "__main__":
    tls = sys.argv[1] if len(sys.argv) > 1 else "/sim/tls"
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(f"{tls}/leaf.pem", f"{tls}/leaf.key")
    viewer = ThreadingHTTPServer(("0.0.0.0", 8025), Viewer)
    threading.Thread(target=viewer.serve_forever, daemon=True).start()
    with Server(("0.0.0.0", 2525), smtp_handler(ctx)) as smtp:
        smtp.serve_forever()
