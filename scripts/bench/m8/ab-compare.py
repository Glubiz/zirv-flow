"""Builds the comparison JSON of ab-settings.sh (#397).
Usage: ab-compare.py <out> <arm> <image tag> <runs per side> <complete 0|1> <data dir> <runs tsv: side, n, exit code, data file name>
Each run's metrics come from its data JSON (write_data); the per-side means skip runs without that metric.
"""
import datetime
import json
import pathlib
import sys


def dig(value, *path):
    for key in path:
        if not isinstance(value, dict) or key not in value:
            return None
        value = value[key]
    return value


def metrics(arm, results):
    m = {}
    if arm == 'ingest':
        m['p99_ms'] = dig(results, 'ingest.json', 'total', 'latency_from_schedule', 'p99_ms')
        m['max_ms'] = dig(results, 'ingest.json', 'total', 'latency_from_schedule', 'max_ms')
        m['achieved_rate'] = dig(results, 'ingest.json', 'total', 'achieved_rate')
        m['applied_p95_ms'] = dig(results, 'ingest-applied.json', 'p95_ms')
    else:
        for mode in ('noload', 'loaded'):
            for key in ('p99_s', 'max_s'):
                m[f'{mode}_{key}'] = dig(results, f'{mode}/timers.json', 'lateness', 'overall', key)
    diag = results.get('io-diag.json') or {}
    m['slow_seconds'] = dig(diag, 'latency', 'slow_seconds')
    m['slow_second_p99_ms'] = dig(diag, 'latency', 'p99_ms')
    for key in ('p50', 'p99', 'max'):
        m[f'wal_sync_ms_per_sync_{key}'] = dig(diag, 'pg', 'wal_sync_ms_per_sync', key)
    m['writeback_bytes_per_second_mean'] = dig(diag, 'pg', 'writeback_bytes_per_second', 'mean')
    m['writeback_bytes_per_second_max'] = dig(diag, 'pg', 'writeback_bytes_per_second', 'max')
    m['cgroup_write_bytes_per_second_max'] = dig(diag, 'cgroup', 'write_bytes_per_second', 'max')
    m['maxwritten_clean_total'] = dig(diag, 'pg', 'maxwritten_clean_total')
    m['wal_buffers_full_total'] = dig(diag, 'pg', 'wal_buffers_full_total')
    return m


def main():
    out, arm, image, per_side, complete, data_dir, tsv = sys.argv[1:]
    runs, order = [], ''
    for line in pathlib.Path(tsv).read_text().splitlines():
        side, n, rc, data = line.split('\t')
        order += side.upper()
        record = dict(side=side, n=int(n), exit_code=int(rc), data_file=data, metrics={}, show=None)
        path = pathlib.Path(data_dir) / data
        if path.exists():
            report = json.loads(path.read_text())
            record['metrics'] = metrics(arm, report.get('results', {}))
            record['show'] = dig(report, 'topology', 'pg_settings_side', 'show')
            record['image_version'] = report.get('image_version')
        runs.append(record)
    means = {}
    for side in ('a', 'b'):
        ok = [r for r in runs if r['side'] == side and r['exit_code'] == 0]
        names = sorted({k for r in ok for k in r['metrics']})
        means[side] = {'runs': len(ok)}
        for k in names:
            values = [r['metrics'][k] for r in ok if isinstance(r['metrics'].get(k), (int, float))]
            means[side][k] = sum(values) / len(values) if values else None
    report = dict(
        benchmark='ab-settings', arm=arm, image_version=image, smoke=False,
        exit_code=0 if complete == '1' and all(r['exit_code'] == 0 for r in runs) else 1,
        complete=complete == '1', runs_per_side=int(per_side), order=order,
        topology=dict(sides=dict(
            a='current M8 settings, with track_io_timing and track_wal_io_timing on',
            b='A plus backend_flush_after=256kB, bgwriter_lru_maxpages=1000, bgwriter_delay=50ms, wal_buffers=64MB')),
        recorded_at=datetime.datetime.now(datetime.timezone.utc).isoformat(), runs=runs, means=means)
    target = pathlib.Path(out)
    target.parent.mkdir(parents=True, exist_ok=True)
    tmp = target.with_suffix('.json.tmp')
    tmp.write_text(json.dumps(report, indent=2) + '\n')
    tmp.replace(target)


main()
