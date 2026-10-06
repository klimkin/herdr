"""Summarize benchmark records and pair transport stages on one clock domain."""

import argparse
from collections import defaultdict, deque
import json
import math
from pathlib import Path


def pair_stages(records):
    pending = defaultdict(deque)
    exact = {}
    pairs = []
    for record in sorted(records, key=lambda item: item["ns"]):
        # Scope identifies one queue; legacy records cannot prove this pairing.
        if not record.get("scope"):
            continue
        key = (record["pid"], record["scope"], record["id"])
        occurrence = record.get("occurrence", 0)
        exact_key = (record["pid"], record.get("connection", 0), occurrence)
        if record["stage"] == "server.queue_discard":
            for pending_key in list(pending):
                if pending_key[:2] == key[:2]:
                    del pending[pending_key]
            exact = {item_key: item for item_key, item in exact.items()
                     if item is not None and (item["pid"], item["scope"]) != key[:2]}
        elif record["stage"] == "server.enqueue":
            if occurrence:
                # A duplicate or missing occurrence never falls back to ordinal pairing.
                exact[exact_key] = record if exact_key not in exact else None
            else:
                pending[key].append(record)
        elif record["stage"] == "server.writer_claim":
            enqueued = exact.pop(exact_key, None) if occurrence else pending[key].popleft() if pending[key] else None
            if enqueued is None or enqueued["id"] != record["id"] or enqueued["scope"] != record["scope"]:
                continue
            pairs.append({"pid": record["pid"], "scope": record["scope"], "frame_fingerprint": record["id"],
                          "connection":record.get("connection",0),"occurrence":occurrence,"offset":record.get("offset",0),
                          "enqueue_ns": enqueued["ns"], "claim_ns": record["ns"],
                          "queue_ns": record["ns"] - enqueued["ns"]})
    return pairs


def input_actor_path(records, identity, pid, start, end):
    """Pair accepted actor input parts without inferring missing boundaries."""
    selected = [record for record in records
                if record['pid'] == pid and start <= record['ns'] <= end]
    fragments = [record for record in selected
                 if record['stage'] == 'input.actor_fragment' and record['id'] == identity]
    if not fragments:
        return {'attribution': 'actor records unavailable', 'parts': []}
    scopes = {record.get('scope') for record in fragments}
    if len(scopes) != 1 or not next(iter(scopes)):
        return {'attribution': 'ambiguous actor identity', 'parts': []}
    scope = next(iter(scopes))
    selected = [record for record in selected if record.get('scope') == scope]
    part_ids = sorted({record['value'] for record in fragments})
    parts = []
    def unique(stage, identifier):
        matches = [record for record in selected
                   if record['stage'] == stage and record['id'] == identifier]
        return matches[0] if len(matches) == 1 else None
    def interval(left, right):
        return right-left if left is not None and right is not None and right >= left else None
    for part_id in part_ids:
        link = unique('input.actor_part', part_id)
        command_id = link['value'] if link else None
        enqueue = unique('input.actor_enqueue', command_id)
        claim = unique('input.actor_claim', command_id)
        pending = unique('input.pty_pending', part_id)
        attempt = unique('input.pty_write_attempt', part_id)
        complete = unique('input.pty_part_complete', part_id)
        discarded = any(record['stage'] == 'input.pty_part_discard' and record['id'] == part_id
                        or record['stage'] == 'input.actor_discard' and record['id'] == command_id
                        for record in selected)
        timestamps = {name: record['ns'] if record else None
                      for name, record in [('enqueue_ns', enqueue), ('claim_ns', claim),
                                           ('pending_ns', pending), ('attempt_ns', attempt),
                                           ('complete_ns', None if discarded else complete)]}
        parts.append({'part_id': part_id, 'command_id': command_id, 'scope': scope,
                      'discarded': discarded, **timestamps,
                      'command_queue_ns': interval(timestamps['enqueue_ns'], timestamps['claim_ns']),
                      'claim_to_pending_ns': interval(timestamps['claim_ns'], timestamps['pending_ns']),
                      'pending_to_attempt_ns': interval(timestamps['pending_ns'], timestamps['attempt_ns']),
                      'attempt_to_complete_ns': interval(timestamps['attempt_ns'], timestamps['complete_ns'])})
    complete = all(not part['discarded'] and all(part[name] is not None for name in
                   ['command_queue_ns', 'claim_to_pending_ns', 'pending_to_attempt_ns', 'attempt_to_complete_ns'])
                   for part in parts)
    return {'attribution': 'complete contributing accepted parts' if complete else 'incomplete contributing parts',
            'parts': parts, 'first_enqueue_ns': min(part['enqueue_ns'] for part in parts) if complete else None,
            'final_write_complete_ns': max(part['complete_ns'] for part in parts) if complete else None,
            'limits': ['elapsed boundaries do not establish CPU or off-CPU cause',
                       'fragment intervals may overlap; do not add them',
                       'trace loss may omit contributing identity links']}


