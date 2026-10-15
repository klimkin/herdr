#!/usr/bin/env python3
"""Rebuild frozen Herdr revisions and collect native, paired measurements."""

import argparse
from dataclasses import asdict, dataclass
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import time

from remeasure_resources import Resources, descendants
from remeasure_report import cpu_intervals, report

BASELINE = "2563803dca97c040beaf3dc3acdcb5a3221b4238"
CANDIDATE = "45c5bb3f7e27630feb47203e2a5a9464f5576dda"
REPO = Path(__file__).resolve().parent.parent


@dataclass(frozen=True)
class Job:
    name: str
    path: str
    load: str
    layout: str
    panes: int
    samples: int


JOBS = (
    Job("echo15", "echo", "visible", "active", 15, 2000),
    Job("echo1", "echo", "visible", "active", 1, 600),
    Job("action1", "action", "visible", "active", 1, 600),
    Job("action15", "action", "visible", "active", 15, 600),
    Job("hidden15", "echo", "hidden", "tabs", 15, 600),
)


def save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    temporary.replace(path)


def sha256(path):
    with path.open("rb") as source:
        digest = hashlib.sha256()
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def capture(command, cwd=REPO, env=None):
    return subprocess.check_output(command, cwd=cwd, env=env, text=True, stderr=subprocess.STDOUT).strip()


def environment():
    env = os.environ.copy()
    for name in list(env):
        if name.startswith(("HERDR_", "TRACY_")):
            del env[name]
    return env


def allowed_cpus():
    if sys.platform == "linux":
        return sorted(os.sched_getaffinity(0))
    import ctypes
    from ctypes import wintypes
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.GetCurrentProcess.restype = wintypes.HANDLE
    kernel.GetProcessAffinityMask.argtypes = [wintypes.HANDLE, ctypes.POINTER(ctypes.c_size_t), ctypes.POINTER(ctypes.c_size_t)]
    process_mask, system_mask = ctypes.c_size_t(), ctypes.c_size_t()
    if not kernel.GetProcessAffinityMask(kernel.GetCurrentProcess(), ctypes.byref(process_mask), ctypes.byref(system_mask)):
        raise ctypes.WinError(ctypes.get_last_error())
    return [index for index in range(64) if process_mask.value & (1 << index)]


def plan(args):
    available = allowed_cpus()
    cpus = [int(value) for value in args.cpus.split(",")] if args.cpus else available[:8]
    if not cpus or len(set(cpus)) != len(cpus) or not set(cpus) <= set(available):
        raise ValueError(f"CPUs must be distinct and available: {available}")
    return {
        "schema": 1, "refs": {"before": args.baseline, "after": args.candidate},
        "platform": sys.platform, "rust_toolchain": args.rust_toolchain,
        "zig_version": "0.16.0", "cpus": cpus, "logical_cpus": os.cpu_count(),
        "jobs": [asdict(job) for job in JOBS], "repetitions": args.repetitions,
        "order": [["before", "after"] if repeat % 2 == 0 else ["after", "before"]
                  for repeat in range(args.repetitions)],
        "warmups": 10, "seed": 92341, "interval_ms": 40, "load_hz": 60,
        "idle_seconds": args.idle_seconds, "settle_seconds": 5,
        "geometry": [120, 40], "recording": "disabled", "profiling": "disabled",
        "topology": "one real TUI connected to one local server",
        "observer_sources": {str(path.relative_to(REPO)): sha256(path)
                             for path in observer_sources()},
        "runner_sources": {path.name: sha256(path) for path in Path(__file__).parent.glob("remeasure*")
                           if path.is_file()},
    }


def observer_sources():
    return sorted([REPO / "examples/latency_bench.rs", REPO / "examples/latency_probe.rs"]
                  + list((REPO / "examples/latency").rglob("*.rs"))
                  + list((REPO / "src/platform/latency").rglob("*.rs"))
                  + [REPO / "src/platform/latency.rs"])


