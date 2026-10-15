#!/usr/bin/env python3
"""Regenerate descriptive latency and CPU reports from raw remeasure captures."""

import argparse
from collections import defaultdict
import csv
import hashlib
import json
from pathlib import Path
from statistics import mean, median


PERCENTILES = (("p50", 50, 100), ("p90", 90, 100), ("p95", 95, 100),
               ("p97", 97, 100), ("p98", 98, 100), ("p99", 99, 100),
               ("p99.9", 999, 1000))
STATISTICS = ("mean", *(name for name, _, _ in PERCENTILES), "max")
THRESHOLDS = (5, 10, 16.667, 20, 50)
CPU_LABELS = ("Server CPU", "Client CPU", "Total CPU", "Owned CPU",
              "Console host CPU", "Observer CPU", "Probe CPU", "Owned other CPU",
              "Host CPU", "External CPU")
METHOD = {
    "pooling": "Raw observations grouped by workload, variant and measure; smoke excluded",
    "percentile": "Integer nearest rank, sorted[ceil(N*p)-1]",
    "p99.9_minimum_count": 10000,
    "cpu": "Portable cpu_ns deltas; percent of one core; unchanged process identities required",
    "cpu_endpoints": "Clip intervals to measurement bounds; interpolate cumulative counters linearly",
    "cpu_mean": "Weight each interval by its clipped duration; percentile ranks weight intervals equally",
    "visible_age": "Integrate the age of the latest observed generation over covered pane time",
    "comparisons": "Descriptive paired repeats; pooling establishes neither independence nor significance",
}


def distribution(values):
    ordered = sorted(values)
    count = len(ordered)
    result = {"count": count, "mean": mean(ordered) if count else None,
              "max": ordered[-1] if count else None}
    for name, numerator, denominator in PERCENTILES:
        rank = (count * numerator + denominator - 1) // denominator
        result[name] = (ordered[rank - 1] if count and
                        (name != "p99.9" or count >= 10000) else None)
    return result


def _process_name(name):
    return str(name).replace("\\", "/").rsplit("/", 1)[-1].lower().removesuffix(".exe")


def _console(name):
    return _process_name(name) in {
        "conhost", "conhost.exe", "openconsole", "openconsole.exe"}