def mapped_connection(records, queue, client_pid, trace_audit=None):
    """Native peer PIDs prove only a unique captured connection per PID pair.

    Reconnects and multiple sockets from one client process remain unassigned.
    The proof does not use receipt order, content alone, or timestamp proximity.
    """
    for pid in (queue["pid"], client_pid):
        if trace_audit is not None:
            process = next((item for item in trace_audit.get("processes", []) if item["pid"] == pid), None)
            if process is None or not process.get("complete"):
                return None
        else:
            process_records = [item for item in records if item["pid"] == pid]
            finished = [item for item in process_records if item["stage"] == "process.finish"]
            if len(finished) != 1 or any(item.get("dropped", 0) for item in process_records):
                return None
            if any(item["ns"] > finished[0]["ns"] for item in process_records):
                return None
    servers = [item for item in records if item["stage"] == "server.connection"
               and item["pid"] == queue["pid"] and item["value"] == client_pid]
    clients = [item for item in records if item["stage"] == "client.connection"
               and item["pid"] == client_pid and item["value"] == queue["pid"]]
    if len(servers) == len(clients) == 1 and servers[0]["id"] == queue["connection"]:
        return clients[0]["id"]
    return None


def delivery_join(records, queues, queue, output, trace_audit=None):
    """Require retained occurrence + framed byte offset for modern traces."""
    if trace_audit is not None:
        for pid in (queue["pid"], output["pid"]):
            process = next((item for item in trace_audit.get("processes", []) if item["pid"] == pid), None)
            if process is None or not process.get("complete"):
                return None
    wire = queue["frame_fingerprint"]
    if queue.get("occurrence"):
        connection = mapped_connection(records, queue, output["pid"], trace_audit)
        if connection is None or output.get("connection") != connection or output.get("offset") != queue["offset"]:
            return None
        receive = [item for item in records if item["stage"] == "transport.receive"
                   and item["pid"] == output["pid"] and item["id"] == wire
                   and item.get("connection") == connection and item.get("offset") == queue["offset"]
                   and queue["claim_ns"] <= item["ns"] <= output["ns"]]
        def written(stage):
            return [item for item in records if item["stage"] == stage and item["pid"] == queue["pid"]
                    and item["id"] == wire and item.get("scope") == queue["scope"]
                    and item.get("connection") == queue["connection"] and item.get("occurrence") == queue["occurrence"]
                    and item.get("offset") == queue["offset"]
                    and queue["claim_ns"] <= item["ns"] <= output["ns"]]
        starts, completes = written("server.write_start"), written("server.write_complete")
        if len(receive) != 1 or len(starts) != 1 or len(completes) != 1:
            return None
        return receive[0]["ns"], starts[0]["ns"], completes[0]["ns"], "native peer PID and framed-byte offset"
    if output.get("connection"):
        return None
    candidates = [pair for pair in queues if pair["pid"] == queue["pid"]
                  and pair["frame_fingerprint"] == wire and pair["claim_ns"] <= output["ns"]]
    if len(candidates) != 1:
        return None
    receive = next((item["ns"] for item in records if item["stage"] == "transport.receive"
                    and item["pid"] == output["pid"] and item["id"] == wire
                    and queue["claim_ns"] <= item["ns"] <= output["ns"]), None)
    def written(stage):
        return next((item["ns"] for item in records if item["stage"] == stage and item["pid"] == queue["pid"]
                     and item["id"] == wire and item.get("scope") == queue["scope"]
                     and queue["claim_ns"] <= item["ns"] <= output["ns"]), None)
    return receive, written("server.write_start"), written("server.write_complete"), "legacy unique fingerprint"


