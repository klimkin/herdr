"""Exercise required outcomes, artifact isolation and owned-process cleanup."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "bench", "probe"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="herdr-latency-smoke-") as directory:
        output = Path(directory)
        runs = []
        for path, extra in [("output", []), ("output", []), ("echo", []),
                            ("action", ["--clients", "2"]),
                            ("action", ["--clients", "3", "--stall-ms", "150"]),
                            ("action", ["--clients", "3", "--slow-reader-ms", "20"]),
                            ("echo", ["--interval-ms", "0", "--max-pending", "1"])]:
            before = set(output.glob("*/samples.json"))
            command = [str(args.bench.resolve()), "--binary", str(args.binary.resolve()),
                       "--probe", str(args.probe.resolve()), "--output", str(output),
                       "--path", path, "--samples", "8", "--warmups", "0"] + extra
            result = subprocess.run(command, capture_output=True, text=True, timeout=40)
            if result.returncode:
                raise RuntimeError(result.stdout + result.stderr)
            created = set(output.glob("*/samples.json")) - before
            assert len(created) == 1, "each run must create one unique report"
            run = json.loads(created.pop().read_text())
            assert not Path(run["base"]).exists(), "temporary session directories survived cleanup"
            if os.name == "posix":
                for pid in [run["server_pid"]] + run["client_pids"]:
                    try:
                        os.kill(pid, 0)
                    except ProcessLookupError:
                        continue
                    raise AssertionError(f"owned process {pid} survived cleanup")
            if extra[-2:] == ["--max-pending", "1"]:
                assert any(sample["outcome"].startswith("rejected") for sample in run["samples"])
                assert len(run["samples"]) == 8, "rejected samples disappeared"
            else:
                assert all(summary["completed"] == 8 and summary["missing"] == 0
                           for summary in run["summary"]), run["summary"]
            report = output / run["run_id"] / "recovery-report.md"
            subprocess.run(["python3", "scripts/latency_report.py", str(report.parent / "samples.json"),
                            "--output", str(report)], check=True, timeout=30)
            recovery = json.loads(report.with_suffix(".json").read_text())["reader_recovery"]
            if run["stall_ms"]:
                assert recovery["status"] == "measured", recovery
                lifecycle = recovery["lifecycle"]
                assert lifecycle["client_index"] == 2 and lifecycle["client_pid"] == run["client_pids"][2]
                assert lifecycle["paused_ns"] <= run["started_ns"], "offered probes preceded pause acknowledgment"
                assert lifecycle["reset_complete_ns"] <= lifecycle["reader_resumed_ns"] <= lifecycle["first_read_attempt_ns"] <= lifecycle["first_receipt_ns"]
                assert recovery["cohort_accepted"] > 0, "stall offered no required backlog"
                assert len(recovery["clients"]) == 3
                assert all(client["missing"] == 0 for client in recovery["clients"]), recovery
                assert recovery["clients"][2]["backlog_at_reset"] > 0, "paused reader had no backlog"
                assert recovery["clients"][2]["catch_up_ns"] >= lifecycle["reader_resumed_ns"]
            else:
                assert recovery["status"] == "not_applicable", recovery
            runs.append(run)
        assert len({run["run_id"] for run in runs}) == len(runs)
        print("Latency smoke: output, echo, fanout, rejection, isolation and cleanup passed")


if __name__ == "__main__":
    main()