def logged(command, cwd, env, log):
    print("BUILD " + " ".join(map(str, command)), flush=True)
    with log.open("w", encoding="utf-8") as output:
        proc = subprocess.run(command, cwd=cwd, env=env, stdout=output, stderr=subprocess.STDOUT)
    if proc.returncode:
        raise RuntimeError(f"command failed ({proc.returncode}): {log}")


def cargo_binary(tree, env, name, log):
    kind = "--bin" if name == "herdr" else "--example"
    logged(["cargo", "build", "--release", "--locked", kind, name, "--message-format=json"], tree, env, log)
    matches = []
    for line in log.read_text(encoding="utf-8", errors="replace").splitlines():
        if not line.startswith("{"):
            continue
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and message.get("target", {}).get("name") == name and message.get("executable"):
            matches.append(Path(message["executable"]))
    if len(matches) != 1:
        raise RuntimeError(f"expected one Cargo executable for {name}: {log}")
    return matches[0]


def freeze_package(binary, destination):
    destination.mkdir(parents=True)
    shutil.copy2(binary, destination / binary.name)
    if os.name == "nt":
        for directory in ("conpty", "THIRD-PARTY-NOTICES"):
            source = binary.parent / directory
            if not source.is_dir():
                raise RuntimeError(f"missing app-local Windows package: {source}")
            shutil.copytree(source, destination / directory)
    return destination / binary.name


def build(root, spec, args):
    for source in observer_sources():
        relative = str(source.relative_to(REPO))
        target = root / "observer-sources" / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
        if sha256(target) != spec["observer_sources"][relative]:
            raise ValueError(f"observer changed while freezing source: {source}")
    env = environment()
    env["RUSTUP_TOOLCHAIN"] = args.rust_toolchain
    for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS", "CARGO_BUILD_TARGET", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"):
        env.pop(name, None)
    zig = args.zig or env.get("ZIG") or shutil.which("zig")
    if not zig or capture([zig, "version"]) != "0.16.0":
        raise ValueError("Zig 0.16.0 required; pass --zig PATH or set ZIG")
    env["ZIG"] = str(Path(zig).resolve())
    versions = {name: capture(command, env=env) for name, command in {
        "rustc": ["rustc", "-vV"], "cargo": ["cargo", "--version"],
        "just": ["just", "--version"], "zig": [zig, "version"], "git": ["git", "--version"],
    }.items()}
    host = next(line.split(": ", 1)[1] for line in versions["rustc"].splitlines() if line.startswith("host:"))
    if os.name == "nt" and (host != "x86_64-pc-windows-msvc" or (os.cpu_count() or 0) > 64):
        raise ValueError("Windows runner requires native x86_64 MSVC and at most 64 logical CPUs")
    versions.update(python=sys.version, host=platform.platform())
    paths, trees = {}, []
    logs = root / "build-logs"
    logs.mkdir()
    cache = args.target_root.resolve() if args.target_root else root / "targets"
    try:
        for side, revision in spec["refs"].items():
            if len(revision) != 40 or any(char not in "0123456789abcdef" for char in revision):
                raise ValueError("provide full lowercase 40-character source SHAs")
            actual = capture(["git", "rev-parse", revision + "^{commit}"])
            if actual != revision:
                raise ValueError(f"source mismatch: {revision}")
            tree = root / "sources" / side
            tree.parent.mkdir(exist_ok=True)
            subprocess.run(["git", "worktree", "add", "--detach", str(tree), revision], cwd=REPO, check=True)
            trees.append(tree)
            env["CARGO_TARGET_DIR"] = str(cache / revision)
            env["SOURCE_DATE_EPOCH"] = capture(["git", "show", "-s", "--format=%ct", revision])
            logged(["just", "build"], tree, env, logs / f"{side}-just-build.log")
            binary = cargo_binary(tree, env, "herdr", logs / f"{side}-artifacts.log")
            paths[side] = str(freeze_package(binary, root / "bin" / side).relative_to(root))
            if side == "after":
                for relative, expected in spec["observer_sources"].items():
                    source = root / "observer-sources" / relative
                    if sha256(source) != expected:
                        raise ValueError(f"frozen observer source changed: {source}")
                    target = tree / relative
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(source, target)
                overlay = hashlib.sha256(json.dumps(spec["observer_sources"], sort_keys=True).encode()).hexdigest()[:16]
                env["CARGO_TARGET_DIR"] = str(cache / f"observer-{revision}-{overlay}")
                bench = cargo_binary(tree, env, "latency_bench", logs / "observer-build.log")
                probe = cargo_binary(tree, env, "latency_probe", logs / "probe-build.log")
                observer_dir = root / "bin" / "observer"
                observer_dir.mkdir()
                shutil.copy2(bench, observer_dir / bench.name)
                shutil.copy2(probe, observer_dir / probe.name)
                if os.name == "nt":
                    package = (root / paths[side]).parent
                    for directory in ("conpty", "THIRD-PARTY-NOTICES"):
                        shutil.copytree(package / directory, observer_dir / directory)
                paths.update(harness=str((observer_dir / bench.name).relative_to(root)),
                             probe=str((observer_dir / probe.name).relative_to(root)))
        manifest = {**spec, "paths": paths, "tools": versions,
                    "resource_accounting": Resources().metadata,
                    "files": {str(path.relative_to(root)): sha256(path)
                              for directory in (root / "bin", root / "observer-sources")
                              for path in directory.rglob("*") if path.is_file()},
                    "created_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                    "limits": ["host scheduling and frequency vary", "byte endpoint excludes physical keypress and display",
                               "local transport, one server; three-server topology unmeasured",
                               "producer generations correlated", "no per-change causal attribution",
                               "sequential process snapshots; short-lived descendants may escape CPU accounting"]}
        save(root / "manifest.json", manifest)
        return manifest
    finally:
        for tree in reversed(trees):
            # These worktrees were created by this invocation; only observer files were copied in.
            subprocess.run(["git", "worktree", "remove", "--force", str(tree)], cwd=REPO, check=True)