def fingerprint(text):
    value = 0xcbf29ce484222325
    for byte in text.encode():
        value = ((value ^ byte) * 0x100000001b3) & ((1 << 64) - 1)
    return value


def critical_paths(run, records, trace_audit=None):
    """Join diagnostic links, refusing absent or ambiguous causal attribution."""
    stages = defaultdict(list)
    for record in sorted(records, key=lambda item: item["ns"]):
        stages[record["stage"]].append(record)
    queues = pair_stages(records)
    def gate_window(pid, ready, surface):
        selected = [item for item in stages["presentation.selected_overdue"] if item["pid"]==pid and ready<=item["ns"]<=surface]
        starts = [item for item in stages["server.frame_start"] if item["pid"]==pid and ready<=item["ns"]<=surface]
        if not selected or not starts:
            return None
        gate = selected[-1]
        frame = starts[-1]
        remaining = next((item["value"] for item in stages["presentation.selected_remaining"] if item["pid"]==pid and abs(item["ns"]-gate["ns"])<1_000_000),0)
        absolute = next((item["value"] for item in stages["presentation.selected_deadline"] if item["pid"]==pid and item["ns"]==gate["ns"]),None)
        cadence_floor = absolute if absolute is not None else gate["ns"]-gate["value"] if gate["value"] else gate["ns"]+remaining
        eligible = max(ready, cadence_floor)
        return {"frame_start_ns":frame["ns"],"cadence_floor_ns":cadence_floor,"eligible_ns":eligible,"state_ready_to_eligible_ns":max(0,eligible-ready),"eligible_to_frame_start_ns":max(0,frame["ns"]-eligible),"attribution":"selected presentation opportunity, host clock"}

    paths = []
    for sample in run["samples"]:
        identity = int(sample["identity"], 16)
        end = max((value for value in sample["observed_ns"] if value is not None), default=0)
        start = sample.get("process_start_ns") if run["path"] == "output" else sample["injected_ns"]
        if start is None or end <= start:
            continue
        stimuli = [item for item in stages["terminal.stimulus"]
                   if item["id"] == identity and start <= item["ns"] <= end]
        links = []
        if run["path"] == "action":
            # Action identities encode sequence, while authoritative labels use decimal.
            label = fingerprint(f"A{identity:06d}")
            links = [(None, item) for item in stages["snapshot.label"]
                     if item["id"] == label and start <= item["ns"] <= end]
        else:
            for stimulus in stimuli:
                for surface in stages["surface.content"]:
                    if (surface["pid"] == stimulus["pid"] and surface["id"] == next((item["value"] for item in stages["pane.identity"] if item["pid"]==stimulus["pid"] and item["id"]==stimulus["scope"]),stimulus["scope"])
                            and surface["value"] >= stimulus["value"] and stimulus["ns"] <= surface["ns"] <= end):
                        # A replacement marker in this pane supersedes the older effect.
                        overwritten = any(other["pid"] == stimulus["pid"] and other["scope"] == stimulus["scope"]
                                          and other["id"] != stimulus["id"] and other["value"] >= stimulus["value"] and other["value"] <= surface["value"]
                                          for other in stages["terminal.stimulus"])
                        if not overwritten:
                            links.append((stimulus, surface))
        seen = set()
        for stimulus, surface in links:
            wire = surface["scope"]
            for queue in queues:
                if queue["pid"] != surface["pid"] or queue["frame_fingerprint"] != wire:
                    continue
                if not surface["ns"] <= queue["enqueue_ns"] <= end:
                    continue
                outputs = [item for item in stages["client.delivery"]
                           if item["id"] == wire and queue["claim_ns"] <= item["ns"] <= end]
                for output in outputs:
                    client_pids = run.get("client_pids", [])
                    client_index = client_pids.index(output["pid"]) if output["pid"] in client_pids else None
                    observed = sample["observed_ns"][client_index] if client_index is not None else None
                    if observed is None or output["ns"] > observed:
                        continue
                    joined = delivery_join(records, queues, queue, output, trace_audit)
                    if joined is None:
                        continue
                    receive, write_start, written, connection_attribution = joined
                    # One earliest proven delivered effect per sample and destination.
                    key = (sample["identity"], output["pid"])
                    if key in seen:
                        continue
                    seen.add(key)
                    action_commit = next((item for item in stages["server.action_committed"] if run["path"]=="action" and item["pid"]==queue["pid"] and item["value"]==label and start<=item["ns"]<=surface["ns"]),None)
                    state_ready = stimulus["ns"] if stimulus else action_commit["ns"] if action_commit else start
                    gate = gate_window(queue["pid"], state_ready, surface["ns"])
                    forward = {}
                    if run["path"]=="echo":
                        for stage in ("input.stdin_stimulus","input.client_frame","input.server_frame","input.server_dispatch","input.pty_write_complete"):
                            match = next((item for item in stages[stage] if item["id"]==identity and start<=item["ns"]<=surface["ns"]),None)
                            if match:
                                forward[stage]=match["ns"]
                                if stage=="input.client_frame":
                                    for boundary in ("client.input_enqueue","client.input_claim","client.input_write_complete"):
                                        candidates=[item for item in stages[boundary] if item["pid"]==match["pid"] and item["id"]==match["scope"] and start<=item["ns"]<=surface["ns"]]
                                        if len(candidates)==1: forward[boundary]=candidates[0]["ns"]
                    if action_commit:
                        link=next((item for item in stages["action.request_link"] if item["pid"]==queue["pid"] and item["value"]==action_commit["id"]),None)
                        if link:
                            for stage in ("client.action_enqueue","client.action_send","server.action_received","server.action_dispatched"):
                                event=next((item for item in stages[stage] if item["id"]==link["id"] and start<=item["ns"]<=surface["ns"]),None)
                                if event:forward[stage]=event["ns"]
                    paths.append({"identity":sample["identity"], "server_pid":queue["pid"],
                                  "client_pid":output["pid"], "queue_scope":queue["scope"],
                                  "frame_fingerprint":wire, "start_ns":start,
                                  "connection":queue.get("connection",0),"occurrence":queue.get("occurrence",0),"wire_offset":queue.get("offset",0),"connection_attribution":connection_attribution,
                                  "terminal_ready_ns":stimulus["ns"] if stimulus else None,
                                  "terminal_revision":stimulus["value"] if stimulus else None,
                                  "surface_content_revision":surface["value"], "surface_ns":surface["ns"],"presentation_gate":gate,"forward_boundaries":forward,"action_commit_ns":action_commit["ns"] if action_commit else None,
                                  "enqueue_ns":queue["enqueue_ns"],"claim_ns":queue["claim_ns"],
                                  "queue_ns":queue["queue_ns"],"socket_start_ns":write_start,"socket_complete_ns":written,
                                  "client_receive_ns":receive,"client_output_ns":output["ns"],
                                  "outer_observed_ns":observed,
                                  "unavailable":["CPU versus off-CPU", "PTY submission queue residence"] + ([] if gate else ["per-stimulus selected gate"]) + ([] if client_index is not None else ["client index mapping"])})
    return paths