def cpu_intervals(path, start, end, server_pid, client_pids, logical_cpus=None):
    """Keep clipped accounting intervals and reject incomplete or changed identities."""
    values = {label: [] for label in CPU_LABELS}
    if logical_cpus:
        values["External CPU, machine"] = []
    result = {"values": values, "seconds": [], "intervals": [], "failures": [],
              "coverage_ns": 0, "valid": False, "warnings": []}
    failures = result["failures"]
    try:
        rows = [json.loads(line) for line in Path(path).read_text().splitlines() if line.strip()]
        if end <= start:
            raise ValueError("measurement window must have positive duration")
        if len(rows) < 2 or rows[0]["ns"] > start or rows[-1]["ns"] < end:
            failures.append("resource snapshots do not bracket the measurement window")
        identities = {}
        tracked = {str(server_pid), *(str(pid) for pid in client_pids)}
        owned_identity = None
        for first, last in zip(rows, rows[1:]):
            a, b = first["ns"], last["ns"]
            if b <= a:
                raise ValueError("resource timestamps must increase strictly")
            left, right = max(start, a), min(end, b)
            if left >= right:
                continue
            interval = {"start_ns": left, "end_ns": right, "source_start_ns": a,
                        "source_end_ns": b, "seconds": (right - left) / 1e9}
            result["intervals"].append(interval)
            try:
                owned_first = {str(pid) for pid in first["owned"]} | tracked
                owned_last = {str(pid) for pid in last["owned"]} | tracked
                if owned_first != owned_last:
                    raise ValueError("owned process set changed")
                if owned_identity is not None and owned_first != owned_identity:
                    raise ValueError("owned process set changed during measurement")
                owned_identity = owned_first
                consoles = {pid for pid in owned_first
                            if _console(first["processes"][pid].get("name", ""))}
                observers = {pid for pid in owned_first
                             if _process_name(first["processes"][pid].get("name", "")) == "latency_bench"}
                probes = {pid for pid in owned_first
                          if _process_name(first["processes"][pid].get("name", "")) == "latency_probe"}
                groups = {"Server CPU": {str(server_pid)},
                          "Client CPU": {str(pid) for pid in client_pids},
                          "Total CPU": tracked, "Owned CPU": owned_first,
                          "Console host CPU": consoles, "Observer CPU": observers, "Probe CPU": probes,
                          "Owned other CPU": owned_first-tracked-consoles-observers-probes}
                deltas = {}
                for pid in owned_first:
                    process_a, process_b = first["processes"][pid], last["processes"][pid]
                    identity = (process_a["start"], process_a["ppid"], process_a.get("name", ""))
                    identity_b = (process_b["start"], process_b["ppid"], process_b.get("name", ""))
                    if identity != identity_b or identities.get(pid, identity) != identity:
                        raise ValueError(f"process {pid} identity changed")
                    identities[pid] = identity
                    delta = process_b["cpu_ns"] - process_a["cpu_ns"]
                    if delta < 0:
                        raise ValueError(f"process {pid} CPU counter decreased")
                    deltas[pid] = delta
                fraction = (right - left) / (b - a)
                cpu_ns = {label: sum(deltas[pid] for pid in pids) * fraction
                          for label, pids in groups.items()}
                host_delta = last["host_cpu_ns"] - first["host_cpu_ns"]
                if host_delta < 0:
                    raise ValueError("host CPU counter decreased")
                cpu_ns["Host CPU"] = host_delta * fraction
                cpu_ns["External CPU"] = cpu_ns["Host CPU"] - cpu_ns["Owned CPU"]
                if cpu_ns["External CPU"] < 0:
                    result["warnings"].append(f"interval {left}..{right}: negative external CPU estimate; counter quantization or sampling skew")
                percentages = {label: delta / (right - left) * 100
                               for label, delta in cpu_ns.items()}
                if logical_cpus:
                    percentages["External CPU, machine"] = percentages["External CPU"] / logical_cpus
                interval.update(cpu_ns=cpu_ns, cpu_pct=percentages,
                                process_cpu_ns={pid: delta * fraction for pid, delta in deltas.items()})
                for label, value in percentages.items():
                    values[label].append(value)
                result["seconds"].append(interval["seconds"])
                result["coverage_ns"] += right - left
            except (KeyError, TypeError, ValueError) as error:
                interval["failure"] = str(error)
                failures.append(f"interval {left}..{right}: {error}")
        if result["coverage_ns"] != end - start:
            failures.append("CPU accounting does not cover the complete measurement window")
    except (OSError, KeyError, TypeError, ValueError) as error:
        failures.append(str(error))
    if result["intervals"] and all("cpu_ns" in interval for interval in result["intervals"]):
        host = sum(interval["cpu_ns"]["Host CPU"] for interval in result["intervals"])
        owned = sum(interval["cpu_ns"]["Owned CPU"] for interval in result["intervals"])
        tolerance = max((end-start) * 0.05, max(host, owned) * 0.05)
        if owned-host > tolerance:
            failures.append("cumulative owned CPU materially exceeds host CPU accounting")
    result["valid"] = not failures
    if failures:
        result["values"] = {label: [] for label in values}
        result["seconds"] = []
    return result


