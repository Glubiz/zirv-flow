"""Shared by the traffic generators: control file, daily curve, bursts, open-loop pacing, stats."""

import json
import math
import os
import random
import threading
import time
from collections import deque
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

CONTROL = Path("/sim/control.json")
STATS_DIR = Path("/sim/stats")
LATE_AFTER = 1.0  # seconds a chunk may wait past its due time before it counts as late
MAX_LAG = 30.0  # owed work older than this is dropped and counted, never silently

# Share of the total rate each generator carries (design 4.1).
SHARE = {"ingest": 0.68, "kafka": 0.22, "changes": 0.08, "forms": 0.02}


def _hump(hour, centre, width):
    d = min(abs(hour - centre), 24 - abs(hour - centre))
    return math.exp(-0.5 * (d / width) ** 2)


def _raw(hour):
    return 0.3 + 1.0 * _hump(hour, 11, 2.2) + 1.2 * _hump(hour, 20, 2.2)


_MEAN = sum(_raw(i / 100) for i in range(2400)) / 2400


def curve(hour):
    """Multiplier of the daily mean at `hour` (0 to 24): two humps, mean 1.0."""
    return _raw(hour % 24) / _MEAN


class Control:
    """/sim/control.json, re-read when its modification time changes."""

    def __init__(self):
        self.mtime = None
        self.value = {"rate": 250, "day_seconds": 86400, "burst_factor": 1.0,
                      "burst_until": None, "paused": False}

    def get(self):
        try:
            mtime = CONTROL.stat().st_mtime_ns
            if mtime != self.mtime:
                self.value = json.loads(CONTROL.read_text())
                self.mtime = mtime
        except (OSError, ValueError):
            pass
        return self.value


def write_control(update):
    """Atomic read-modify-write of the control file."""
    try:
        value = json.loads(CONTROL.read_text())
    except (OSError, ValueError):
        value = {}
    value.update(update)
    tmp = CONTROL.with_suffix(".tmp")
    tmp.write_text(json.dumps(value))
    os.replace(tmp, CONTROL)


def scheduled_burst(now, day_seconds):
    """Random bursts, about three per sim-day, 60 to 180 s, factor 3; the same in every process."""
    day = int(now // day_seconds)
    rnd = random.Random(day * 7919 + 13)
    for _ in range(3):
        start = day * day_seconds + rnd.random() * (day_seconds - 200)
        length = rnd.uniform(60, 180)
        if start <= now < start + length:
            return 3.0
    return 1.0


def factor(control, now=None):
    """Curve position times burst: the multiplier of `rate` right now."""
    now = now or time.time()
    day_seconds = float(control.get("day_seconds") or 86400)
    hour = (now % day_seconds) / day_seconds * 24
    burst = scheduled_burst(now, day_seconds)
    until = control.get("burst_until")
    if until and now < until:
        burst = max(burst, float(control.get("burst_factor") or 1.0))
    return curve(hour) * burst


class Stats:
    """Counters of one generator, written to /sim/stats/<name>.json every few seconds."""

    def __init__(self, name, part=0):
        self.name = name
        self.part = part
        self.lock = threading.Lock()
        self.sent = 0
        self.late = 0
        self.dropped = 0
        self.errors = 0
        self.target = 0.0
        self.window = deque()
        self.started = time.time()

    def done(self, count):
        with self.lock:
            self.sent += count
            self.window.append((time.time(), count))

    def add(self, **kw):
        with self.lock:
            for key, value in kw.items():
                setattr(self, key, getattr(self, key) + value)

    def achieved(self, seconds=30):
        now = time.time()
        with self.lock:
            while self.window and self.window[0][0] < now - 60:
                self.window.popleft()
            span = min(seconds, now - self.started)
            total = sum(c for t, c in self.window if t >= now - seconds)
        return total / span if span > 0 else 0.0

    def write(self, extra=None):
        STATS_DIR.mkdir(parents=True, exist_ok=True)
        body = {"name": self.name, "ts": time.time(), "target_per_s": round(self.target, 1),
                "achieved_per_s": round(self.achieved(), 1), "sent_total": self.sent,
                "late_total": self.late, "dropped_total": self.dropped, "errors_total": self.errors}
        body.update(extra or {})
        # Part 0 keeps the single-process file name; further parts of one generator write
        # <name>.<part>.json with the same "name", and sim.sh status adds the parts up.
        stem = self.name if self.part == 0 else f"{self.name}.{self.part}"
        tmp = STATS_DIR / f".{stem}.tmp"
        tmp.write_text(json.dumps(body))
        os.replace(tmp, STATS_DIR / f"{stem}.json")


def run(name, send, chunk_items=1, workers=8, make_extra=None, part=0, parts=1):
    """Open-loop pacing. `send(count)` is called from a worker for each chunk and returns the
    number of items it delivered (or raises). Work owed accumulates at the target rate whether or
    not the senders keep up; a chunk started more than LATE_AFTER seconds after its due time is
    late, and owed work older than MAX_LAG is dropped and counted. A generator split over
    `parts` processes runs one `run` per process (`part` 0 to parts-1); each paces 1/parts of the
    generator's rate, so together they send what one process would."""
    control = Control()
    stats = Stats(name, part)
    pool = ThreadPoolExecutor(max_workers=workers)
    sem = threading.Semaphore(workers * 2)
    owed = 0.0
    due = deque()  # (due time, count)
    last = time.time()
    last_write = 0.0

    def job(due_at, count):
        try:
            if time.time() - due_at > LATE_AFTER:
                stats.add(late=1)
            delivered = send(count)
            stats.done(count if delivered is None else delivered)
        except Exception as exc:  # a failed send is an error count, never a crash
            stats.add(errors=1)
            print(f"{name}: send failed: {type(exc).__name__}", flush=True)
        finally:
            sem.release()

    while True:
        time.sleep(0.05)
        now = time.time()
        ctl = control.get()
        rate = 0.0
        if not ctl.get("paused"):
            rate = float(ctl.get("rate") or 0) * SHARE[name.removeprefix("gen-")] * factor(ctl, now) / parts
        stats.target = rate
        owed += rate * (now - last)
        last = now
        while owed >= chunk_items:
            owed -= chunk_items
            due.append((now - owed / rate if rate else now, chunk_items))
        while due and now - due[0][0] > MAX_LAG:
            stats.add(dropped=due.popleft()[1])
        while due and sem.acquire(blocking=False):
            due_at, count = due.popleft()
            pool.submit(job, due_at, count)
        if now - last_write >= 5:
            last_write = now
            stats.write(make_extra() if make_extra else None)