def distribution(values, denominator):
    """Nearest-rank duration summary; incomplete effects keep their denominator."""
    ordered = sorted(values)
    def percentile(fraction):
        return ordered[math.ceil(len(ordered) * fraction) - 1] if ordered else None
    return {"denominator": denominator, "sample_count": len(ordered),
            "unassigned_count": denominator - len(ordered),
            "coverage_fraction": len(ordered) / denominator if denominator else None,
            "p50_ns": percentile(.5), "p95_ns": percentile(.95),
            "p99_ns": percentile(.99),
            "p999_ns": percentile(.999) if len(ordered) >= 10_000 else None,
            "max_ns": ordered[-1] if ordered else None}


# Pipeline intervals form a partition only when all boundaries are present and
# ordered. Socket-write completion can follow receive: report its overlapping
# duration separately, never as an additive pipeline component.
OUTPUT_INTERVALS = {
    "source_to_state_ready": ("start_ns", "state_ready_ns"),
    "presentation_gate": ("state_ready_ns", "eligible_ns"),
    "presentation_scheduling": ("eligible_ns", "frame_start_ns"),
    "frame_to_surface": ("frame_start_ns", "surface_ns"),
    "surface_to_enqueue": ("surface_ns", "enqueue_ns"),
    "server_writer_queue": ("enqueue_ns", "claim_ns"),
    "writer_claim_to_write_start": ("claim_ns", "socket_start_ns"),
    "write_start_to_client_receive": ("socket_start_ns", "client_receive_ns"),
    "client_work": ("client_receive_ns", "client_output_ns"),
    "outer_observer": ("client_output_ns", "outer_observed_ns"),
    "server_socket_write": ("socket_start_ns", "socket_complete_ns"),
}