def _latency(run, start, end):
    values = defaultdict(list)
    values["Response"] = []
    outcomes = {"completed": 0, "missing": 0, "timeouts": 0, "offered": 0,
                "missing_observations": 0, "rejected": 0, "interrupted": 0,
                "by_outcome": defaultdict(int)}
    for sample in run.get("samples", []):
        intended, injected = sample["intended_ns"], sample["injected_ns"]
        values["Scheduling lag"].append((injected - intended) / 1e6)
        observed = sample["observed_ns"]
        outcomes["offered"] += 1
        outcome = sample.get("outcome", "unknown")
        outcomes["by_outcome"][outcome] += 1
        outcomes["timeouts"] += int(outcome == "timeout")
        outcomes["rejected"] += int(outcome.startswith("rejected"))
        outcomes["interrupted"] += int(outcome.startswith("interrupted"))
        for label, first_field, last_field in (
            ("Harness input completion", "injected_ns", "accepted_ns"),
            ("Injected-to-helper", "injected_ns", "helper_received_ns"),
            ("Helper queue", "helper_received_ns", "process_start_ns"),
            ("Helper processing", "process_start_ns", "process_end_ns"),
            ("Action phase-to-injected", "phase_observed_ns", "injected_ns")):
            first, last = sample.get(first_field), sample.get(last_field)
            if first is not None and last is not None:
                values[label].append((last-first)/1e6)
        outcomes["missing_observations"] += sum(value is None for value in observed)
        if not observed or observed[0] is None:
            outcomes["missing"] += 1
        else:
            outcomes["completed"] += 1
        for client, value in enumerate(observed):
            label = "" if client == 0 else f", client {client + 1}"
            if value is None:
                continue
            if value < injected:
                raise ValueError("observation precedes input injection")
            values["Response" + label].append((value - injected) / 1e6)
            values["Intended-to-observed" + label].append((value - intended) / 1e6)
        if observed and all(value is not None for value in observed):
            values["Fanout spread"].append((max(observed) - min(observed)) / 1e6)
    for leg in run.get("echo_legs") or []:
        for label, field in (("Echo forward", "forward_ns"), ("Echo helper", "helper_ns")):
            if leg.get(field) is not None:
                values[label].append(leg[field] / 1e6)
        for client, value in enumerate(leg.get("return_ns") or []):
            if value is not None:
                suffix = "" if client == 0 else f", client {client + 1}"
                values["Echo return" + suffix].append(value / 1e6)
    transport = run.get("transport") or {}
    values["Matched socket RTT"].extend(value / 1e6 for value in transport.get("values_ns", []))
    events = run.get("load_events") or []
    written = {(str(event["pane"]).rsplit("p", 1)[-1], event["generation"]): event
               for event in events}
    load = [event for event in events if start <= event["start_ns"] <= end]
    scalars = {"load_bytes_per_second": sum(event.get("bytes", 0) for event in load) / ((end-start) / 1e9),
               "load_writes_per_second": len(load) / ((end-start) / 1e9)}
    age_area, covered, above20 = 0.0, 0, 0
    for client, observations in enumerate(run.get("presented_load") or []):
        by_pane = defaultdict(list)
        label = "Visible producer freshness" + (f", client {client + 1}" if client else "")
        for pane, generation, observed in observations:
            event = written.get((str(pane), generation))
            if event is None or observed < event["start_ns"]:
                continue
            source = event["start_ns"]
            if start <= source <= end:
                values[label].append((observed-source) / 1e6)
            by_pane[str(pane)].append((observed, source))
        for points in by_pane.values():
            points.sort()
            for index, (observed, source) in enumerate(points):
                next_observed = points[index+1][0] if index+1 < len(points) else end
                left, right = max(start, observed), min(end, next_observed)
                if right <= left:
                    continue
                age_area += ((left-source) + (right-source)) / 2 * (right-left)
                covered += right-left
                above20 += max(0, right-max(left, source+20_000_000))
    scalars.update(time_weighted_visible_age_ms=age_area / covered / 1e6 if covered else None,
                   visible_time_above20ms_percent=above20 / covered * 100 if covered else None,
                   visible_age_covered_ns=covered, visible_age_area_ns2=age_area,
                   visible_time_above20ms_ns=above20,
                   socket_missing=(transport.get("summary") or {}).get("missing", 0),
                   socket_unavailable=transport.get("unavailable"))
    return values, outcomes, scalars


def _summary(values, seconds=None):
    result = distribution(values)
    if seconds is not None:
        result["time_weighted_mean"] = (sum(value * duration for value, duration in zip(values, seconds))
                                        / sum(seconds) if seconds else None)
    else:
        result["thresholds"] = {str(limit): sum(value > limit for value in values) for limit in THRESHOLDS}
    return result


