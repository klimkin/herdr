"""Report render-slot feedback separately from completed socket writes."""

import argparse
from collections import Counter, defaultdict, deque
import json
from pathlib import Path

from latency_report import process_trace_complete, read_process_traces


def summarize(records, expected_pids=None, trace_audit=None):
    """Keep per-process queues distinct; a retry request is not delivered output."""
    clients = {}
    begins = defaultdict(deque)
    finishes = Counter()
    drops = Counter()
    loops = Counter()
    orphaned = Counter()
    for record in sorted(records, key=lambda item: item["ns"]):
        stage = record["stage"]
        pid = record["pid"]
        value = record.get("value", 0)
        drops[pid] = max(drops[pid], record.get("dropped", 0))
        if stage == "process.finish":
            finishes[pid] += 1
        if stage == "writer.server_pass":
            loops[pid] += 1
        if stage == "writer.feedback.orphaned":
            orphaned[pid] += 1
        if not stage.startswith("writer.") or not record.get("scope"):
            continue
        key = (pid, record["scope"])
        row = clients.setdefault(key, {
            "pid": pid, "scope": key[1], "client_id": None,
            "render_dequeues": 0, "control_dequeues": 0,
            "render_write_completions": 0, "control_write_completions": 0,
            "render_written_bytes": 0, "control_written_bytes": 0,
            "feedback_attempted": 0, "feedback_sent": 0, "feedback_send_failed": 0,
            "feedback_requesting_retry": 0, "feedback_without_deferred_work": 0,
            "feedback_send_wait_ns": 0, "feedback_send_max_wait_ns": 0,
            "event_channel_high_water": 0, "control_lane_high_water": 0,
            "render_lane_high_water": 0, "writer_wakes": 0,
            "deferred_registrations": 0, "writer_exits": 0,
            "feedback_pending_high_water": 0, "feedback_attempts_while_pending": 0,
            "feedback_unhandled": 0,
        })
        if stage == "writer.client":
            row["client_id"] = record["id"]
        elif stage == "writer.dequeue.render":
            row["render_dequeues"] += 1
        elif stage == "writer.dequeue.control":
            row["control_dequeues"] += 1
        elif stage == "writer.write_complete.render":
            row["render_write_completions"] += 1
            row["render_written_bytes"] += value
        elif stage == "writer.write_complete.control":
            row["control_write_completions"] += 1
            row["control_written_bytes"] += value
        elif stage == "writer.feedback.begin":
            row["feedback_attempted"] += 1
            if row["feedback_unhandled"]:
                row["feedback_attempts_while_pending"] += 1
            row["feedback_unhandled"] += 1
            row["feedback_pending_high_water"] = max(row["feedback_pending_high_water"], row["feedback_unhandled"])
            row["event_channel_high_water"] = max(row["event_channel_high_water"], value)
            begins[key].append(record["ns"])
        elif stage == "writer.feedback.end":
            row["feedback_sent" if value else "feedback_send_failed"] += 1
            if not value:
                row["feedback_unhandled"] = max(0, row["feedback_unhandled"] - 1)
            if begins[key]:
                wait = record["ns"] - begins[key].popleft()
                row["feedback_send_wait_ns"] += wait
                row["feedback_send_max_wait_ns"] = max(row["feedback_send_max_wait_ns"], wait)
        elif stage == "writer.feedback.handled":
            row["feedback_requesting_retry" if value else "feedback_without_deferred_work"] += 1
            row["feedback_unhandled"] = max(0, row["feedback_unhandled"] - 1)
        elif stage == "writer.queue.occupancy":
            row["control_lane_high_water"] = max(row["control_lane_high_water"], record["id"])
            row["render_lane_high_water"] = max(row["render_lane_high_water"], value)
        elif stage == "writer.wake":
            row["writer_wakes"] += value
        elif stage == "writer.deferred":
            row["deferred_registrations"] += 1
        elif stage == "writer.exit":
            row["writer_exits"] += 1

    for row in clients.values():
        attempts = row["feedback_attempted"]
        handled = row["feedback_requesting_retry"] + row["feedback_without_deferred_work"]
        row["feedback_per_render_dequeue"] = attempts / row["render_dequeues"] if row["render_dequeues"] else None
        row["feedback_without_deferred_fraction"] = row["feedback_without_deferred_work"] / handled if handled else None
        row["pending_feedback_sends"] = len(begins[(row["pid"], row["scope"])])

    pids = {record["pid"] for record in records}
    expected = set(expected_pids or [])
    metadata_complete = (bool(expected) and len(expected) == len(expected_pids)
                         and all(type(pid) is int and pid > 0 for pid in expected))
    return {
        "complete": metadata_complete and expected <= pids and all(
            process_trace_complete(records, pid, trace_audit) for pid in expected | pids)
            and (trace_audit is None or all(item["complete"] for item in trace_audit["processes"])),
        "trace_audit": trace_audit,
        "expected_processes": sorted(expected),
        "missing_processes": sorted(expected - pids),
        "missing_finish_processes": sorted(pid for pid in expected | pids if finishes[pid] != 1),
        "record_drops": dict(drops), "server_passes": dict(loops),
        "orphaned_feedback": dict(orphaned),
        "clients": sorted(clients.values(), key=lambda row: (row["pid"], row["scope"])),
        "limits": [
            "A dequeue frees queue space before socket write completion.",
            "Feedback requesting a retry does not prove that a frame was admitted or delivered.",
            "Feedback without deferred work is a coalescing candidate, not proof its removal is race-safe.",
            "Writer wakes count condition-variable returns; server passes count main-loop iterations.",
            "Render-lane occupancy includes ordered direct items; its maximum is three queued items.",
            "Overlap counts begin-to-handler notification lifetime, including blocked send; record loss can invalidate the count.",
            "In-memory summaries audit finish order and drops; file integrity and flush diagnostics require the CLI trace audit.",
            "The CLI PID list carries no process roles; any companion flush failure conservatively invalidates every expected trace.",
            "CPU, memory, observer throughput, latency, and diagnostic overhead require companion benchmark results.",
        ],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("trace_directory", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--expected-pid", type=int, action="append", default=[],
                        help="repeat for server and each client PID from benchmark results")
    args = parser.parse_args()
    expected = {pid: ["expected process"] for pid in args.expected_pid}
    # --expected-pid has no ordering or role contract. A captured companion
    # flush failure invalidates completeness without inventing PID ownership.
    companions = [args.trace_directory.parent / "server.stderr",
                  *sorted(args.trace_directory.parent.glob("client-*.vt"))]
    records, audit = read_process_traces(
        args.trace_directory, expected, {pid: companions for pid in expected})
    if len(expected) != len(args.expected_pid):
        audit["expected_process_metadata_complete"] = False
        audit["all_expected_complete"] = False
    output = json.dumps(summarize(records, args.expected_pid, audit), indent=2) + "\n"
    if args.output:
        args.output.write_text(output)
    else:
        print(output, end="")


if __name__ == "__main__":
    main()