def stage_distributions(run, paths):
    """One contribution per completed effect with a unique causal path."""
    completed = {}
    offered = 0
    client_counts = defaultdict(int)
    completed_count = 0
    for sample in run["samples"]:
        for index, observed in enumerate(sample["observed_ns"]):
            offered += 1
            start = sample.get("process_start_ns") if run["path"] == "output" else sample.get("injected_ns")
            if start is not None and observed is not None and observed >= start:
                completed_count += 1
                client_counts[index] += 1
                pids = run.get("client_pids", [])
                if index < len(pids):
                    completed[(sample["identity"], pids[index])] = (start, observed)
    candidates = defaultdict(list)
    for path in paths:
        key = (path["identity"], path["client_pid"])
        if key in completed:
            candidates[key].append(path)
    values = defaultdict(list)
    effects = []
    for key, options in candidates.items():
        if len(options) != 1:
            continue
        path = options[0]
        start, observed = completed[key]
        ready = path.get("terminal_ready_ns")
        boundaries = {**path, **(path.get("presentation_gate") or {}),
                      "state_ready_ns": ready if ready is not None else path.get("action_commit_ns")}
        intervals = {}
        partition = []
        for stage, (first, last) in OUTPUT_INTERVALS.items():
            before, after = boundaries.get(first), boundaries.get(last)
            if before is None or after is None or not start <= before <= after <= observed:
                continue
            elapsed = after - before
            values[stage].append(elapsed)
            intervals[stage] = {"start_ns": before, "end_ns": after, "duration_ns": elapsed}
            if stage != "server_socket_write":
                partition.append((before, after))
        accounted = 0
        cursor = start
        gaps = []
        for before, after in sorted(partition):
            if before > cursor:
                gaps.append({"start_ns": cursor, "end_ns": before, "duration_ns": before - cursor})
            accounted += max(0, after - max(cursor, before))
            cursor = max(cursor, after)
        if observed > cursor:
            gaps.append({"start_ns": cursor, "end_ns": observed, "duration_ns": observed - cursor})
        effects.append({"identity": key[0], "client_pid": key[1],
                        "intervals": intervals, "total_ns": observed - start,
                        "accounted_pipeline_ns": accounted,
                        "unassigned_pipeline_ns": observed - start - accounted,
                        "unassigned_intervals": gaps})
    stages = {}
    for name, endpoints in OUTPUT_INTERVALS.items():
        stages[name] = {**distribution(values[name], completed_count),
                        "boundaries": list(endpoints),
                        "accounting": "overlapping diagnostic" if name == "server_socket_write" else "pipeline interval",
                        "overlaps": ["write_start_to_client_receive", "client_work"] if name == "server_socket_write" else []}
    ambiguous = sum(len(options) > 1 for options in candidates.values())
    pids = run.get("client_pids", [])
    clients = [{"client": index, "client_pid": pids[index] if index < len(pids) else None,
                "completed_effects": client_counts[index],
                "uniquely_attributed_effects": sum(effect["client_pid"] == pids[index] for effect in effects) if index < len(pids) else 0,
                "stages": {name: distribution([effect["intervals"][name]["duration_ns"] for effect in effects
                                                if index < len(pids) and effect["client_pid"] == pids[index] and name in effect["intervals"]], client_counts[index])
                           for name in OUTPUT_INTERVALS}}
               for index in range(run.get("clients", len(pids)))]
    return {"coverage": {"offered_effects": offered, "completed_effects": completed_count,
                         "uniquely_attributed_effects": len(effects),
                         "unassigned_completed_effects": completed_count - len(effects),
                         "ambiguous_completed_effects": ambiguous},
            "stages": stages, "effects": effects, "clients": clients,
            "semantics": "Matched elapsed intervals include scheduling; stage percentiles are not additive. Socket write overlaps delivery and is excluded from pipeline accounting."}