def _read_run(root, path, logical_cpus):
    result = json.loads(path.read_text())
    row = {"result_file": path.relative_to(root).as_posix(), "result": result,
           "job": result["job"], "version": result["variant"], "repeat": result["repeat"],
           "failures": []}
    if row["version"] not in ("before", "after") or type(row["repeat"]) is not int:
        raise ValueError("result requires before/after variant and integer repeat")
    run = json.loads((root / result["samples_file"]).read_text())
    start, end = run["measurement_first_intended_ns"], run["measurement_last_intended_ns"]
    if end <= start:
        raise ValueError("measurement window must have positive duration")
    row.update(measurement_start_ns=start, measurement_end_ns=end, measurement_seconds=(end-start)/1e9)
    if row["job"] == "idle":
        values, outcomes, scalars = {}, {}, {}
    else:
        values, outcomes, scalars = _latency(run, start, end)
    row.update(scalars)
    row["outcomes"] = outcomes
    for field in ("terminal_bytes_during_measurement", "terminal_bytes_whole_run", "idle_seconds"):
        row[field] = run.get(field, result.get(field))
    if row["terminal_bytes_during_measurement"] is not None:
        row["terminal_bytes_per_second"] = row["terminal_bytes_during_measurement"] / row["measurement_seconds"]
    if row["terminal_bytes_whole_run"] is None and run.get("observer_cost") is not None:
        row["terminal_bytes_whole_run"] = sum(observer.get("bytes", 0) for observer in run["observer_cost"])
    clients = run["client_pids"]
    cpu = cpu_intervals(path.parent / "resources.jsonl", start, end, run["server_pid"], clients, logical_cpus)
    row["cpu"] = {key: value for key, value in cpu.items() if key != "values"}
    row["failures"].extend(cpu["failures"])
    row["distributions"] = {label: _summary(data) for label, data in values.items()}
    for label, data in cpu["values"].items():
        row["distributions"][label] = _summary(data, cpu["seconds"])
        values[label] = data
    if "Response" in row["distributions"]:
        row["distributions"]["Response"].update(outcomes)
        row["response"] = row["distributions"]["Response"]
    cleanup = result.get("cleanup")
    if cleanup is False or isinstance(cleanup, dict) and any(value is False for value in cleanup.values()):
        row["failures"].append("cleanup failed")
    return row, values, cpu["seconds"]


def _verify_capture(root, path, result, manifest):
    for relative, expected in result.get("evidence_sha256", {}).items():
        digest = hashlib.sha256((root / relative).read_bytes()).hexdigest()
        if digest != expected:
            raise ValueError(f"raw evidence hash mismatch: {relative}")
    if manifest.get("files") and not result.get("evidence_sha256"):
        raise ValueError("completed capture lacks evidence hashes")
    sample = json.loads((root / result["samples_file"]).read_text())
    expected = manifest.get("files", {}).get(manifest.get("paths", {}).get(result["variant"]))
    if expected and sample.get("binary_sha256") != expected:
        raise ValueError("capture binary differs from frozen manifest")
    if result["job"] == "idle":
        if sample.get("terminal_bytes_during_measurement") != 0:
            raise ValueError("idle capture emitted terminal bytes")
    else:
        jobs = {job["name"]: job for job in manifest.get("jobs", []) if isinstance(job, dict)}
        expected_count = jobs.get(result["job"], {}).get("samples")
        if expected_count is not None and len(sample.get("samples", [])) != expected_count:
            raise ValueError("capture effect count differs from planned workload")
        if any(effect.get("outcome") != "presented" or not effect.get("observed_ns")
               or any(value is None for value in effect["observed_ns"])
               for effect in sample.get("samples", [])):
            raise ValueError("capture contains rejected, interrupted, or missing effects")


def _spread(values):
    return {"median": median(values) if values else None,
            "range": [min(values), max(values)] if values else None, "count": len(values)}


