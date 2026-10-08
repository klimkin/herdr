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
  -o .local/latency/captures/server.tracy
```

```sh
# Terminal 2: client 0
tracy-capture -a 127.0.0.1 -p 18087 \
  -o .local/latency/captures/client-0.tracy
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

Collectors save after their runtime disconnects. Wait for successful save
completion before inspecting files. A fixed collector cutoff can truncate the
measurement interval.

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
unavailable. CPU versus off-CPU attribution remains unassigned. Accepted
input-actor records can establish PTY submission queue residence when all contributing command and
part boundaries are complete; missing boundaries remain unassigned. Overlapping
intervals must not be summed.

For local fanout, diagnostics retain each queued occurrence and framed-byte
position through socket write, receipt, and client output. Native peer PIDs link
one captured server connection to one captured client connection, allowing
identical payloads to reach distinct clients without relying on hash uniqueness
or receipt order. This requires complete, lossless records for both processes
and that client's completed outer-PTY observation. Reconnects, multiple sockets
between the same PID pair, missing records, and unavailable peer credentials
remain unassigned. Windows currently lacks this native mapping; SSH bridge peer
PIDs do not prove a remote destination. Published wire bytes stay unchanged.

Schema-2 diagnostics carry explicit presentation, retained/full attempt,
serialization, runtime-instance and service-attempt identities beside the existing
connection/queue identities. Retained fallback and its subsequent full attempt
remain distinct. Runtime replacement cannot inherit an earlier reader's revision
links. Missing modern identities remain unassigned. Published wire bytes do not
carry these fields.

`report.json` includes:

| Field | Use |
|---|---|
| `critical_paths`, `stage_report` | Per-response service and presentation chain, stage coverage and response accounting |
| `event_service` | Offered/admitted/received populations, service attempts, dispatch links, queue-residence bounds and tracing coverage |
| `wait_service` | Selected branches, real deadline reasons, overdue time and invalid/unmatched wait records |
| `runtime_work` | Complete-message batches, nested API barriers, notifications and accept/handshake stages |
| `input_report` | Accepted command/part intervals without multiplying input samples by client fanout |
| `trace_audit` | Expected processes, record integrity, drops and final flush |

Whole-handler and barrier spans overlap. Sender return can follow consumer
service, so publication-to-service residence is an interval. A selected future
return is separate from a scheduler wakeup or context switch. Untraced internal
producers limit whole-lane oldest-age attribution.

Detailed background: [latency measurement documentation](website/src/content/docs/latency-measurement.mdx).

## Server deadline waiting

Linux uses a reusable absolute one-shot `timerfd` for server-loop deadlines.
Incoming events cancel the pending timer; idle waits remain event-driven.
The presentation interval stays 16 ms. This reduces timer rounding delay;
it does not guarantee an end-to-end maximum latency.

If the native timer cannot initialize or fails, the server logs a warning
and uses Tokio deadline sleeping. macOS and Windows retain Tokio sleeping;
native high-resolution implementations remain out of scope.

## Early-action presentation experiment

Linux supports an opt-in `action-full` policy for committed workspace renames.
An eligible action can request a coherent full frame before ordinary cadence.
The experiment allows at most one extra attempt per 16 ms, expires unused
opportunities after 16 ms, and creates no refill or expiry timer. Ordinary
presentation clocks remain unchanged. Renames that change no state receive no
opportunity; unrelated terminal output alone cannot grant one.

The default remains `ordinary`. Screening found active-output action p95
54–70% lower, but quiet latency, CPU, and one output-freshness result exceeded
the experiment's gates. Keep `action-full` experimental; these results do not
justify enabling it by default or claiming an idle CPU benefit.

Build once with selectors enabled and recording disabled, then compare both
policies using the same binary, geometry, workload, and seed:

```sh
just latency-build latency-experiments
latency_artifacts="${CARGO_TARGET_DIR:-target}/release"

for latency_policy in ordinary action-full; do
  env -u HERDR_LATENCY_TRACE_DIR \
    HERDR_LATENCY_PRESENTATION="$latency_policy" \
    HERDR_LATENCY_QUEUE_ORDER=current HERDR_LATENCY_QUEUE_COUNT=64 \
    "$latency_artifacts/examples/latency_bench" \
    --binary "$latency_artifacts/herdr" \
    --probe "$latency_artifacts/examples/latency_probe" \
    --path action --clients 3 --samples 1100 --warmups 10 --interval-ms 40 \
    --load visible --layout active --panes 15 \
    --output ".local/latency/action-$latency_policy"
done
```