def load_traces(run, directory):
    """Read usable records and audit flush/loss for every expected process."""
    expected = defaultdict(list)
    diagnostics = defaultdict(list)
    valid_pid = lambda pid: type(pid) is int and pid > 0
    if valid_pid(run.get("server_pid")):
        expected[run["server_pid"]].append("server")
        diagnostics[run["server_pid"]].append(directory.parent / "server.stderr")
    for index, pid in enumerate(run.get("client_pids", [])):
        if not valid_pid(pid):
            continue
        expected[pid].append(f"client {index}")
        diagnostics[pid].append(directory.parent / f"client-{index}.vt")
    paths = {path.name: path for path in directory.glob("*.jsonl")}
    for pid in expected:
        paths.setdefault(f"{pid}.jsonl", directory / f"{pid}.jsonl")
    records, processes = [], []
    for name, path in sorted(paths.items()):
        pid = int(path.stem) if path.stem.isdecimal() else None
        events, invalid, mismatch = [], 0, 0
        exists, unreadable, truncated = path.exists(), False, False
        if exists:
            try:
                raw = path.read_bytes()
            except OSError:
                raw, unreadable = b"", True
            truncated = bool(raw and not raw.endswith(b"\n"))
            for line in raw.decode("utf-8", errors="replace").splitlines():
                try:
                    record = json.loads(line)
                except json.JSONDecodeError:
                    invalid += 1
                    continue
                if (not isinstance(record, dict) or not isinstance(record.get("stage"), str)
                        or any(type(record.get(field)) is not int or record[field] < 0
                               for field in ("pid", "ns", "id", "scope", "value"))
                        or type(record.get("dropped", 0)) is not int or record.get("dropped", 0) < 0):
                    invalid += 1
                    continue
                if record["pid"] != pid:
                    mismatch += 1
                    continue
                events.append(record)
            records.extend(events)
        finishes = [event for event in events if event["stage"] == "process.finish"]
        post_finish = bool(finishes and any(event["ns"] > finishes[-1]["ns"] for event in events))
        dropped = max((event.get("dropped", 0) for event in events), default=0)
        finish_last = bool(events and events[-1]["stage"] == "process.finish")
        flush_failures = set()
        for diagnostic in diagnostics[pid]:
            try:
                content = diagnostic.read_bytes()
            except OSError:
                continue
            for message in ("latency recorder final flush timed out", "latency recorder final flush unavailable"):
                if message.encode() in content:
                    flush_failures.add(message)
        issues = []
        for condition, issue in ((not exists, "missing file"), (unreadable, "unreadable file"),
                                 (truncated, "truncated final record"), (invalid > 0, "invalid records"),
                                 (mismatch > 0, "record PID differs from filename"),
                                 (not finishes, "missing finish marker"),
                                 (bool(finishes) and not finish_last, "records after finish marker"),
                                 (post_finish, "event timestamp after finish marker"),
                                 (len(finishes) > 1, "multiple finish markers"),
                                 (bool(flush_failures), "observed bounded-flush failure"),
                                 (dropped > 0, "reported dropped records")):
            if condition:
                issues.append(issue)
        processes.append({"pid": pid, "file": name, "roles": expected.get(pid, []),
                          "expected": pid in expected, "exists": exists,
                          "record_count": len(events), "invalid_records": invalid,
                          "mismatched_pid_records": mismatch, "truncated": truncated,
                          "finish_marker_present": bool(finishes), "finish_marker_last": finish_last,
                          "finish_marker_count": len(finishes), "dropped_records": dropped,
                          "flush_failures": sorted(flush_failures),
                          "complete": not issues, "issues": issues})
    process_pids = [run.get("server_pid"), *run.get("client_pids", [])]
    expected_complete = (all(valid_pid(pid) for pid in process_pids)
                         and len(set(process_pids)) == len(process_pids)
                         and len(run.get("client_pids", [])) == run.get("clients"))
    return records, {"expected_process_count": len(expected),
                     "expected_process_metadata_complete": expected_complete,
                     "all_expected_complete": expected_complete and all(item["complete"] for item in processes if item["expected"]),
                     "missing_expected_files": [item["pid"] for item in processes if item["expected"] and not item["exists"]],
                     "total_reported_dropped_records": sum(item["dropped_records"] for item in processes),
                     "processes": processes,
                     "semantics": "A final finish marker confirms recorder flush; missing markers leave bounded-flush outcome unknown. Zero drops alone does not prove complete traces."}


