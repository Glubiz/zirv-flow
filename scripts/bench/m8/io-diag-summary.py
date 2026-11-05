"""Summarises the M8_IO_DIAG samples of one arm (#397). Usage: io-diag-summary.py <raw dir>; prints JSON.

Reads <raw>/instrument/{pg-io.ndjson,cgroup-io.txt,waits.ndjson} and loadgen's per-second timeline
(<raw>/ingest.json, or <raw>/loaded/ingest.json for timers). Counter resets (a restart) clamp a delta to 0.
"""
import json
import pathlib
import sys

SLOW_MS = 262.0


def rank(values, p):
    ordered = sorted(values)
    if not ordered:
        return None
    return ordered[min(len(ordered), max(1, -(-len(ordered) * p // 100))) - 1]


def stats(values):
    values = list(values)
    if not values:
        return None
    return dict(n=len(values), mean=sum(values) / len(values), p50=rank(values, 50), p99=rank(values, 99), max=max(values))


def deltas(series):
    return [max(0, b - a) for a, b in zip(series, series[1:])]


def lines(path):
    if not path.exists():
        return []
    out = []
    for line in path.read_text().splitlines():
        try:
            out.append(json.loads(line))
        except ValueError:
            continue
    return out


def pg_io(rows):
    wal = [r['wal'] for r in rows if r.get('wal')]
    syncs = deltas([w['wal_sync'] for w in wal])
    sync_ms = deltas([w['wal_sync_time'] for w in wal])
    per_sync = [t / n for n, t in zip(syncs, sync_ms) if n > 0]
    out = dict(samples=len(rows), wal_sync_total=sum(syncs), wal_sync_seconds=len(per_sync),
               wal_sync_ms_per_sync=stats(per_sync),
               wal_write_total=sum(deltas([w['wal_write'] for w in wal])),
               wal_buffers_full_total=sum(deltas([w['wal_buffers_full'] for w in wal])))
    wb_bytes, by_type = [], {}
    previous = None
    for r in rows:
        now = {(i['backend_type'], i['context'], i['object']): i for i in r.get('io', [])}
        if previous is not None:
            second = 0
            for key, i in now.items():
                before = previous.get(key, {})
                size = i.get('op_bytes') or 8192
                writes = max(0, (i.get('writebacks') or 0) - (before.get('writebacks') or 0)) * size
                second += writes
                kind = by_type.setdefault(key[0], dict(writebacks_bytes=0, writes=0, fsyncs=0))
                kind['writebacks_bytes'] += writes
                kind['writes'] += max(0, (i.get('writes') or 0) - (before.get('writes') or 0))
                kind['fsyncs'] += max(0, (i.get('fsyncs') or 0) - (before.get('fsyncs') or 0))
            wb_bytes.append(second)
        previous = now
    out['writeback_bytes_per_second'] = stats(wb_bytes)
    out['io_by_backend_type'] = by_type
    bg = [r['bgwriter'] for r in rows if r.get('bgwriter')]
    out['maxwritten_clean_total'] = sum(deltas([b['maxwritten_clean'] for b in bg]))
    out['buffers_clean_total'] = sum(deltas([b['buffers_clean'] for b in bg]))
    out['temp_bytes_total'] = sum(deltas([r['temp_bytes'] or 0 for r in rows if 'temp_bytes' in r]))
    return out


def cgroup(path):
    if not path.exists():
        return dict(available=False)
    source, blocks, cur = '', [], None
    for line in path.read_text().splitlines():
        if line.startswith('@@source'):
            source = line[9:]
        elif line.startswith('@@t '):
            cur = dict(io=[], mem=[], in_mem=False)
            blocks.append(cur)
        elif cur is None:
            continue
        elif line.startswith('@@mem'):
            cur['in_mem'] = True
        elif line.startswith('@@end'):
            cur['in_mem'] = False
        elif cur['in_mem']:
            cur['mem'].append(line.strip())
        else:
            cur['io'].append(line.strip())

    def written(block):
        total, seen = 0, False
        for entry in block['io']:
            parts = entry.split()
            if len(parts) >= 3 and parts[1] == 'Write' and parts[2].isdigit():
                total, seen = total + int(parts[2]), True
            for part in parts[1:]:
                if part.startswith('wbytes='):
                    total, seen = total + int(part[7:]), True
        return total if seen else None

    totals = [written(b) for b in blocks]
    totals = [t for t in totals if t is not None]
    swap = {}
    for b in blocks:
        for entry in b['mem']:
            if entry.startswith(('SwapTotal:', 'SwapFree:')):
                key, value = entry.split(':')
                swap.setdefault(key, []).append(int(value.split()[0]))
            elif entry.isdigit():
                swap.setdefault('memory.swap.current', []).append(int(entry))
    return dict(available=bool(totals), source=source, samples=len(blocks), write_bytes_per_second=stats(deltas(totals)),
                write_bytes_total=sum(deltas(totals)),
                swap_kb={k: dict(min=min(v), max=max(v)) for k, v in swap.items() if v})


def waits(rows):
    counts, samples = {}, 0
    for r in rows:
        samples += 1
        for w in r.get('waits', []):
            if w.get('wait_event') is None:
                continue
            key = '|'.join(str(w.get(k)) for k in ('application_name', 'backend_type', 'wait_event_type', 'wait_event'))
            counts[key] = counts.get(key, 0) + w.get('backends', 0)
    top = sorted(counts.items(), key=lambda kv: -kv[1])[:20]
    return dict(samples=samples, keyed_by='application_name|backend_type|wait_event_type|wait_event',
                top_backend_samples=dict(top))


def latency(raw):
    for name in ('ingest.json', 'loaded/ingest.json'):
        path = raw / name
        if path.exists():
            report = json.loads(path.read_text())
            timeline = report.get('timeline_from_schedule') or []
            slow = [t for t in timeline if t.get('max_ms', 0) > SLOW_MS]
            return dict(source=name, p99_ms=report.get('total', {}).get('latency_from_schedule', {}).get('p99_ms'),
                        seconds=len(timeline), slow_second_threshold_ms=SLOW_MS, slow_seconds=len(slow),
                        slow_second_indexes=[t['second'] for t in slow])
    return None


def main():
    raw = pathlib.Path(sys.argv[1])
    inst = raw / 'instrument'
    print(json.dumps(dict(
        measure='io_diag', latency=latency(raw), pg=pg_io(lines(inst / 'pg-io.ndjson')),
        cgroup=cgroup(inst / 'cgroup-io.txt'), waits=waits(lines(inst / 'waits.ndjson'))), indent=2))


main()