def _comparison(job, measure, statistic, before, after, per_side, unit, counts):
    paired = sorted(set(per_side["before"]) & set(per_side["after"]))
    increases = sum(per_side["after"][repeat] > per_side["before"][repeat] for repeat in paired)
    if statistic == "p99.9" and not paired:
        assessment = "Pooled tail only; per-run N below 10,000"
    elif paired and increases == len(paired):
        assessment = "Higher in all paired repeats; descriptive"
    elif paired:
        assessment = "Varies across paired repeats; inconclusive"
    else:
        assessment = "No matched repeats; descriptive pooled comparison only"
    if measure in ("Scheduling lag", "Echo helper", "Matched socket RTT"):
        assessment += "; generator/helper/transport diagnostic"
    if "freshness" in measure:
        assessment += "; observed generations correlated"
    row = {"job": job, "measure": measure, "statistic": statistic, "before": before,
           "after": after, "delta": after-before,
           "relative_percent": (after/before-1)*100 if before else None, "unit": unit,
           "before_count": counts[0], "after_count": counts[1], "pairs_up": increases,
           "valid_pairs": len(paired), "assessment": assessment}
    for side in ("before", "after"):
        spread = _spread(list(per_side[side].values()))
        row[f"{side}_run_median"], row[f"{side}_run_range"] = spread["median"], spread["range"]
        row[f"{side}_run_count"] = spread["count"]
    return row


def _comparisons(distributions, runs):
    pairs = defaultdict(dict)
    for row in distributions:
        pairs[row["job"], row["measure"]][row["version"]] = row
    changes = []
    for (job, measure), sides in sorted(pairs.items()):
        if not all(side in sides for side in ("before", "after")):
            continue
        before, after = sides["before"], sides["after"]
        statistics = (("time_weighted_mean",) + STATISTICS[1:] if "CPU" in measure else STATISTICS)
        for statistic in statistics:
            a, b = before.get(statistic), after.get(statistic)
            if a is None or b is None:
                continue
            per_side = {side: {row["repeat"]: row["distributions"][measure][statistic]
                              for row in runs if row["job"] == job and row["version"] == side
                              and row.get("distributions", {}).get(measure, {}).get(statistic) is not None}
                        for side in ("before", "after")}
            changes.append(_comparison(job, measure, statistic, a, b, per_side, before["unit"],
                                       (before["count"], after["count"])))
    for job in sorted({row["job"] for row in runs}):
        for measure, field, weight, unit in (
            ("Time-weighted visible age", "time_weighted_visible_age_ms", "visible_age_covered_ns", "ms"),
            ("Visible time above 20 ms", "visible_time_above20ms_percent", "visible_age_covered_ns", "%"),
            ("Offered traffic", "load_bytes_per_second", "measurement_seconds", "bytes/s"),
            ("Offered writes", "load_writes_per_second", "measurement_seconds", "writes/s"),
            ("Terminal traffic, measurement", "terminal_bytes_during_measurement", None, "bytes/run"),
            ("Terminal traffic rate, measurement", "terminal_bytes_per_second", "measurement_seconds", "bytes/s"),
            ("Terminal traffic, whole run", "terminal_bytes_whole_run", None, "bytes/run")):
            selected = {side: [row for row in runs if row["job"] == job and row["version"] == side
                               and row.get(field) is not None and (not weight or row.get(weight, 0) > 0)]
                        for side in ("before", "after")}
            if not all(selected.values()):
                continue
            pooled = {side: (sum(row[field]*row[weight] for row in data) / sum(row[weight] for row in data)
                            if weight else mean(row[field] for row in data))
                      for side, data in selected.items()}
            per_side = {side: {row["repeat"]: row[field] for row in data} for side, data in selected.items()}
            changes.append(_comparison(job, measure, "time_weighted_mean" if weight else "run_mean",
                                       pooled["before"], pooled["after"], per_side, unit,
                                       (len(selected["before"]), len(selected["after"]))))
    return changes


def _format(value):
    return "n/a" if value is None else f"{value:.6f}" if isinstance(value, float) else str(value)


def _table(headers, rows):
    return ["| " + " | ".join(headers) + " |", "| " + " | ".join("---" for _ in headers) + " |",
            *("| " + " | ".join(_format(value).replace("|", "\\|") for value in row) + " |" for row in rows)]


