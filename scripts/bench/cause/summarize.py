#!/usr/bin/env python3
"""Turn one cause-bench run directory into one JSON file (stdlib only).

Usage: python3 -I summarize.py DIR ISSUE ARM RUN VARIABLE RC LABEL IMAGE_TAG OUT_JSON
       python3 -I summarize.py --self-test
Missing optional files become null; a missing required file (#76 loadgen.json, #207
raw/<mode>/window.json) exits non-zero with a message.
"""
import csv
import datetime
import json
import pathlib
import re
import sys
import tempfile

SQL_VIA = 'psql shim (.zirv/m8shim/psql, docker exec into the bench PostgreSQL container)'
# zirv_flow_http_request_duration_seconds is recorded in crates/zirv-flow-api/src/metrics.rs:48
# and bucketed by HTTP_DURATION_BUCKETS in crates/zirv-flow/src/main.rs:789.
HTTP_HIST = 'zirv_flow_http_request_duration_seconds'
INGEST_ROUTE = {'batch': '/api/v1/ingest/batch', 'track': '/api/v1/ingest/track'}
# crates/zirv-flow-jobs/src/metrics.rs:195 and :40, counter zirv_flow_jobs_total at :33.
WAKE_HIST = 'zirv_flow_timer_wake_lateness_seconds'
JOB_DURATION = 'zirv_flow_job_duration_seconds'
JOBS_TOTAL = 'zirv_flow_jobs_total'

SAMPLE = re.compile(r'^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{(.*)\})?\s+(\S+)')
LABEL = re.compile(r'([a-zA-Z_][a-zA-Z0-9_]*)="((?:[^"\\]|\\.)*)"')


class Missing(Exception):
    pass


def read_text(path):
    return path.read_text() if path.is_file() else None


def read_json(path, required=False):
    text = read_text(path)
    if text is None or not text.strip():
        if required:
            raise Missing(f'required file missing or empty: {path}')
        return None
    return json.loads(text)


def parse_prom(text):
    if text is None:
        return None
    samples = []
    for line in text.splitlines():
        if line.startswith('#') or not line.strip():
            continue
        match = SAMPLE.match(line)
        if match:
            samples.append((match.group(1), dict(LABEL.findall(match.group(2) or '')), float(match.group(3))))
    return samples


def histogram(samples, name, where=None):
    """Cumulative buckets {le: count} (summed over matching series), plus count and sum."""
    buckets, count, total = {}, 0.0, 0.0
    for sample_name, labels, value in samples:
        if not sample_name.startswith(name) or not all(labels.get(k) == v for k, v in (where or {}).items()):
            continue
        if sample_name == name + '_bucket':
            le = float('inf') if labels['le'] == '+Inf' else float(labels['le'])
            buckets[le] = buckets.get(le, 0.0) + value
        elif sample_name == name + '_count':
            count += value
        elif sample_name == name + '_sum':
            total += value
    return buckets, count, total


def delta(after, before):
    return {le: after[le] - before.get(le, 0.0) for le in after}


def quantile(buckets, q):
    """Prometheus-style linear interpolation over cumulative buckets; None with no observations."""
    bounds = sorted(buckets)
    if not bounds or buckets[bounds[-1]] <= 0:
        return None
    rank = q * buckets[bounds[-1]]
    previous_bound, previous_count = 0.0, 0.0
    for bound in bounds:
        if buckets[bound] >= rank:
            if bound == float('inf'):
                return previous_bound  # lower bound of the +Inf bucket: the value is at least this
            span = buckets[bound] - previous_count
            return bound if span <= 0 else previous_bound + (bound - previous_bound) * (rank - previous_count) / span
        previous_bound, previous_count = bound, buckets[bound]
    return None


def latency_block(buckets, count, total):
    count = max(buckets.values(), default=0.0)  # the +Inf bucket is the observation count
    inf_hits = buckets.get(float('inf'), 0.0) - max((v for k, v in buckets.items() if k != float('inf')), default=0.0)
    return {
        'count': count,
        'mean_s': total / count if count else None,
        'p50_s': quantile(buckets, 0.5),
        'p95_s': quantile(buckets, 0.95),
        'p99_s': quantile(buckets, 0.99),
        'observations_above_last_bucket': inf_hits,
    }


def histogram_delta(after_text, before_text, name, where):
    after, before = parse_prom(after_text), parse_prom(before_text)
    if after is None or before is None:
        return None
    a_buckets, a_count, a_sum = histogram(after, name, where)
    b_buckets, b_count, b_sum = histogram(before, name, where)
    if not a_buckets:
        return None
    return latency_block(delta(a_buckets, b_buckets), a_count - b_count, a_sum - b_sum)


def jobs_delta(after_text, before_text):
    after = parse_prom(after_text)
    if after is None:
        return None
    before = parse_prom(before_text) or []
    totals = {}
    for sign, samples in ((1, after), (-1, before)):
        for name, labels, value in samples:
            if name == JOBS_TOTAL:
                key = f"{labels.get('kind')}/{labels.get('outcome')}"
                totals[key] = totals.get(key, 0.0) + sign * value
    return totals


