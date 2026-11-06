#!/usr/bin/env python3
"""Product events into the two Kafka topics (design 4.2), with a tombstone for about 0.1% of keys."""

import json
import random
import uuid

from confluent_kafka import Producer

import pace
import world

producer = Producer({"bootstrap.servers": "kafka:9092", "compression.type": "gzip",
                     "linger.ms": 20, "queue.buffering.max.messages": 100000})
recent = []


def send(count):
    for _ in range(count):
        if recent and random.random() < 0.001:
            topic, key = recent.pop(random.randrange(len(recent)))
            producer.produce(topic, key=key, value=None)
            continue
        n = random.randrange(1, world.PEOPLE + 1)
        key = "evt-" + uuid.uuid4().hex
        now = world.now_ts()
        if random.random() < 0.8:
            topic = "app-feature-used"
            value = {"customer_id": world.customer_id(n), "feature": random.choice(world.FEATURES),
                     "duration_ms": random.randrange(50, 120000), "at": now}
        else:
            topic = "app-license-activated"
            value = {"customer_id": world.customer_id(n), "plan": random.choice(world.PLANS)[0],
                     "seats": random.randrange(1, 50), "at": now}
        producer.produce(topic, key=key, value=json.dumps(value).encode())
        if len(recent) < 2000:
            recent.append((topic, key))
    producer.poll(0)


if __name__ == "__main__":
    pace.run("kafka", send, chunk_items=50, workers=1,
             make_extra=lambda: {"queued": len(producer)})
