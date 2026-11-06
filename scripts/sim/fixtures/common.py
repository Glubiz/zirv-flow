"""Helpers the sim fixtures share: Prometheus counters, a JSON file that reloads on change."""

import json
import threading
from pathlib import Path


class Counters:
    """Named counters with optional labels, rendered in the Prometheus text format."""

    def __init__(self, prefix):
        self.prefix = prefix
        self.lock = threading.Lock()
        self.values = {}
        self.help = {}

    def declare(self, name, text):
        self.help[name] = text
        with self.lock:
            self.values.setdefault((name, ()), 0)

    def inc(self, name, amount=1, **labels):
        key = (name, tuple(sorted(labels.items())))
        with self.lock:
            self.values[key] = self.values.get(key, 0) + amount

    def get(self, name, **labels):
        with self.lock:
            return self.values.get((name, tuple(sorted(labels.items()))), 0)

    def render(self):
        lines = []
        with self.lock:
            items = sorted(self.values.items())
        seen = set()
        for (name, labels), value in items:
            full = f"{self.prefix}_{name}"
            if name not in seen:
                seen.add(name)
                lines.append(f"# HELP {full} {self.help.get(name, name)}")
                lines.append(f"# TYPE {full} counter")
            text = ",".join(f'{k}="{v}"' for k, v in labels)
            lines.append(f"{full}{{{text}}} {value}" if text else f"{full} {value}")
        return "\n".join(lines) + "\n"


class JsonFile:
    """A JSON file read again whenever its modification time changes; missing means `default`."""

    def __init__(self, path, default):
        self.path = Path(path)
        self.default = default
        self.mtime = None
        self.value = default

    def get(self):
        try:
            mtime = self.path.stat().st_mtime_ns
        except OSError:
            return self.default
        if mtime != self.mtime:
            try:
                self.value = json.loads(self.path.read_text())
                self.mtime = mtime
            except (OSError, ValueError):
                return self.value
        return self.value
