#!/usr/bin/env python3
"""verify.sh helper: produces known keys (or one tombstone) to app-feature-used.

usage: kafka_probe.py produce <prefix> <count> <first customer number>
       kafka_probe.py tombstone <key>
"""

import json
import sys

from confluent_kafka import Producer

producer = Producer({"bootstrap.servers": "kafka:9092", "linger.ms": 5})


def main(argv):
    if argv[0] == "produce":
        prefix, count, first = argv[1], int(argv[2]), int(argv[3])
        for i in range(count):
            value = {"customer_id": f"C{first + i:07d}", "feature": "verify-probe", "duration_ms": 1}
            producer.produce("app-feature-used", key=f"{prefix}-{i}", value=json.dumps(value).encode())
    elif argv[0] == "tombstone":
        producer.produce("app-feature-used", key=argv[1], value=None)
    else:
        sys.exit("unknown command")
    producer.flush(30)


if __name__ == "__main__":
    main(sys.argv[1:])