def parse_time(text):
    return datetime.datetime.fromisoformat(text.replace('Z', '+00:00')).timestamp()


def monitor_stats(path, start=None, end=None):
    """Per container mean and max CPU % over [start, end] epoch seconds (all rows when None)."""
    text = read_text(path)
    if text is None:
        return None
    per = {}
    for row in csv.reader(text.splitlines()):
        if len(row) < 3:
            continue
        try:
            at = parse_time(row[0])
            cpu = float(row[2].rstrip('%'))
        except ValueError:
            continue
        if (start is not None and at < start) or (end is not None and at > end):
            continue
        per.setdefault(row[1], []).append(cpu)
    return {name: {'samples': len(v), 'cpu_mean_pct': sum(v) / len(v), 'cpu_max_pct': max(v)}
            for name, v in sorted(per.items())}


def inbox_stats(path):
    text = read_text(path)
    if text is None:
        return None
    depth, age = [], []
    for row in csv.reader(text.splitlines()):
        try:
            depth.append(float(row[1]))
            age.append(float(row[2]))
        except (ValueError, IndexError):
            continue  # a failed sample is written as "time,,"
    return {'samples': len(depth), 'max_depth': max(depth, default=None), 'max_age_s': max(age, default=None)}


def topology(path):
    text = read_text(path)
    if text is None:
        return None
    return [json.loads(line) for line in text.splitlines() if line.strip()]


def number_file(path):
    text = read_text(path)
    return float(text.strip()) if text and text.strip() else None


def ingest_results(d):
    loadgen = read_json(d / 'loadgen.json', required=True)
    params = (read_text(d / 'params') or '').split()
    mode = params[2] if len(params) == 3 else loadgen.get('mode')
    route = INGEST_ROUTE.get(mode)
    serve_after, serve_before = read_text(d / 'serve-after.prom'), read_text(d / 'serve-before.prom')
    total = loadgen.get('total') or {}
    start = loadgen.get('started_unix_s')
    end = start + total['elapsed_s'] if start is not None and total.get('elapsed_s') is not None else None
    return {
        'params': dict(zip(('rate', 'seconds', 'mode'), params)) if len(params) == 3 else None,
        'loadgen': {k: total.get(k) for k in ('requests', 'items_offered', 'items_accepted', 'achieved_rate',
                                              'elapsed_s', 'status', 'latency_from_schedule', 'service_latency')},
        'server_request_latency': None if route is None else histogram_delta(
            serve_after, serve_before, HTTP_HIST, {'method': 'POST', 'route': route}) and
        {'route': route, **histogram_delta(serve_after, serve_before, HTTP_HIST, {'method': 'POST', 'route': route})},
        'worker_jobs_delta': jobs_delta(read_text(d / 'worker-after.prom'), read_text(d / 'worker-before.prom'))
        if (d / 'worker-before.prom').is_file() else None,
        'cpu_over_load_window': monitor_stats(d / 'monitor.csv', start, end),
        'inbox': inbox_stats(d / 'inbox.csv'),
        'drain_after_load_s': number_file(d / 'drain_after_load_s'),
        'final_state': read_json(d / 'final-state.json'),
    }


def timers_results(d):
    modes = {}
    for window in sorted((d / 'raw').glob('*/window.json')):
        mode_dir = window.parent
        info = read_json(window, required=True)
        start = parse_time(info['t0']) if info.get('t0') else None
        end = start + float(info['window_s']) if start is not None and info.get('window_s') is not None else None
        samples = read_text(mode_dir / 'job-kinds.csv')
        modes[mode_dir.name] = {
            'window': info,
            'timers': read_json(mode_dir / 'timers.json'),
            'lateness': read_json(mode_dir / 'lateness.json'),
            'ingest': read_json(mode_dir / 'ingest.json'),
            'job_kind_samples': None if samples is None else max(0, len(samples.splitlines()) - 1),
            'job_kind_samples_file': str(mode_dir / 'job-kinds.csv') if samples is not None else None,
            'cpu_over_window': monitor_stats(d / 'monitor.csv', start, end),
        }
    if not modes:
        raise Missing(f'required file missing: {d}/raw/<mode>/window.json')
    worker_after = read_text(d / 'worker-after.prom')
    wake = None
    if worker_after is not None:
        buckets, count, total = histogram(parse_prom(worker_after), WAKE_HIST)
        wake = latency_block(buckets, count, total) if buckets else None
    return {
        'modes': modes,
        'lateness_burst': read_json(d / 'lateness-burst.json'),
        'worker_wake_lateness_histogram': wake,
        'worker_jobs_total': jobs_delta(worker_after, None),
        'inbox': inbox_stats(d / 'inbox.csv'),
    }


def summarize(d, issue, arm, run, variable, rc, label, image_tag):
    d = pathlib.Path(d)
    results = {'76': ingest_results, '207': timers_results}[issue](d)
    return {'label': label, 'image_version': image_tag, 'issue': int(issue), 'arm': arm, 'run': int(run),
            'variable': variable, 'exit_code': int(rc), 'sql_via': SQL_VIA,
            'topology': topology(d / 'topology.ndjson'), 'results': results}