def _write_reports(root, summary, changes):
    distributions, runs = summary["distributions"], summary["runs"]
    refs = summary["manifest"].get("refs", {})
    lines = ["# Remeasure report", "", f"Platform `{summary['manifest'].get('platform', 'unspecified')}`.", "",
             f"Before `{refs.get('before', 'unspecified')}`. After `{refs.get('after', 'unspecified')}`.", "",
             "Smoke runs remain raw artifacts and supply no reported statistics.", "",
             "Percentiles pool raw nearest-rank observations. p99.9 requires at least 10,000 observations.", "",
             "CPU counters use nanoseconds and report percent of one core. CPU means weight clipped interval duration.", "",
             "A failed CPU capture supplies no pooled CPU observations. The run retains intervals and failure reasons.", ""]
    lines += _table(["Workload", "Variant", "Measure", "Unit", "N", "Mean", "p50", "p90", "p95",
                     "p97", "p98", "p99", "p99.9", "Max", "Weighted mean"],
                    [[row["job"], row["version"], row["measure"], row["unit"], row["count"],
                      *(row.get(stat) for stat in STATISTICS), row.get("time_weighted_mean")]
                     for row in distributions])
    lines += ["", "## Latency outcomes", ""]
    lines += _table(["Workload", "Variant", "Offered", "Completed", "Missing", "Timeouts", "Rejected", "Interrupted",
                     *(f">{limit} ms" for limit in THRESHOLDS)],
                    [[row["job"], row["version"], row["offered"], row["completed"], row["missing"], row["timeouts"], row["rejected"], row["interrupted"],
                      *(row["thresholds"][str(limit)] for limit in THRESHOLDS)]
                     for row in distributions if row["measure"] == "Response"])
    lines += ["", "## Run observations", ""]
    lines += _table(["Workload", "Variant/repeat", "Seconds", "Response p95, ms", "CPU coverage, ns",
                     "Visible age, ms", "Visible time >20 ms, %", "Offered bytes/s", "Terminal measured bytes",
                     "Terminal whole-run bytes", "Failures"],
                    [[row["job"], f"{row['version']}/r{row['repeat']}", row.get("measurement_seconds"),
                      row.get("response", {}).get("p95"), row.get("cpu", {}).get("coverage_ns"),
                      row.get("time_weighted_visible_age_ms"), row.get("visible_time_above20ms_percent"),
                      row.get("load_bytes_per_second"), row.get("terminal_bytes_during_measurement"),
                      row.get("terminal_bytes_whole_run"), "; ".join(row["failures"]) or "none"] for row in runs])
    lines += ["", "## Failed-capture outcomes", ""]
    lines += _table(["Workload", "Variant/repeat", "Offered", "Completed", "Missing", "Timeouts", "Rejected", "Interrupted"],
                    [[row["job"], f"{row['version']}/r{row['repeat']}",
                      *(row.get("outcomes", {}).get(field) for field in ("offered", "completed", "missing", "timeouts", "rejected", "interrupted"))]
                     for row in runs if row["failures"]])
    lines += ["", "## Measurement limits", "",
              "Observed generations share frames and can coalesce. Visible-age means weight covered pane time.", "",
              "Echo return overlaps helper duration. Socket RTT excludes presentation and remains a transport diagnostic.", "",
              "CPU interval percentiles describe burstiness. Counter resolution depends on the capture platform.", "",
              "Negative external CPU interval estimates retain counter quantization and sampling skew; they are not clamped.", "",
              "Whole-run terminal traffic includes setup and warmups. It does not measure traffic within the latency window.", "",
              "Paired repeats expose variation. Positive deltas establish neither causality nor statistical significance."]
    for limit in summary["manifest"].get("limits", []):
        lines += ["", str(limit)]
    if summary["failures"]:
        lines += ["", "## Capture failures", "", *(f"- {failure}" for failure in summary["failures"])]
    (root / "report.md").write_text("\n".join(lines) + "\n")
    upward = [row for row in changes if row["delta"] > 0]
    refs = summary["manifest"].get("refs", {})
    lines = ["# Descriptive observed increases", "", f"Before `{refs.get('before', 'unspecified')}`. After `{refs.get('after', 'unspecified')}`.", "",
             "Positive deltas report observations. Pair counts and per-run ranges expose variation.", "",
             "No significance or causal claim follows from these comparisons.", ""]
    lines += _table(["Workload", "Measure", "Statistic", "Before", "After", "Delta", "Relative %", "Unit",
                     "Pairs up", "Assessment"],
                    [[row["job"], row["measure"], row["statistic"], row["before"], row["after"], row["delta"],
                      row["relative_percent"], row["unit"], f"{row['pairs_up']}/{row['valid_pairs']}", row["assessment"]]
                     for row in upward])
    lines += ["", "## Per-run variation", ""]
    lines += _table(["Workload", "Measure", "Statistic", "Before median", "Before range", "After median", "After range"],
                    [[row["job"], row["measure"], row["statistic"], row["before_run_median"], row["before_run_range"],
                      row["after_run_median"], row["after_run_range"]] for row in upward])
    lines += ["", "[All distributions and failures](report.md). [Run details](summary.json).",
              "[All comparisons](comparisons.json). [Frozen inputs](manifest.json).", "",
              f"Excluded {len(summary['excluded_smoke'])} smoke captures. Capture failures: {len(summary['failures'])}."]
    (root / "regressions.md").write_text("\n".join(lines) + "\n")
    columns = ["job", "measure", "statistic", "before", "after", "delta", "relative_percent", "unit",
               "before_count", "after_count", "pairs_up", "valid_pairs", "before_run_median", "before_run_range",
               "after_run_median", "after_run_range", "assessment"]
    with (root / "regressions.csv").open("w", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=columns, extrasaction="ignore")
        writer.writeheader()
        writer.writerows(upward)


