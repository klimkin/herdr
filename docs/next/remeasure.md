# Rebuild and recollect Linux and Windows measurements

Run these scripts from the checkout containing them. Each creates isolated source
worktrees, builds frozen release binaries, and records paired raw measurements.
No branch checkout, commit, push, or installed Herdr session changes are required.

## Linux

Install Git, Python 3.10 or newer, `just`, Rust 1.96.1, and Zig 0.16.0.
Linux requires `taskset`, `/proc`, and Python/kernel pidfd support for safe cleanup.

```bash
scripts/remeasure_linux.sh \
  --zig /absolute/path/to/zig \
  --output .local/validation/linux-remeasure
```

## Native Windows

Use native x86_64 Windows with Visual Studio Build Tools and its Windows SDK.
Install Git, Python 3.10 or newer, `just`, Rust 1.96.1, and Zig 0.16.0.
Run outside WSL. Hosts with more than 64 logical CPUs need a separate processor
group accounting implementation; this runner rejects them.

```powershell
.\scripts\remeasure_windows.ps1 --zig 'C:\tools\zig-0.16.0\zig.exe' --output 'C:\measurements\herdr'
```

The runner stages the pinned ConPTY package beside both application binaries and
the shared benchmark. It applies CPU affinity and assigns an owned kill-on-close
job before resuming the Windows benchmark process.

## Collect in stages

Choose an empty output directory. Keep arguments identical across stages.

```bash
scripts/remeasure_linux.sh --zig /absolute/path/to/zig --output .local/validation/run --phase build
scripts/remeasure_linux.sh --zig /absolute/path/to/zig --output .local/validation/run --phase smoke
scripts/remeasure_linux.sh --zig /absolute/path/to/zig --output .local/validation/run --phase measure
scripts/remeasure_linux.sh --output .local/validation/run --phase report
```

Use the Windows launcher with the same arguments on Windows. `all`, the default,
performs every stage. `plan` prints inputs without building or measuring.

Completed runs resume only when the plan, source hashes, frozen packages, and
raw evidence hashes match. Interrupted runs retain their evidence and require a
new output directory. A stale `.runner-lock` requires confirming that its runner
has exited before removing that lock. No partial capture enters the completed
dataset. Build worktrees are disposable; Cargo caches remain in the output's
`targets/` directory. `--target-root` selects a persistent cache. Give concurrent
runners separate cache directories.

## Inspect results

Open `report.md` for every distribution and `regressions.md` for observed
increases. `summary.json` retains outcomes, CPU intervals, run statistics, and
validation failures. `comparisons.json` and `regressions.csv` retain all comparison
details. `manifest.json` records frozen refs, tool versions, package/source hashes,
affinity, geometry, and workload order. Raw data remain under `runs/`, `idle/`, and
`smoke/`. Build commands and compiler output remain under `build-logs/`.

The default baseline is `2563803dca97c040beaf3dc3acdcb5a3221b4238`; candidate is
`45c5bb3f7e27630feb47203e2a5a9464f5576dda`. Both commits must exist locally. Fetch
their branches before running if necessary. `--baseline` and `--candidate` accept
full commit SHAs. The candidate supplies production source; this checkout supplies
the shared observer/probe and idle support. The manifest hashes that observer
overlay separately. Both application builds use the original frozen source.

The default matrix collects five reversed pairs. Each side supplies 10,000 echo15
effects, 3,000 effects for each of echo1, rename1, rename15, and hidden15, and five
60-second silent idle windows. Eight-effect output, echo, and rename smokes plus
short idle smokes run first. Geometry is 120 by 40, seed 92341, input spacing 40 to
80 ms, load 60 Hz per producer, and ten warmups excluded. Recording and profiling
remain disabled.

`--cpus 0,1,2,3,4,5,6,7` selects explicit CPU indices; the default selects the first
eight allowed CPUs. `--repetitions` and `--idle-seconds` support shorter local
checks. Short datasets do not establish tail behavior. p99.9 requires at least
10,000 completed observations. Percentiles pool raw samples using nearest rank;
CPU means weight actual interval durations. CPU percentages normalize to one core.

Timing includes input scheduling lag, input-to-echo round trip, intended-to-observed
delay, echo forward/helper/return legs, socket RTT, producer freshness, and visible
generation age. CPU includes server, TUI, combined, benchmark/probe/console costs
within the captured process tree, and idle. Console CPU remains separately labelled.
Whole-run traffic includes setup and warmups. Rename response starts at Enter;
the CPU window also includes modal preparation.

Reproducibility fixes source, tools, workloads, and accounting. Scheduling,
frequency, filesystem paths, and external build configuration can still affect
timings or binary bytes. Keep Windows and Linux reports separate. The endpoint is
committed terminal bytes, excludes physical display latency, and measures one
local server. Sequential process scans can miss short-lived descendants. Preserve
host contention evidence and investigate regressions in separate profiler runs.

## Verify the scripts

```bash
python3 -m unittest scripts.test_remeasure
```

Use `python` on Windows. Native Windows runtime validation remains necessary;
Linux execution and static checks do not prove the Windows collection path.