def freshness(run):
    results = []
    events = run.get("load_events", [])
    for client, observations in enumerate(run.get("presented_load", [])):
        by_pane = defaultdict(list)
        for pane, generation, ns in observations:
            by_pane[pane].append((generation, ns))
        panes = []
        for public_pane in run.get("pane_ids", []):
            pane = public_pane.rsplit('p',1)[-1]
            generated = [event for event in events if event["pane"] == public_pane]
            presented = by_pane[pane]
            ages = []
            indexed = {item["generation"]:item for item in generated}
            presented_generations = {generation for generation,_ in presented}
            newest = max(presented_generations,default=-1)
            for generation, ns in presented:
                event = indexed.get(generation)
                if event and ns >= event["start_ns"]:
                    ages.append(ns-event["start_ns"])
            duration = (max((event["end_ns"] for event in generated),default=0)
                        - min((event["start_ns"] for event in generated),default=0)) / 1e9
            visible = run["load"] == "visible" and (run.get("layout") == "active" or public_pane == run["pane_ids"][0])
            panes.append({"pane":public_pane,"generated":len(generated),"presented":len(presented),
                          "superseded_by_observed_newer":sum(1 for item in generated if item["generation"] < newest and item["generation"] not in presented_generations) if visible else None,
                          "pending_or_missing":sum(1 for item in generated if item["generation"] > newest) if visible else None,
                          "coverage_window":"all producer events including warmup and drain",
                          "hidden_generated":len(generated) if not visible else 0,
                          "events_per_second":len(generated)/duration if duration>0 else None,
                          "bytes_per_second":sum(item["bytes"] for item in generated)/duration if duration>0 else None,
                          "freshness_max_ns":max(ages,default=None),"freshness_sample_count":len(ages)})
        results.append({"client":client,"panes":panes})
    return results


