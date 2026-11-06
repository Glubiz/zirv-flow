#!/usr/bin/env python3
"""TXT answers for sending-domain verification, from /sim/dns/records.json on every query.

Derived from the dns_reply of scripts/drill/upgrade-fixture.py. Any other query type gets an
empty NOERROR answer, so the resolver does not retry against the next server.
"""

import json
import socketserver
import struct
import sys
from pathlib import Path


def dns_reply(packet, records):
    offset = 12
    labels = []
    while packet[offset]:
        size = packet[offset]
        labels.append(packet[offset + 1:offset + 1 + size].decode("ascii"))
        offset += size + 1
    offset += 1
    kind, _ = struct.unpack("!HH", packet[offset:offset + 4])
    question = packet[12:offset + 4]
    value = records.get(".".join(labels).lower()) if kind == 16 else None
    answer = b""
    if value is not None:
        raw = value.encode("ascii")
        txt = b"".join(bytes([len(raw[i:i + 255])]) + raw[i:i + 255]
                       for i in range(0, len(raw), 255))
        answer = b"\xc0\x0c" + struct.pack("!HHIH", 16, 1, 0, len(txt)) + txt
    return (packet[:2] + struct.pack("!HHHHH", 0x8580, 1, bool(answer), 0, 0)
            + question + answer)


def serve(state):
    path = Path(state) / "dns" / "records.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    if not path.exists():
        path.write_text("{}")

    class DNS(socketserver.BaseRequestHandler):
        def handle(self):
            packet, sock = self.request
            try:
                records = json.loads(path.read_text())
                sock.sendto(dns_reply(packet, records), self.client_address)
            except (OSError, ValueError, IndexError, struct.error, UnicodeError):
                return

    with socketserver.ThreadingUDPServer(("0.0.0.0", 53), DNS) as server:
        server.serve_forever()


if __name__ == "__main__":
    serve(sys.argv[1] if len(sys.argv) > 1 else "/sim")
