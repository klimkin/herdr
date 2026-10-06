"""Summarize benchmark records and pair transport stages on one clock domain."""

import argparse
from collections import defaultdict, deque
import json
from pathlib import Path


def pair_stages(records):
    pending = defaultdict(deque)
    pairs = []
    for record in sorted(records, key=lambda item: item["ns"]):
        # Scope identifies one queue; legacy records cannot prove this pairing.
        if not record.get("scope"):
            continue
        key = (record["pid"], record["scope"], record["id"])
        if record["stage"] == "server.queue_discard":
            for pending_key in list(pending):
                if pending_key[:2] == key[:2]:
                    del pending[pending_key]
        elif record["stage"] == "server.enqueue":
            pending[key].append(record)
        elif record["stage"] == "server.writer_claim" and pending[key]:
            enqueued = pending[key].popleft()
            pairs.append({"pid": record["pid"], "scope": record["scope"], "frame_fingerprint": record["id"],
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


def fingerprint(text):
    value = 0xcbf29ce484222325
    for byte in text.encode():
        value = ((value ^ byte) * 0x100000001b3) & ((1 << 64) - 1)
    return value


def critical_paths(run, records):
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
                    # Repeated identical payloads on several connections cannot prove which delivered.
                    candidates = [pair for pair in queues if pair["pid"] == queue["pid"]
                                  and pair["frame_fingerprint"] == wire and pair["claim_ns"] <= output["ns"]]
                    if len(candidates) != 1:
                        continue
                    key = (sample["identity"], output["pid"], wire)
                    if key in seen:
                        continue
                    seen.add(key)
                    receive = next((item["ns"] for item in stages["transport.receive"]
                                    if item["pid"] == output["pid"] and item["id"] == wire
                                    and queue["claim_ns"] <= item["ns"] <= output["ns"]), None)
                    written = next((item["ns"] for item in stages["server.write_complete"]
                                    if item["pid"] == queue["pid"] and item["id"] == wire
                                    and item.get("scope") == queue["scope"]
                                    and queue["claim_ns"] <= item["ns"] <= output["ns"]), None)
                    client_pids = run.get("client_pids", [])
                    client_index = client_pids.index(output["pid"]) if output["pid"] in client_pids else None
                    observed = sample["observed_ns"][client_index] if client_index is not None else None
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
                                  "terminal_ready_ns":stimulus["ns"] if stimulus else None,
                                  "terminal_revision":stimulus["value"] if stimulus else None,
                                  "surface_content_revision":surface["value"], "surface_ns":surface["ns"],"presentation_gate":gate,"forward_boundaries":forward,"action_commit_ns":action_commit["ns"] if action_commit else None,
                                  "enqueue_ns":queue["enqueue_ns"],"claim_ns":queue["claim_ns"],
                                  "queue_ns":queue["queue_ns"],"socket_complete_ns":written,
                                  "client_receive_ns":receive,"client_output_ns":output["ns"],
                                  "outer_observed_ns":observed,
                                  "unavailable":["CPU versus off-CPU", "PTY submission queue residence"] + ([] if gate else ["per-stimulus selected gate"]) + ([] if client_index is not None else ["client index mapping"])})
    return paths


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
    if args.traces:
        for path in args.traces.glob("*.jsonl"):
            for line in path.read_text().splitlines():
                try:
                    records.append(json.loads(line))
                except json.JSONDecodeError:
                    lines.append(f"\nIncomplete trace record: {path.name}.")
        pairs = pair_stages(records)
        paths = critical_paths(run, records)
        lines.extend(["", f"Proven causal paths: {len(paths)}. Missing or ambiguous links remain unavailable."])
        if paths:
            worst_path = max(paths,key=lambda path:path["client_output_ns"]-path["start_ns"])
            lines.extend(["", "Representative slow path (host monotonic ns):", "", "```json", json.dumps(worst_path,indent=2), "```"])
        if pairs:
            worst = max(pairs, key=lambda pair: pair["queue_ns"])
            lines.extend(["", f"Largest paired server writer-queue residence: {worst['queue_ns'] / 1e6:.3f} ms.",
                          f"PID {worst['pid']}; frame fingerprint {worst['frame_fingerprint']}; enqueue {worst['enqueue_ns']}; claim {worst['claim_ns']}."])
        dropped = max((record.get("dropped", 0) for record in records), default=0)
        lines.extend(["", f"Trace records: {len(records)}; maximum reported dropped records per process: {dropped}."])
        if args.output:
            args.output.with_suffix(".json").write_text(json.dumps({"queue_pairs": pairs,"critical_paths":paths,"freshness":freshness(run),"max_consecutive_deadline_misses":miss_bursts(run)}, indent=2)+"\n")
    lines.extend(["", "Queue pairing uses process and queue identity plus content fingerprints. Causal paths require terminal/snapshot links and client write completion. Ambiguous links remain unassigned.",
                  "", "Results include observer and host scheduling. No pixel, GPU, remote one-way, or production-SLO claim.", ""])
    text = "\n".join(lines)
    if args.output:
        args.output.write_text(text)
    else:
        print(text)


if __name__ == "__main__":
    main()