def miss_bursts(run):
    result = []
    for client in range(run["clients"]):
        streak = worst = 0
        for sample in run["samples"]:
            start = sample.get("process_start_ns") if run["path"] == "output" else sample["injected_ns"]
            end = sample["observed_ns"][client]
            miss = start is None or end is None or end-start > run["budget_ns"]
            streak = streak+1 if miss else 0
            worst = max(worst,streak)
        result.append(worst)
    return result

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("samples", type=Path)
    parser.add_argument("--traces", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    run = json.loads(args.samples.read_text())
    lines = ["# Latency measurement", "", "Endpoint: committed terminal bytes in owned outer PTYs.",
             "", f"Path: {run['path']}; load: {run['load']}; panes: {run['panes']}; clients: {run['clients']}.",
             "", "| Client | Completed | Missing | p50 ms | p95 ms | p99 ms |",
             "|---|---:|---:|---:|---:|---:|"]
    for index, summary in enumerate(run["summary"]):
        values = [summary.get(field) for field in ("p50_ns", "p95_ns", "p99_ns")]
        rendered = [f"{value / 1e6:.3f}" if value is not None else "unavailable" for value in values]
        lines.append(f"| {index} | {summary['completed']} | {summary['missing']} | {' | '.join(rendered)} |")
    lines.extend(["", f"Layout: {run.get('layout','unavailable')}; burst: {run.get('burst',1)}; warmups: {run.get('warmups',0)}.",
                  f"Maximum consecutive deadline misses per client: {miss_bursts(run)}.",
                  "", "Freshness and actual producer rates:", "", "```json", json.dumps(freshness(run),indent=2), "```"])
    records = []
    trace_audit = None
    if args.traces:
        records, trace_audit = load_traces(run, args.traces)
    pairs = pair_stages(records)
    paths = critical_paths(run, records, trace_audit)
    stage_report = stage_distributions(run, paths)
    coverage = stage_report["coverage"]
    lines.extend(["", f"Uniquely attributed effects: {coverage['uniquely_attributed_effects']} / {coverage['completed_effects']} completed; {coverage['offered_effects']} offered.",
                  f"Unassigned completed effects: {coverage['unassigned_completed_effects']}; multiple causal candidates: {coverage['ambiguous_completed_effects']}.",
                  "", "| Stage | Matched / completed | p50 ms | p95 ms | p99 ms | Accounting |",
                  "|---|---:|---:|---:|---:|---|"])
    for name, summary in stage_report["stages"].items():
        rendered = [f"{summary[field] / 1e6:.3f}" if summary[field] is not None else "unavailable"
                    for field in ("p50_ns", "p95_ns", "p99_ns")]
        lines.append(f"| {name} | {summary['sample_count']} / {summary['denominator']} | {' | '.join(rendered)} | {summary['accounting']} |")
    lines.extend(["", "Stage distributions use uniquely matched completed effects; per-stage percentiles are not additive. Socket-write duration overlaps delivery and is excluded from pipeline accounting.",
                  "CPU versus off-CPU remains unassigned; frame-to-surface and client-work intervals include scheduling and nested work."])
    if paths:
        worst_path = max(paths,key=lambda path:path["client_output_ns"]-path["start_ns"])
        effect = next((effect for effect in stage_report["effects"]
                       if effect["identity"] == worst_path["identity"] and effect["client_pid"] == worst_path["client_pid"]), None)
        lines.extend(["", "Representative slow path (host monotonic ns):", "", "```json",
                      json.dumps({"path": worst_path, "accounting": effect},indent=2), "```"])
    if pairs:
        worst = max(pairs, key=lambda pair: pair["queue_ns"])
        lines.extend(["", f"Largest paired server writer-queue residence: {worst['queue_ns'] / 1e6:.3f} ms.",
                      f"PID {worst['pid']}; frame fingerprint {worst['frame_fingerprint']}; enqueue {worst['enqueue_ns']}; claim {worst['claim_ns']}."])
    if trace_audit is not None:
        lines.extend(["", f"Trace records: {len(records)}; total reported dropped records: {trace_audit['total_reported_dropped_records']}.",
                      f"All expected process traces complete: {trace_audit['all_expected_complete']}; expected-process metadata complete: {trace_audit['expected_process_metadata_complete']}.",
                      "", "| Process | Roles | File | Records | Finish | Drops | Integrity |",
                      "|---|---|---|---:|---|---:|---|"])
        for process in trace_audit["processes"]:
            lines.append(f"| {process['pid']} | {', '.join(process['roles']) or 'unexpected'} | {process['file']} | {process['record_count']} | {process['finish_marker_present']} | {process['dropped_records']} | {'; '.join(process['issues']) or 'complete'} |")
        lines.extend(["", trace_audit["semantics"]])
    if args.output:
        args.output.with_suffix(".json").write_text(json.dumps({"queue_pairs": pairs,"critical_paths":paths,
            "stage_report":stage_report,"trace_audit":trace_audit,"freshness":freshness(run),
            "max_consecutive_deadline_misses":miss_bursts(run)}, indent=2)+"\n")
    lines.extend(["", "Queue pairing uses process and queue identity plus content fingerprints. Causal paths require terminal/snapshot links and client write completion. Ambiguous links remain unassigned.",
                  "", "Results include observer and host scheduling. No pixel, GPU, remote one-way, or production-SLO claim.", ""])
    text = "\n".join(lines)
    if args.output:
        args.output.write_text(text)
    else:
        print(text)


if __name__ == "__main__":
    main()
