# Custom metrics and providers (protocol v1)

winstats can bind numeric, history-chart and boolean-state widgets to dynamically registered
metrics. Built-in metrics are available under `builtin/` (catalog below). Other data sources
run as child processes and publish UTF-8 newline-delimited JSON over stdout; host commands
arrive on stdin. No listening port, shared DLL ABI or particular provider language is required.

Existing CPU/GPU/etc. widgets and their specialised popups remain supported. Custom metrics
are **not yet forwarded through the remote agent/share protocol**. Local hardware discovery,
sampling, popup details and collector reconfiguration run on a dedicated worker. The UI
consumes complete snapshots; external provider startup, pipe I/O, network requests and
process cleanup also run off the UI thread.

## Built-in bindings

| Metric | Unit | Value |
| --- | --- | --- |
| `builtin/cpu/usage` | `%` | Total CPU load, 0–100 |
| `builtin/memory/used` | `B` | Used physical memory |
| `builtin/memory/usage` | `%` | Physical memory usage, 0–100 |
| `builtin/network/rx` | `B/s` | Download over enabled interfaces |
| `builtin/network/tx` | `B/s` | Upload over enabled interfaces |
| `builtin/disk/read` | `B/s` | Read throughput over enabled disks |
| `builtin/disk/write` | `B/s` | Write throughput over enabled disks |
| `builtin/gpu/usage` | `%` | Load from selected GPU adapters, 0–100 |
| `builtin/battery/level` | `%` | Battery level, 0–100 |

Bind any of these with `[custom_widgets.<alias>]`, just like an external provider metric.
They use the same snapshot as the existing specialised widgets. Rates remain in bytes per
second regardless of the specialised network widget's display-unit setting. Generic numeric
widgets show the descriptor's raw unit (they do not yet auto-scale bytes to GiB).

The worker produces sequence numbers and timestamps once per collection, with TTL equal to
three times `update_ms` (minimum one second). UI redraws never renew freshness. Missing GPU,
battery or disk sources produce error samples rather than fabricated zeroes. A stalled worker
leaves its last values to expire; an exited/panicked worker marks the source offline. Release
builds use Rust panic unwinding so a worker panic does not abort the UI; native process crashes
cannot be contained by a thread. Reload config can restart a finished worker.

Both mailboxes have capacity one: the newest complete snapshot replaces an unread snapshot,
and the newest complete desired settings replace pending settings. Popup/adapter/filter
changes wake the worker. Reload reuses a running worker and ignores snapshots from its previous
configuration; startup, reload and shutdown do not wait for hardware calls. A native call that
hangs indefinitely cannot be cancelled; its one worker remains occupied until the call returns
or the application exits. All built-ins currently share that worker, so a slow driver can delay
other built-in samples while the UI and external providers continue independently.

Specialised widgets retain their existing last-value behaviour; freshness/error diagnostics
are shown by generic widgets. Lower-level Windows collectors retain their existing handling
of individual counter failures; this change does not add per-counter diagnostic reporting to
every Windows API. `--diag` intentionally uses synchronous collection in its non-UI process.

## Configure

Merge [the example configuration](../examples/providers/config.toml) into
`%APPDATA%\winstats\config.toml`, set `providers.website.cwd` to the example directory, and use
**Reload config**. The existing `widgets = ["cpu_mini", ...]` format stays compatible:
`custom:<alias>` refers to an entry in `[custom_widgets.<alias>]`.

```toml
widgets = ["cpu_mini", "custom:web", "custom:cpu_history"]

[custom_widgets.web]
metric = "website/homepage/latency"
view = "number"                 # number | line | state
label = "WEB"                   # optional; defaults to the registered metric name
color = "teal"                  # palette name or #RRGGBB; default is the accent
keep_history = false            # true subscribes even while the widget is hidden

[custom_widgets.cpu_history]
metric = "builtin/cpu/usage"
view = "line"
```

A provider's config has `enabled`, `command`, `args`, `cwd`, `interval_ms` and `heartbeat_ms`.
See the full example for values. Arguments are passed directly, without a shell. Relative
`cwd` is resolved against the configuration directory, and defaults to that directory.
Executable paths containing a directory component resolve against `cwd`; bare commands use
the process PATH. Providers must resolve their own relative file arguments against `cwd`.

The **Custom metrics** menu toggles configured widgets and their vertical chart stretching.
The menu lists at most 128 custom widget definitions and 64 data sources. At most 64 provider
configurations are considered (in key order); each provider can declare up to 256 metrics.
Its **Data sources** submenu enables/disables configured providers. Dragging reorders custom
widgets alongside the existing widgets. Click a custom widget for its value, freshness,
last-success age, source diagnostics and a 60-second chart. Error/expired/offline values are
dimmed and marked `!`; the last successful value remains available in the popup.

Hiding all widgets for a metric removes its subscription unless `keep_history` is set.
An enabled provider remains alive and sends heartbeats while idle. Disabling a provider stops
its entire process tree and marks its metrics offline; enabling it starts a new session.
Reloading config restarts local providers and clears custom metric history.

## HTTP example

[`http.ps1`](../examples/providers/http.ps1) runs on Windows PowerShell 5.1 or PowerShell 7.
The example sets the child process's execution policy to `RemoteSigned`; it does not change
the user's or system's policy. Domain-enforced policy still applies. PowerShell 5.1 otherwise
uses a restrictive default on some systems.

