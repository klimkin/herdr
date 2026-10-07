#!/usr/bin/env python3
"""Absolute CPU over complete bracketed scans; no causal comparison is implied."""
from collections import defaultdict
import math


def cpu_report(resources, start, end, run, controller_pid, tick_hz, minimum_seconds=180):
    issues, points, rejected = [], [], []
    integer = lambda value: type(value) is int and 0 <= value < 1 << 64
    if (not integer(start) or not integer(end) or start >= end or type(tick_hz) is not int or tick_hz <= 0
            or type(minimum_seconds) not in (int,float) or not math.isfinite(minimum_seconds) or minimum_seconds <= 0):
        return {'runtime_process_coverage_complete':False,'total_herdr_cpu_percent':None,
                'validation_errors':['Invalid window/tick frequency'],'cpu_percent':{},'by_process':{}}
    previous_end = None
    for index, point in enumerate(resources):
        begin, finish = point.get('ns'), point.get('sample_end_ns')
        if type(begin) is not int or type(finish) is not int or begin > finish:
            issues.append(f'Invalid whole scan {index}')
            continue
        if previous_end is not None and begin < previous_end:
            issues.append(f'Unordered or overlapping whole scan {index}')
        previous_end = finish
        if begin < start or finish > end:
            rejected.append(index)
            continue
        rows = point.get('processes', [])
        if any(not integer(row.get('pid')) or row['pid'] == 0
               or not integer(row.get('start_ticks')) or row['start_ticks'] == 0
               for row in rows) or len({(row.get('pid'),row.get('start_ticks')) for row in rows}) != len(rows):
            issues.append(f'Invalid or duplicate process identities in scan {index}')
            continue
        if any(type(row.get('sample_start_ns')) is not int
               or type(row.get('sample_end_ns')) is not int
               or not begin <= row['sample_start_ns'] <= row['sample_end_ns'] <= finish
               for row in rows):
            issues.append(f'Process read outside whole scan {index}')
            continue
        points.append(point)
    if len(points) < 2 or any(right['ns']-left['ns'] > 1_000_000_000 for left,right in zip(points,points[1:])):
        issues.append('Resource scan cadence/coverage incomplete')
    identities = defaultdict(list)
    for point in points:
        for row in point.get('processes', []):
            identities[(row['pid'], row['start_ticks'])].append(row)
    roles = defaultdict(list)
    client_pids = run.get('client_pids', [])
    metadata_valid = (type(run.get('server_pid')) is int and run['server_pid'] > 0
                      and all(type(pid) is int and pid > 0 for pid in client_pids)
                      and len(set(client_pids)) == len(client_pids)
                      and run['server_pid'] not in client_pids and tick_hz > 0)
    if not metadata_valid:
        issues.append('Invalid runtime process identities')
    for (pid, identity), rows in identities.items():
        if len(rows) < 2:
            issues.append(f'Missing CPU lifetime samples for {pid}/{identity}')
            continue
        first, last = rows[0], rows[-1]
        duration = ((last['sample_start_ns'] + last['sample_end_ns'])
                    - (first['sample_start_ns'] + first['sample_end_ns'])) / 2e9
        complete = len(rows) == len(points)
        if (any(type(row.get('ticks')) is not int or row['ticks'] < 0 for row in rows)
                or any(right['ticks'] < left['ticks'] for left, right in zip(rows, rows[1:]))):
            issues.append(f'Invalid/regressed CPU ticks for {pid}/{identity}')
            complete = False
        if duration <= 0 or duration < minimum_seconds:
            issues.append(f'CPU interval too short for {pid}/{identity}')
            complete = False
        role = ('server' if pid == run.get('server_pid') else 'clients' if pid in client_pids
                else 'controller' if pid == controller_pid else 'helpers'
                if first.get('command') and first['command'][0].endswith('latency_probe') else 'other_owned')
        roles[role].append({'pid':pid,'start_ticks':identity,'sample_count':len(rows),
                            'complete':complete,'measured_seconds':duration,
                            'cpu_percent':(last['ticks']-first['ticks'])/tick_hz/duration*100 if complete else None})
    coverage = (metadata_valid and len(roles.get('server', [])) == 1
                and len(roles.get('clients', [])) == len(client_pids)
                and all(row['complete'] for role in ('server','clients') for row in roles[role])
                and not issues)
    percents = {role:sum(row['cpu_percent'] for row in rows) if all(row['complete'] for row in rows) else None
                for role, rows in roles.items()}
    total = percents.get('server', 0) + percents.get('clients', 0) if coverage else None
    return {'runtime_process_coverage_complete':coverage,'total_herdr_cpu_percent':total,
            'cpu_percent':percents,'by_process':dict(roles),'validation_errors':issues,
            'retained_scans':len(points),'rejected_edge_scans':rejected,
            'semantics':'Absolute CPU/core from bracketed cumulative ticks. Missing lifetimes, coverage or intervals remain unavailable; no readiness regression comparison.',
            'observer_cpu':None,'scheduler_wakeups':None}
