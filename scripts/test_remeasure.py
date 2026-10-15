"""Contracts for percentile pooling, native accounting, and safe collection."""

import json
import os
from pathlib import Path
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS))
from remeasure_report import _verify_capture, cpu_intervals, distribution, report
from remeasure_resources import Resources


class Distributions(unittest.TestCase):
    def test_nearest_rank_and_p999_population_gate(self):
        small = distribution([9, 1, 3, 5])
        self.assertEqual((small["p50"], small["p95"], small["mean"]), (3, 9, 4.5))
        self.assertIsNone(distribution(range(9999))["p99.9"])
        self.assertEqual(distribution(range(1, 10001))["p99.9"], 9990)

    def test_cpu_interpolates_window_and_separates_console(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "resources.jsonl"
            rows = [self.snapshot(second) for second in (0, 1, 3)]
            path.write_text("\n".join(json.dumps(row) for row in rows))
            cpu = cpu_intervals(path, 500_000_000, 2_500_000_000, 1, [2], 4)
            self.assertTrue(cpu["valid"], cpu["failures"])
            self.assertEqual(cpu["coverage_ns"], 2_000_000_000)
            self.assertEqual(cpu["values"]["Total CPU"], [3, 3])
            self.assertEqual(cpu["values"]["Console host CPU"], [3, 3])
            self.assertEqual(cpu["values"]["Owned CPU"], [6, 6])

    def test_reused_process_invalidates_cpu(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "resources.jsonl"
            rows = [self.snapshot(second) for second in (0, 1)]
            rows[-1]["processes"]["2"]["start"] = 999
            path.write_text("\n".join(json.dumps(row) for row in rows))
            cpu = cpu_intervals(path, 0, 1_000_000_000, 1, [2], 4)
            self.assertFalse(cpu["valid"])
            self.assertTrue(any("identity changed" in error for error in cpu["failures"]))
            self.assertEqual(cpu["values"]["Total CPU"], [])

    def test_missing_endpoint_is_not_valid_cpu(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "resources.jsonl"
            path.write_text("\n".join(json.dumps(self.snapshot(second)) for second in (1, 2)))
            cpu = cpu_intervals(path, 0, 2_000_000_000, 1, [2], 4)
            self.assertFalse(cpu["valid"])

    def test_report_excludes_smoke_and_flags_missing_matrix(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "manifest.json").write_text(json.dumps({
                "refs": {"before": "base", "after": "candidate"},
                "jobs": [{"name": "echo1"}], "repetitions": 1, "logical_cpus": 4,
            }))
            smoke = root / "smoke/echo1/before-r0"
            smoke.mkdir(parents=True)
            (smoke / "result.json").write_text('{}')
            summary = report(root)
            self.assertEqual(len(summary["excluded_smoke"]), 1)
            self.assertEqual(len(summary["failures"]), 4)
            self.assertEqual(summary["distributions"], [])
            self.assertIn("missing successful capture", (root / "report.md").read_text())

    def test_report_rejects_modified_raw_evidence(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "samples.json").write_text('{"samples": []}')
            with self.assertRaisesRegex(ValueError, "hash mismatch"):
                _verify_capture(root, root / "result.json", {
                    "samples_file": "samples.json", "variant": "after", "job": "echo1",
                    "evidence_sha256": {"samples.json": "0" * 64},
                }, {"jobs": [{"name": "echo1", "samples": 8}]})

    @staticmethod
    def snapshot(second):
        return {"ns": second * 1_000_000_000, "host_cpu_ns": second * 100_000_000,
                "owned": [1, 2, 3], "processes": {
                    str(pid): {"ppid": 0, "start": pid, "cpu_ns": second * pid * 10_000_000,
                               "name": "OpenConsole.exe" if pid == 3 else "herdr"}
                    for pid in (1, 2, 3)}}


@unittest.skipUnless(sys.platform == "linux", "live Linux pidfd contract")
class NativeOwnership(unittest.TestCase):
    def test_launch_affinity_and_wrong_identity_refusal(self):
        resources = Resources()
        cpu = min(os.sched_getaffinity(0))
        with tempfile.TemporaryFile() as log:
            proc = resources.launch([sys.executable, "-c", "import time; time.sleep(30)"], os.environ.copy(), log, [cpu])
            try:
                snapshot = resources.snapshot()
                created = snapshot["processes"][proc.pid]["start"]
                self.assertEqual(os.sched_getaffinity(proc.pid), {cpu})
                self.assertFalse(resources.terminate(proc.pid, created + 1))
                self.assertIsNone(proc.poll())
            finally:
                resources.close_launch(proc)
                proc.wait(timeout=5)
            self.assertFalse(resources.alive(proc.pid, created))


if __name__ == "__main__":
    unittest.main()