def main(argv):
    if len(argv) != 10:
        print(__doc__, file=sys.stderr)
        return 2
    try:
        doc = summarize(*argv[1:9])
    except (Missing, KeyError) as error:
        print(f'summarize: {error}', file=sys.stderr)
        return 1
    out = pathlib.Path(argv[9])
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(doc, indent=2) + '\n')
    return 0


def prom(name, route_buckets, extra=''):
    lines = [f'{name}_bucket{{method="POST",route="/api/v1/ingest/batch",le="{le}"}} {n}' for le, n in route_buckets]
    return '\n'.join(lines) + '\n' + extra


def self_test():
    close = lambda a, b: a is not None and abs(a - b) < 1e-9
    with tempfile.TemporaryDirectory() as tmp:
        root = pathlib.Path(tmp)
        # #76: before has 10 obs all <=0.1; after adds 100 obs: 90 in (0,0.1], 10 in (0.1,1.0].
        d = root / '76'
        d.mkdir()
        (d / 'params').write_text('2000 300 batch\n')
        (d / 'loadgen.json').write_text(json.dumps({'mode': 'batch', 'started_unix_s': 1000, 'total': {
            'requests': 100, 'elapsed_s': 60.0, 'achieved_rate': 5.0}}))
        before = [('0.1', 10), ('1', 10), ('+Inf', 10)]
        after = [('0.1', 100), ('1', 110), ('+Inf', 110)]
        (d / 'serve-before.prom').write_text(prom(HTTP_HIST, before))
        (d / 'serve-after.prom').write_text(prom(HTTP_HIST, after))
        (d / 'monitor.csv').write_text(
            '1970-01-01T00:16:41Z,c1,10.00%,1GiB / 2GiB\n1970-01-01T00:16:50Z,c1,30.00%,1GiB / 2GiB\n'
            '1970-01-01T01:00:00Z,c1,99.00%,1GiB / 2GiB\n')
        (d / 'inbox.csv').write_text('t,5,1.5\nt,,\nt,9,4.0\n')
        out = root / 'out' / 'x.json'
        assert main(['s', str(d), '76', 'A', '1', 'v', '0', 'L', '0.1.0', str(out)]) == 0, 'ingest summarize'
        doc = json.loads(out.read_text())
        latency = doc['results']['server_request_latency']
        # p99: rank 99 of 100; first bucket holds 90, second reaches 100: 0.1 + 0.9 * 9/10 = 0.91.
        assert close(latency['p99_s'], 0.91), latency
        assert close(latency['p50_s'], 0.1 * 50 / 90), latency
        assert latency['count'] == 100
        assert doc['label'] == 'L' and doc['arm'] == 'A' and doc['sql_via'] == SQL_VIA
        assert doc['results']['worker_jobs_delta'] is None and doc['results']['drain_after_load_s'] is None
        assert doc['topology'] is None and doc['results']['final_state'] is None
        cpu = doc['results']['cpu_over_load_window']['c1']
        assert cpu['samples'] == 2 and close(cpu['cpu_mean_pct'], 20.0) and cpu['cpu_max_pct'] == 30.0, cpu
        assert doc['results']['inbox'] == {'samples': 2, 'max_depth': 9.0, 'max_age_s': 4.0}
        # Missing required file for #76.
        (d / 'loadgen.json').unlink()
        assert main(['s', str(d), '76', 'A', '1', 'v', '0', 'L', '0.1.0', str(root / 'o2.json')]) == 1
        assert not (root / 'o2.json').exists()
        # #207: window.json required, everything else optional.
        t = root / '207'
        (t / 'raw' / 'loaded').mkdir(parents=True)
        out = root / 'o3.json'
        assert main(['s', str(t), '207', 'F', '2', 'v', '0', 'L', '0.1.0', str(out)]) == 1
        (t / 'raw' / 'loaded' / 'window.json').write_text(json.dumps({'t0': '1970-01-01T00:16:40+00:00', 'window_s': 600}))
        (t / 'worker-after.prom').write_text(
            f'{WAKE_HIST}_bucket{{le="1"}} 50\n{WAKE_HIST}_bucket{{le="+Inf"}} 100\n{WAKE_HIST}_count 100\n')
        assert main(['s', str(t), '207', 'F', '2', 'v', '0', 'L', '0.1.0', str(out)]) == 0
        doc = json.loads(out.read_text())
        loaded = doc['results']['modes']['loaded']
        assert loaded['lateness'] is None and loaded['job_kind_samples'] is None and loaded['cpu_over_window'] is None
        assert doc['results']['lateness_burst'] is None and doc['results']['inbox'] is None
        wake = doc['results']['worker_wake_lateness_histogram']
        assert wake['count'] == 100 and wake['p99_s'] == 1.0 and wake['observations_above_last_bucket'] == 50.0, wake
    print('self-test ok')
    return 0


if __name__ == '__main__':
    sys.exit(self_test() if sys.argv[1:] == ['--self-test'] else main(sys.argv))