The benchmark records its fixed seed. Repeat pairs in reversed order.
Capture required outcomes and CPU coverage;
successful-response percentiles alone cannot establish acceptance. The default
build rejects `action-full`; macOS and Windows reject this experiment. Queue
selectors currently accept only `current` and `64`.

For causal diagnostics, build with `just latency-build` and set
`HERDR_LATENCY_TRACE_DIR=1` for the benchmark. After the run finishes, use the
printed run directory and place derived reports outside its raw artifacts:

```sh
latency_run=.local/latency/action-diagnostic/REPLACE_WITH_PRINTED_RUN_ID
mkdir -p .local/latency/reports
python3 scripts/latency_action_report.py "$latency_run/samples.json" \
  --traces "$latency_run/traces" \
  --output .local/latency/reports/action.json
```

The action report separates an opportunity's grant, admission, and coherent
enqueue from client output completion. `critical_paths` carries completed
server/client paths; an `early_enqueued` opportunity alone does not prove a
committed client presentation. Missing identities remain unassigned.

## Early terminal-feedback experiment

Linux also supports `target` and `target-all` with the `latency-experiments`
feature. Accepted nonempty terminal input creates one opportunity for that
terminal/runtime. Its original 16 ms expiry remains fixed across coalesced
input. Successful ordinary delivery advances the presented revision floor and
preserves the unused early allowance. Later output newer than that floor can
still request early presentation, including output arriving during enqueue.
Changed target state can spend one global extra attempt
per 16 ms. Unchanged state creates no early frame; empty/rejected input and
release cleanup create no opportunity. Successful delivery after an early
attempt retires the opportunity; failed attempts receive no refund.
One successful recipient can retire a spent window while other recipients have
deferred queues. Those recipients retain ordinary recovery and can still wait
for its cadence.

`target` uses a coherent retained update for the selected terminal. Residual
sources, titles, and generic work keep their ordinary deadline. Unsafe retained
state falls back to ordinary scheduling. `target-all` is a diagnostic comparator
that can carry unrelated dirty sources in a full frame after target readiness.
Neither policy changes the ordinary presentation interval or adds an expiry,
refill, or idle timer. macOS and Windows keep native experiment support disabled.
The default stays `ordinary`; queue selectors remain `current` and `64`.

`action-full+target` enables both `action-full` and `target` together. Action
and terminal opportunities stay separate, but they share the single global
extra attempt per 16 ms, so the combination never adds a second early frame
within one interval. An early action frame is a full frame and also carries any
ready target echo.

Compare both policies with the same recording-disabled binary:

```sh
just latency-build latency-experiments
latency_artifacts="${CARGO_TARGET_DIR:-target}/release"

for latency_policy in ordinary target; do
  env -u HERDR_LATENCY_TRACE_DIR \
    HERDR_LATENCY_PRESENTATION="$latency_policy" \
    HERDR_LATENCY_QUEUE_ORDER=current HERDR_LATENCY_QUEUE_COUNT=64 \
    "$latency_artifacts/examples/latency_bench" \
    --binary "$latency_artifacts/herdr" \
    --probe "$latency_artifacts/examples/latency_probe" \
    --path echo --clients 3 --samples 1100 --warmups 10 --interval-ms 40 \
    --load visible --layout active --panes 15 \
    --output ".local/latency/target-$latency_policy"
done
```

Repeat at least three pairs in reversed order. Keep all required response
outcomes, actual producer progress, per-client latency, and server/client CPU.
The local screen completed all 79,200 outputs and reduced active-15 p95 by
67–80%, but summed client CPU exceeded the experiment's component limit in all
active pairs. Active-1 median improved while p95 stayed near ordinary cadence.
These results support further experiments; they do not justify default promotion
or an idle CPU claim. Recording-on diagnostic latency differed materially from
recording-disabled runs, so use diagnostics to explain mechanisms and separate
runs to judge operating cost.

For target diagnostics, build with `just latency-build`, enable
`HERDR_LATENCY_TRACE_DIR=1`, and pass the printed run's PID-named trace files:

```sh
python3 scripts/latency_target_report.py "$latency_run"/traces/[0-9]*.jsonl \
  > .local/latency/reports/target.json
```

The target report preserves accepted, empty, rejected, coalesced, and unassigned
input outcomes. `deliveries` separates ordinary floor advances from opportunity
retirement; unused windows remain pending until expiry or cancellation.
Exact runtime/revision/recipient/serialization receipts establish
which target state reached a queue. A receipt alone does not prove controlled
process echo or every client's committed output. Use the regular latency report
and owned-PTY outcomes for those boundaries. An earlier extra attempt can still
consume urgency before a later echo; delayed-response tests must retain that
outcome rather than extend expiry to hide it.