def report(root: Path):
    """Write reproducible reports beside manifest.json and return the summary."""
    root = Path(root).resolve()
    manifest = json.loads((root / "manifest.json").read_text())
    for relative, expected in manifest.get("files", {}).items():
        if hashlib.sha256((root / relative).read_bytes()).hexdigest() != expected:
            raise ValueError(f"frozen binary/package hash mismatch: {relative}")
    logical_cpus = manifest.get("logical_cpus", manifest.get("host_cpu_count"))
    pooled, weights = defaultdict(list), defaultdict(list)
    runs, failures, seen = [], [], set()
    excluded_smoke = [path.relative_to(root).as_posix() for path in sorted((root / "smoke").glob("*/*/result.json"))]
    paths = sorted((root / "runs").glob("*/*/result.json")) + sorted((root / "idle").glob("*/result.json"))
    failed_paths = sorted((root / "runs").glob("*/*/failure.json")) + sorted((root / "idle").glob("*/failure.json"))
    for path in failed_paths:
        if (path.parent / "result.json").exists():
            continue
        failure = json.loads(path.read_text())
        side_repeat = path.parent.name.rsplit("-r", 1)
        version, repeat = (side_repeat[0], int(side_repeat[1])) if len(side_repeat) == 2 and side_repeat[1].isdigit() else ("unknown", "unknown")
        job = "idle" if path.parent.parent.name == "idle" else path.parent.parent.name
        samples = list(path.parent.glob("*/samples.json"))
        failure_result = {"job": job, "variant": version, "repeat": repeat, "failure": failure}
        if len(samples) == 1:
            failure_result["samples_file"] = samples[0].relative_to(root).as_posix()
        row = {"job": job, "version": version, "repeat": repeat, "result": failure_result,
               "result_file": path.relative_to(root).as_posix(), "failures": [failure.get("error", "capture failed")]}
        if len(samples) == 1:
            try:
                sample_data = json.loads(samples[0].read_text())
                start, end = sample_data["measurement_first_intended_ns"], sample_data["measurement_last_intended_ns"]
                _, outcomes, _ = _latency(sample_data, start, end) if job != "idle" else ({}, {}, {})
                row["outcomes"] = outcomes
                row["measurement_seconds"] = (end-start)/1e9
                row["cpu"] = cpu_intervals(path.parent/"resources.jsonl", start, end,
                                            sample_data["server_pid"], sample_data["client_pids"], logical_cpus)
                row["failures"].extend(row["cpu"]["failures"])
            except (OSError, KeyError, TypeError, ValueError) as error:
                row["failures"].append(f"raw failure evidence: {error}")
        runs.append(row)
        failures.extend(f"{path.relative_to(root).as_posix()}: {error}" for error in row["failures"])
    for path in paths:
        result = {}
        try:
            result = json.loads(path.read_text())
            if result.get("repeat") == "smoke":
                excluded_smoke.append(path.relative_to(root).as_posix())
                continue
            identity = (result["job"], result["variant"], result["repeat"])
            if identity in seen:
                raise ValueError(f"duplicate workload/variant/repeat {identity}")
            seen.add(identity)
            _verify_capture(root, path, result, manifest)
            row, values, seconds = _read_run(root, path, logical_cpus)
            runs.append(row)
            failures.extend(f"{row['result_file']}: {error}" for error in row["failures"])
            for label, data in values.items():
                key = row["job"], row["version"], label
                pooled[key].extend(data)
                if "CPU" in label:
                    weights[key].extend(seconds)
        except (OSError, KeyError, TypeError, ValueError) as error:
            failures.append(f"{path.relative_to(root).as_posix()}: {error}")
            runs.append({"job": result.get("job", path.parent.parent.name),
                         "version": result.get("variant", "unknown"),
                         "repeat": result.get("repeat", "unknown"), "result": result,
                         "result_file": path.relative_to(root).as_posix(), "failures": [str(error)]})
    jobs = manifest.get("jobs") or []
    repetitions = manifest.get("repetitions")
    if type(repetitions) is int:
        names = {job["name"] if isinstance(job, dict) else job for job in jobs} | {"idle"}
        for job in sorted(names):
            for version in ("before", "after"):
                for repeat in range(1, repetitions+1):
                    if (job, version, repeat) not in seen:
                        failures.append(f"missing successful capture: {job}/{version}-r{repeat}")
    distributions = []
    for (job, version, label), data in sorted(pooled.items()):
        cpu = "CPU" in label
        row = {"job": job, "version": version, "measure": label,
               "unit": "% machine" if label == "External CPU, machine" else "% of one core" if cpu else "ms",
               **_summary(data, weights[job, version, label] if cpu else None)}
        if label == "Response":
            for field in ("completed", "missing", "timeouts", "rejected", "interrupted", "offered", "missing_observations"):
                row[field] = sum(run.get("outcomes", {}).get(field, 0) for run in runs
                                 if run["job"] == job and run["version"] == version)
        if label == "Matched socket RTT":
            matching = [run for run in runs if run["job"] == job and run["version"] == version]
            row["missing"] = sum(run.get("socket_missing", 0) for run in matching)
            row["unavailable_runs"] = sum(run.get("socket_unavailable") is not None for run in matching)
        row["run_statistics"] = {}
        for statistic in (*STATISTICS, "time_weighted_mean"):
            run_values = [run["distributions"][label][statistic] for run in runs
                          if run["job"] == job and run["version"] == version
                          and run.get("distributions", {}).get(label, {}).get(statistic) is not None]
            row["run_statistics"][statistic] = _spread(run_values)
        if cpu:
            row["seconds"] = sum(weights[job, version, label])
        distributions.append(row)
    summary = {"manifest": manifest, "method": METHOD, "runs": runs, "distributions": distributions,
               "failures": failures, "excluded_smoke": excluded_smoke}
    changes = _comparisons(distributions, runs)
    for name, artifact in (("summary.json", summary), ("comparisons.json", changes)):
        (root / name).write_text(json.dumps(artifact, indent=2, allow_nan=False) + "\n")
    _write_reports(root, summary, changes)
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="Capture root containing manifest.json")
    args = parser.parse_args()
    summary = report(args.output)
    print(f"Reported {len(summary['runs'])} captures; {len(summary['failures'])} failures; smoke excluded")
    return 1 if summary["failures"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