Edit [`targets.json`](../examples/providers/targets.json) with unique `id`, display `name` and
HTTP(S) `url` per target. Each target declares `<id>/latency` (milliseconds to response
headers) and `<id>/up` (a boolean indicating HTTP 2xx). Requests to different targets are
concurrent; at most one request per target is in flight. A slow request skips overlapping
scheduled attempts. The default request timeout is 3 seconds, interval is 5 seconds and metric
TTL is 15 seconds. Increase the TTL in the provider if you choose a longer interval.

Failures produce an error sample for latency and `false` for reachability; they do not create
a zero latency. Completing one target never waits for the others. Heartbeats continue while
requests are pending. Unsubscription cancels requests that no longer have any subscribers.

## Protocol

Each line, including its newline, is limited to 64 KiB. JSON is UTF-8 without a BOM; stdout
must contain only protocol messages. stderr is drained continuously and only its last 2 KiB
is retained for failure diagnostics (the UI displays a shortened reason). Don't put secrets
in diagnostics. Metric ids use ASCII letters, digits, `_`, `-`, `.` and `/`; empty, `.` and
`..` path components are rejected. Provider ids are a single component; `builtin` is reserved.
The host prefixes a metric with `<provider-id>/` to avoid collisions.

1. Host sends `{"t":"hello","v":1}`.
2. Provider replies `{"t":"hello","v":1}`, followed by one complete catalog:

   ```json
   {"t":"describe","metrics":[{"id":"homepage/latency","name":"Website latency","unit":"ms","value_type":"number","ttl_ms":15000,"min":0}]}
   ```

   `value_type` is `number` (default) or `boolean`; `unit`, `min`, `max` are optional. Name,
   unit, TTL and bounds are validated. A session can declare at most 256 metrics. Declaration
   does not start collection. Changing a catalog requires a new process session.

3. Host sends the complete desired subscription set:

   ```json
   {"t":"subscribe","metrics":[{"metric":"homepage/latency","interval_ms":5000}]}
   ```

   An empty array pauses all collection. Multiple widgets share one subscription. The
   provider owns scheduling, request deadlines, concurrency and cancellation. It may use an
   async runtime or worker threads; the host does not wait for a collection batch.

4. Provider sends results as each becomes ready:

   ```json
   {"t":"sample","metric":"homepage/latency","seq":1,"sampled_at_ms":1792454400000,"value":42.5}
   {"t":"sample","metric":"homepage/latency","seq":2,"sampled_at_ms":1792454405000,"value":null,"error":"request timed out"}
   ```

   Use the actual Unix millisecond timestamp, not the example timestamp. Each metric's `seq`
   must strictly increase within a session and sample times must not go backwards. Samples
   over one day old or more than five seconds in the future are rejected. Old results
   are ignored. Exactly one of a typed value and an error is required. Non-finite numeric
   values, invalid types and undeclared metrics are rejected; unsubscribed samples are
   ignored. A boolean value is a JSON `true` or `false`, not `0` or `1`.

5. Provider sends `{"t":"heartbeat"}` at least once per second, including when idle. Samples
   and other valid messages also count as activity. Host sends `{"t":"stop"}` on shutdown
   when possible; providers must also exit on stdin EOF. Stop delivery is best effort: the
   host terminates the process tree without waiting on the UI thread.

Handshake/catalog must complete within 5 seconds. Heartbeat timeout defaults to 15 seconds.
Exits, invalid protocol, broken pipes and heartbeat timeouts trigger restart with exponential
backoff (1–30 seconds). A session alive for 30 seconds resets backoff. Queues, frame sizes,
metric catalogs and histories are bounded; the host drains a fixed budget per UI tick.
Windows Job Objects terminate descendant processes when a provider is disabled, reconfigured,
or the host exits. A failure to attach a Job Object prevents the provider from running.

Metric freshness accounts for delivery delay and then uses a monotonic clock so wall-clock
changes don't extend an existing value's lifetime. Heartbeats indicate process health, not
fresh metric values. Charts use
sample times, keep at most 240 points per metric, and break at errors, disconnections and
expired intervals. UI redraws never add samples. A chart autoscales unless bounds are declared.

## Verification

```powershell
cargo +1.77.0 test --workspace --locked
cargo +1.77.0 build --release --workspace --locked
```

The provider tests launch real PowerShell processes. They check independent fast/slow
results, nonblocking polling, malformed/oversized output, process exit, heartbeat timeout,
and descendant cleanup on disable. An HTTP integration test uses two local endpoints with
a fast response and a delayed response to verify the example's asynchronous requests and
timeouts without relying on an external website.

Compatibility compilation uses the project's Rust 1.77 toolchain. Actual runtime verification
must be recorded separately for Windows versions and processor architectures beyond the
development environment.

For native UI automation, `winstats.exe --ui-test` exposes the floating strip and its
detail popup as ordinary selectable windows and gives the context menu the strip as
its owner. This is an acceptance aid: normal launches keep their existing taskbar and
tool-window behavior. Use an isolated `APPDATA` directory for acceptance configuration.
Full-desktop captures are needed to check menu placement and taskbar integration; a
window-only capture may omit these transient windows. See the
[2026-10-20 acceptance record](acceptance/providers-2026-10-20.md) for tested scope.