def verify_manifest(root, manifest, spec):
    for key, value in spec.items():
        if manifest.get(key) != value:
            raise ValueError(f"resume plan changed at {key}; use a new output directory")
    for relative, expected in manifest["files"].items():
        if sha256(root / relative) != expected:
            raise ValueError(f"frozen binary/package changed: {relative}")


def quiet_preflight(resources):
    first = resources.snapshot()
    time.sleep(3)
    last = resources.snapshot()
    duration = (last["ns"] - first["ns"]) / 1e9
    cpu = (last["host_cpu_ns"] - first["host_cpu_ns"]) / 1e9 / duration * 100
    return {"elapsed_seconds": duration, "host_cpu_pct_one_core": cpu,
            "host_cpu_pct_machine": cpu / (os.cpu_count() or 1)}


def collect(root, manifest, job, side, repeat, resources, smoke=False, idle=False):
    group = "smoke" if smoke else "idle" if idle else "runs"
    folder = root / group / (f"{side}-r{repeat}" if idle and not smoke else f"{job.name}/{side}-r{repeat}")
    if (folder / "result.json").exists():
        result = json.loads((folder / "result.json").read_text(encoding="utf-8"))
        if not all(sha256(root / path) == digest for path, digest in result["evidence_sha256"].items()):
            raise ValueError(f"completed evidence changed: {folder}")
        print(f"VERIFIED {folder.relative_to(root)}", flush=True)
        return
    if folder.exists():
        raise ValueError(f"incomplete run retained: {folder}; use a new output directory")
    folder.mkdir(parents=True)
    command = [str(root / manifest["paths"]["harness"]), "--binary", str(root / manifest["paths"][side]),
               "--probe", str(root / manifest["paths"]["probe"]), "--output", str(folder)]
    if idle:
        command += ["--idle-seconds", "2" if smoke else str(manifest["idle_seconds"]),
                    "--settle-seconds", "1" if smoke else str(manifest["settle_seconds"])]
    else:
        command += ["--path", job.path, "--load", job.load, "--layout", job.layout,
                    "--panes", str(job.panes), "--clients", "1", "--samples", "8" if smoke else str(job.samples),
                    "--warmups", "10", "--interval-ms", "40", "--load-hz", "60"]
    save(folder / "command.json", {"argv": command, "cpus": manifest["cpus"]})
    preflight = quiet_preflight(resources)
    print(f"START {group} {job.name} {side} r{repeat}", flush=True)
    owned = {}
    proc = None
    cleanup = {}
    try:
        with (folder / "bench.log").open("w", encoding="utf-8") as log, (folder / "resources.jsonl").open("w", encoding="utf-8") as raw:
            proc = resources.launch(command, environment(), log, manifest["cpus"])
            deadline = time.monotonic() + (manifest["idle_seconds"] + 90 if idle else max(180, job.samples * .12 + 90))
            while True:
                snapshot = resources.snapshot()
                found = descendants(snapshot["processes"], proc.pid) | {proc.pid}
                owned.update({pid: snapshot["processes"][pid]["start"] for pid in found if pid in snapshot["processes"]})
                snapshot["processes"] = {pid: data for pid, data in snapshot["processes"].items()
                                         if owned.get(pid) == data["start"]}
                snapshot["owned"] = sorted(snapshot["processes"])
                raw.write(json.dumps(snapshot) + "\n")
                raw.flush()
                if proc.poll() is not None:
                    break
                if time.monotonic() > deadline:
                    raise TimeoutError(f"benchmark deadline exceeded: {folder}")
                time.sleep(.25)
        if proc.returncode:
            raise RuntimeError(f"benchmark exited {proc.returncode}: {folder / 'bench.log'}")
        files = list(folder.glob("*/samples.json"))
        if len(files) != 1:
            raise ValueError(f"expected one sample file: {folder}")
        samples = json.loads(files[0].read_text(encoding="utf-8"))
        expected_hash = manifest["files"][manifest["paths"][side]]
        if samples["binary_sha256"] != expected_hash:
            raise ValueError("harness ran wrong binary")
        window = (samples["measurement_first_intended_ns"], samples["measurement_last_intended_ns"])
        if not all(isinstance(value, int) for value in window) or window[1] <= window[0]:
            raise ValueError("invalid native measurement window")
        if idle and samples["terminal_bytes_during_measurement"] != 0:
            raise ValueError("idle terminal emitted bytes during measurement")
        if not idle and (len(samples["samples"]) != (8 if smoke else job.samples)
                         or any(sample["outcome"] != "presented" or any(value is None for value in sample["observed_ns"])
                                for sample in samples["samples"])):
            raise ValueError("missing/rejected/timed-out effects; raw capture retained")
        accounting = cpu_intervals(folder / "resources.jsonl", *window,
                                   samples["server_pid"], samples["client_pids"], manifest["logical_cpus"])
        if not accounting["valid"]:
            raise ValueError(f"incomplete CPU accounting: {accounting['failures']}")
        alive = [pid for pid, created in owned.items() if pid != proc.pid and resources.alive(pid, created)]
        cleanup = {"processes_gone_before_forced_cleanup": not alive,
                   "base_removed": not Path(samples["base"]).exists(), "survivors": alive}
        if alive or not cleanup["base_removed"]:
            raise ValueError(f"harness cleanup failed: {cleanup}")
        result = {"variant": side, "job": job.name, "repeat": repeat,
                  "samples_file": str(files[0].relative_to(root)), "preflight": preflight,
                  "cleanup": cleanup, "owned_identities": owned,
                  "binary_sha256": expected_hash,
                  "cpu_means_pct_one_core": {label: sum(value * duration for value, duration in zip(values, accounting["seconds"])) / sum(accounting["seconds"])
                                              for label, values in accounting["values"].items()},
                  "evidence_sha256": {path.relative_to(root).as_posix(): sha256(path)
                                      for path in [files[0], folder / "resources.jsonl", folder / "command.json"]}}
        save(folder / "result.json", result)
        print(f"DONE {group} {job.name} {side} r{repeat}", flush=True)
    except BaseException as error:
        save(folder / "failure.json", {"error": str(error), "cleanup": cleanup, "owned_identities": owned})
        raise
    finally:
        if proc is not None:
            resources.close_launch(proc)
            for pid, created in reversed(list(owned.items())):
                if resources.alive(pid, created):
                    resources.terminate(pid, created)
            if proc.poll() is None:
                proc.wait(timeout=10)
            deadline = time.monotonic() + 2
            while any(resources.alive(pid, created) for pid, created in owned.items()) and time.monotonic() < deadline:
                time.sleep(.05)
            for pid, created in owned.items():
                if resources.alive(pid, created):
                    resources.kill(pid, created)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--phase", choices=("plan", "build", "smoke", "measure", "all", "report"), default="all")
    parser.add_argument("--baseline", default=BASELINE)
    parser.add_argument("--candidate", default=CANDIDATE)
    parser.add_argument("--rust-toolchain", default="1.96.1")
    parser.add_argument("--zig", help="Zig 0.16.0 executable")
    parser.add_argument("--cpus", help="comma-separated CPU indices; default first eight allowed CPUs")
    parser.add_argument("--repetitions", type=int, default=5)
    parser.add_argument("--idle-seconds", type=int, default=60)
    parser.add_argument("--target-root", type=Path, help="optional persistent Cargo cache; do not share between concurrent runs")
    args = parser.parse_args()
    if args.repetitions < 1 or not 1 <= args.idle_seconds <= 3600:
        parser.error("repetitions must be positive; idle seconds must be 1..3600")
    root = args.output.resolve()
    if args.phase == "report":
        summary = report(root)
        if summary.get("failures"):
            raise ValueError(f"report contains {len(summary['failures'])} capture failures")
        return
    spec = plan(args)
    if args.phase == "plan":
        print(json.dumps(spec, indent=2))
        return
    root.mkdir(parents=True, exist_ok=True)
    lock = root / ".runner-lock"
    lock.mkdir()
    try:
        manifest_file = root / "manifest.json"
        if manifest_file.exists():
            manifest = json.loads(manifest_file.read_text(encoding="utf-8"))
            verify_manifest(root, manifest, spec)
        else:
            if any(path.name != lock.name for path in root.iterdir()):
                raise ValueError("output contains unverified artifacts; choose a new directory")
            manifest = build(root, spec, args)
        if args.phase == "build":
            return
        resources = Resources()
        idle_job = Job("idle", "output", "quiet", "tabs", 1, 8)
        smoke_jobs = (Job("output1", "output", "quiet", "active", 1, 8), *JOBS)
        for side in ("before", "after"):
            for job in smoke_jobs:
                collect(root, manifest, job, side, 0, resources, smoke=True)
            collect(root, manifest, idle_job, side, 0, resources, smoke=True, idle=True)
        if args.phase == "smoke":
            return
        for job in JOBS:
            for repeat, order in enumerate(manifest["order"], 1):
                for side in order:
                    collect(root, manifest, job, side, repeat, resources)
        for repeat, order in enumerate(manifest["order"], 1):
            for side in order:
                collect(root, manifest, idle_job, side, repeat, resources, idle=True)
        summary = report(root)
        save(root / "completion.json", {"complete": not summary.get("failures"), "completed_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())})
        if summary.get("failures"):
            raise ValueError(f"measurement incomplete: {summary['failures']}")
    finally:
        lock.rmdir()


if __name__ == "__main__":
    main()
