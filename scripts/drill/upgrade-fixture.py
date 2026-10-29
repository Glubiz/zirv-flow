#!/usr/bin/env python3
"""Isolated drill DNS and TLS SMTP sink. Retains a delivery count, never message data."""

import base64
import json
from pathlib import Path
import socketserver
import ssl
import struct
import sys
import threading


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


def serve(directory):
    directory = Path(directory)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(directory / "smtp.pem", directory / "smtp.key")
    lock = threading.Lock()
    count = 0
    (directory / "arrivals").write_text("0")

    class DNS(socketserver.BaseRequestHandler):
        def handle(self):
            packet, sock = self.request
            records = json.loads((directory / "records.json").read_text())
            sock.sendto(dns_reply(packet, records), self.client_address)

    class SMTP(socketserver.BaseRequestHandler):
        def handle(self):
            nonlocal count
            with context.wrap_socket(self.request, server_side=True) as conn:
                conn.settimeout(30)
                with conn.makefile("rwb") as stream:
                    def reply(message):
                        stream.write(message + b"\r\n")
                        stream.flush()

                    reply(b"220 fixture ESMTP")
                    while line := stream.readline(8192):
                        command = line.split(b" ", 1)[0].strip().upper()
                        if command == b"EHLO":
                            reply(b"250-fixture\r\n250 AUTH PLAIN LOGIN")
                        elif command == b"AUTH":
                            # The isolated sink accepts the drill credential only.
                            parts = line.strip().split()
                            if len(parts) == 2 and parts[1].upper() == b"PLAIN":
                                reply(b"334 ")
                                parts.append(stream.readline(8192).strip())
                            valid = (len(parts) == 3 and parts[1].upper() == b"PLAIN"
                                     and base64.b64decode(parts[2]) == b"\0drill\0drill")
                            reply(b"235 authenticated" if valid else b"535 refused")
                        elif command in (b"MAIL", b"RCPT", b"RSET"):
                            reply(b"250 ok")
                        elif command == b"DATA":
                            reply(b"354 end with dot")
                            while True:
                                data = stream.readline(65536)
                                if not data:
                                    return
                                if data == b".\r\n":
                                    break
                            with lock:
                                count += 1
                                (directory / "arrivals").write_text(str(count))
                            reply(b"250 accepted")
                        elif command == b"QUIT":
                            reply(b"221 bye")
                            return
                        else:
                            reply(b"500 unsupported")

    class Server(socketserver.ThreadingTCPServer):
        allow_reuse_address = True
        daemon_threads = True

    with socketserver.ThreadingUDPServer(("0.0.0.0", 53), DNS) as dns, \
            Server(("0.0.0.0", 2525), SMTP) as smtp:
        threading.Thread(target=dns.serve_forever, daemon=True).start()
        (directory / "fixture-ready").touch()
        smtp.serve_forever()


if __name__ == "__main__":
    serve(sys.argv[1])
