# Latency benchmark usage

Run from repository checkout containing commit `5ea85a83` or later. Commands
below use a POSIX shell. Linux paths have been validated; native macOS,
Windows/ConPTY, and controlled SSH validation remain pending.

## Setup and checks

Use repository build prerequisites, including Rust and Zig 0.16.0. On this
machine:

```sh
cd /home/adklimki/e/ai/herdr
export ZIG=/home/adklimki/.local/opt/zig-0.16.0/zig

just latency-check         # Feature clippy and observer/report contracts
just latency-smoke         # Real sessions: outcomes, fanout, recovery, cleanup
just latency-render-scale  # Fixed geometry with 1 and 15 populated panes
```

Checks need `just`, Rust, Python 3, and normal repository build dependencies.
Tracy collectors are needed only for captures.

## Run scenarios

`just bench-latency` builds optimized binaries, starts an isolated server,
creates controlled pane processes, and attaches real TUI clients to owned PTYs.
Each normal run cleans up its server, clients, and temporary configuration.

```sh
# Process -> server -> client
just bench-latency --path output --samples 200 --output .local/latency/output

# Client -> server -> process -> server -> client
just bench-latency --path echo --samples 200 --output .local/latency/echo

# Echo during output from 14 hidden panes plus one foreground pane
just bench-latency --path echo --load hidden --panes 15

# Echo during continuous output from 15 active panes
just bench-latency --path echo --load visible --layout active --panes 15

# Client A -> server -> clients A/B/C, using authoritative workspace rename
just bench-latency --path action --clients 3 --samples 200

# Throttle last client's outer PTY reader throughout run
just bench-latency --path action --clients 3 --slow-reader-ms 120

# Stall last reader, then resume after 500 ms
just bench-latency --path action --clients 3 --stall-ms 500

# Four output updates per burst, 20 bursts/second per active pane
just bench-latency --path echo --load visible --layout active --panes 15 \
  --load-hz 20 --burst 4

# About 60 seconds of scheduled probes, excluding setup and drain
just bench-latency --path echo --load visible --layout active --panes 15 \
  --samples 1100 --warmups 10 --interval-ms 40
```

Action timing starts at Enter confirming a prepared rename. Modal opening and
label entry happen before timing. Completion requires authoritative label in
each client's terminal output. Echo uses a distinct controlled-process response,
with terminal line-discipline echo disabled.

| Option | Default | Meaning |
|---|---|---|
| `--path` | `output` | `output`, `echo`, or `action` |
| `--samples` | `40` | Measured probes, excluding warmups |
| `--warmups` | `10` | Initial probes excluded from response summaries |
| `--clients` | `1` | Number of real TUI observers |
| `--panes` | `1` | Total controlled panes |
| `--load` | `quiet` | `quiet`, `visible`, or `hidden` |
| `--layout` | `tabs` | `tabs` or balanced `active` splits |
| `--load-hz` | `60` | Producer burst frequency |
| `--burst` | `1` | Output updates per producer burst |
| `--interval-ms` | `30` | Seeded probe spacing, randomized between 1x and 2x |
| `--max-pending` | `64` | Maximum outstanding required probes |
| `--slow-reader-ms` | `0` | Delay each read on last client; needs multiple clients |
| `--stall-ms` | `0` | Initial reader stall followed by resume; maximum 10 seconds |
| `--output` | `.local/latency-run` | Parent directory for unique run artifacts |
| `--tracy-port` | `8086` | Server collector port; client 0 uses next port |

Hidden load requires `tabs`; `--panes 1 --load hidden` has no background producer.
Run performance comparisons sequentially, with no concurrent builds or tests.

## Record diagnostics and generate report

Enable JSONL stage records without active Tracy capture:

```sh
HERDR_LATENCY_TRACE_DIR=1 just bench-latency \
  --path echo --load visible --samples 200 --output .local/latency/diagnostic
```

Command prints exact path to `samples.json`. Set `latency_run` to its containing
directory:

```sh
latency_run=.local/latency/diagnostic/REPLACE_WITH_PRINTED_RUN_ID
python3 scripts/latency_report.py "$latency_run/samples.json" \
  --traces "$latency_run/traces" --output "$latency_run/report.md"
```

Artifacts include `samples.json`, raw `client-N.vt`, helper events, server logs,
and optional `traces/PID.jsonl`. Annotated reporting produces `report.md` and
`report.json`. Without stage records, omit `--traces` for response/freshness report.

## Capture Tracy

Use Tracy **0.14.1**, compatible with pinned `tracy-client` **0.19.0**. Build
before starting collectors:

```sh
just latency-build
mkdir -p .local/latency/captures
```

Start collectors in two terminals from repository root. Keep both running while
starting benchmark in a third terminal:

```sh
# Terminal 1: server
tracy-capture -a 127.0.0.1 -p 18086 \
  -o .local/latency/captures/server.tracy -s 30
```

```sh
# Terminal 2: client 0
tracy-capture -a 127.0.0.1 -p 18087 \
  -o .local/latency/captures/client-0.tracy -s 30
```

```sh
# Terminal 3: prebuilt benchmark; no compile delay
HERDR_LATENCY_TRACE_DIR=1 HERDR_TRACY=1 \
  "${CARGO_TARGET_DIR:-target}/release/examples/latency_bench" \
  --binary "${CARGO_TARGET_DIR:-target}/release/herdr" \
  --probe "${CARGO_TARGET_DIR:-target}/release/examples/latency_probe" \
  --path echo --load visible --layout active --panes 15 \
  --samples 400 --interval-ms 40 --tracy-port 18086 \
  --output .local/latency/tracy
```

Additional clients use ports 18088, 18089, and so on. Choose new capture filenames
for repeat runs. Open original `.tracy` files in matching Tracy profiler.
Preserve originals: merging does not synchronize clocks and drops lock,
scheduler, callstack, GPU, allocation, and frame-image evidence.

## Compare profiling modes

Keep copies of equivalent optimized binaries:

```sh
mkdir -p .local/latency/bin
just latency-build ''
cp "${CARGO_TARGET_DIR:-target}/release/herdr" .local/latency/bin/herdr-disabled
just latency-build
cp "${CARGO_TARGET_DIR:-target}/release/herdr" .local/latency/bin/herdr-profiled
```

Use prebuilt benchmark with `--binary .local/latency/bin/herdr-disabled` or
`--binary .local/latency/bin/herdr-profiled`; always supply optimized helper using
`--probe "${CARGO_TARGET_DIR:-target}/release/examples/latency_probe"`.

Compare disabled binary, profiled binary with `HERDR_LATENCY_TRACE_DIR=1`, and
profiled binary with both diagnostic and Tracy variables plus collectors. Clear
inherited profiling variables for disabled run. Repeat in reversed order with
identical samples, geometry, workload, and observers.

## Interpret results

All paths end at complete matching **committed terminal bytes**, including host
scheduling and observer cost. They measure no physical pixels or scanout.

Response summaries retain missing effects and diagnostic deadline misses;
20 ms threshold is not a production SLO. Percentiles use nearest rank; p99.9
requires at least 10,000 completions. Streaming supersession requires evidence
of newer observed generations; undelivered required echoes/actions remain missing.

Same-host monotonic clocks are checked explicitly. Matched socket RTT remains
separate from application response. Missing or ambiguous causal stages stay
unavailable. Off-CPU attribution and PTY submission queue residence are unassigned;
overlapping intervals must not be summed.

Detailed background: [latency measurement documentation](website/src/content/docs/latency-measurement.mdx).

## Server deadline waiting

Linux uses a reusable absolute one-shot `timerfd` for server-loop deadlines.
Incoming events cancel the pending timer; idle waits remain event-driven.
The presentation interval stays 16 ms. This reduces timer rounding delay;
it does not guarantee an end-to-end maximum latency.

If the native timer cannot initialize or fails, the server logs a warning
and uses Tokio deadline sleeping. macOS and Windows retain Tokio sleeping;
native high-resolution implementations remain out of scope.
