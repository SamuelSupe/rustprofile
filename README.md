# rustprofile

[English](README.md) | [简体中文](README.zh-CN.md)

![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange?logo=rust)
![Linux](https://img.shields.io/badge/platform-Linux%205.4%2B-blue?logo=linux)
![License](https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-green)
[![Latest release](https://img.shields.io/github/v/release/SamuelSupe/rustprofile?label=release)](https://github.com/SamuelSupe/rustprofile/releases)

`rustprofile` is a Linux command-line profiler for an already-running native
process, Docker container, or Kubernetes application container. It records
per-thread CPU samples, optional off-CPU intervals, and sampled allocator
activity into gzip-compressed pprof profile protobufs, plus a JSON diagnostics
file for every time window. `launch` can start a command suspended and attach
before it runs; `import` converts existing perf.data/simpleperf captures; and
`serve` exposes a Firefox profile with symbol/source/assembly APIs. When
configured, the same completed windows are also exported as OTLP Profiles over
HTTP/protobuf; local files remain the authoritative record.

[发行记录](CHANGELOG.md) · [GitHub Releases](https://github.com/SamuelSupe/rustprofile/releases)

This README describes the implemented CLI and its limits. Validation claims
are limited to the explicitly listed evidence; no broader production-kernel,
allocator, or deployment claim is implied.

## Requirements and build

- Linux 5.4 or newer for CPU profiling. Heap profiling requires Linux 5.12 or
  newer. On Linux 5.4-5.11, request `--profiles cpu`; a request containing
  heap fails unless `--allow-partial` can retain another requested profile.
  `check`, `record`, and `launch` require the invoking process to run as root;
  `import` and `serve` are user-space workflows.
- A native 64-bit x86_64 or aarch64 build. The target process architecture
  must match the profiler architecture.
- Rust/Cargo 1.88 or newer (the crate declares `rust-version = "1.88"`).
- A Linux build environment with clang/LLVM and Linux UAPI headers, including
  the architecture-specific include directory used by `build.rs`
  (`/usr/include/x86_64-linux-gnu` or `/usr/include/aarch64-linux-gnu`). Cargo
  builds the libbpf/libbpf-cargo dependencies and compiles the embedded eBPF
  programs.
- Docker targets require access to the Docker Engine API socket and a profiler
  container with host PID visibility and the privileges needed by perf/eBPF.
- Kubernetes targets require running from the node-local profiler DaemonSet
  supplied in `deploy/kubernetes/rustprofile.yaml`. It uses `hostPID`, a
  privileged security context, a read-only Pod RBAC rule, and the in-cluster
  ServiceAccount token/CA.

Build on the Linux target that will run the profiler:

```sh
cargo build --release
```

The resulting binary is `target/release/rustprofile`.

## Quick start

Inspect a process before recording it:

```sh
sudo target/release/rustprofile check --pid "$PID"
sudo target/release/rustprofile check --pid "$PID" --json > check.json
```

Record the default CPU and heap profiles for ten minutes, publishing one
60-second window at a time:

```sh
sudo target/release/rustprofile record \
  --pid "$PID" \
  --duration 10m \
  --window 60s \
  --output ./profiles
```

Use `--duration 0` to record until the target exits or the recorder receives
SIGINT/SIGTERM.

Start a new command suspended, attach the collectors, and then continue it.
On cgroup-v2 hosts this also follows descendants for CPU and off-CPU data:

```sh
sudo target/release/rustprofile launch \
  --profiles cpu,off-cpu \
  --firefox-profile json \
  --duration 10m --window 60s --output ./profiles \
  -- ./my-api --port 8080
```

Convert an existing capture without attaching to a process:

```sh
target/release/rustprofile import \
  --input perf.data --format auto --window 60s \
  --firefox-profile jslb --output ./imported
```

Serve a Firefox processed profile and the symbol/source/assembly endpoints
used by Firefox Profiler/Samply-compatible clients. Loopback listeners do not
need a token; a non-loopback listener requires one:

```sh
target/release/rustprofile serve \
  --profile ./profiles/firefox-session-000000-123.json.gz \
  --listen 127.0.0.1:8080
```

Serve all Firefox windows in a directory with the built-in gallery:

```sh
target/release/rustprofile serve \
  --directory ./profiles \
  --listen 127.0.0.1:8080
```

The same commands can select a running Docker container by ID or name:

```sh
sudo target/release/rustprofile check --docker-container my-api --json
sudo target/release/rustprofile record \
  --docker-container my-api \
  --duration 10m --window 60s --output ./profiles
```

From a profiler Pod on the same Kubernetes node as the target Pod, select a
Pod as `NAMESPACE/NAME`. A single application container can omit `--container`;
multi-container Pods must name the application container explicitly:

```sh
kubectl exec -n rustprofile-system "$PROFILER_POD" -- \
  rustprofile check --k8s-pod default/api --container app --json
kubectl exec -n rustprofile-system "$PROFILER_POD" -- \
  rustprofile record --k8s-pod default/api --container app \
  --duration 10m --window 60s --output /profiles
```

## Commands and options

### `check`

```text
rustprofile check (--pid PID | --docker-container ID_OR_NAME |
                   --k8s-pod NAMESPACE/NAME [--container NAME]) [--json]
                   [--symbol-dir DIR]... [--debuginfod URL]
```

`check` reports the executable, architecture, kernel release/support, root
status, thread count, mapped-module build IDs/unwind sections/symbol counts,
and the selected allocator family. It also performs the perf access,
lifecycle eBPF, off-CPU eBPF, and eligible heap eBPF load probes used by the
preflight path; it does not start a recording or attach the allocator uprobes
used by `record`.

Exactly one target selector is required. `--pid` selects a host process,
`--docker-container` selects a Docker container by ID or name, and `--k8s-pod`
selects a Kubernetes Pod in `NAMESPACE/NAME` form. `--container` is only valid
with `--k8s-pod` and is required when the Pod has multiple application
containers.

`--json` emits schema version 3 (`CheckReport`) with `target`, `capabilities`,
`warnings`, and `errors`. Capabilities include perf/lifecycle/heap/off-CPU
probe status, container cgroup discovery, and observed perf-map/jitdump files.
An error makes the command exit non-zero and prevents `record` from starting.
`--symbol-dir` is repeatable; `check` verifies each directory exists. The
`--debuginfod` value is validated as an `http://` or `https://` URL by
`check`; `check` itself does not fetch debug information.

### `record`

```text
rustprofile record (--pid PID | --docker-container ID_OR_NAME |
                    --k8s-pod NAMESPACE/NAME [--container NAME]) [OPTIONS]
```

| Option | Default and accepted values |
| --- | --- |
| `--pid PID` | Existing host process ID; mutually exclusive with the container selectors. |
| `--docker-container ID_OR_NAME` | Docker container ID or name; mutually exclusive with the other selectors. |
| `--k8s-pod NAMESPACE/NAME` | Kubernetes Pod selector; requires in-cluster execution and is mutually exclusive with the other selectors. |
| `--container NAME` | Kubernetes application container name; only with `--k8s-pod`, required for multi-container Pods. |
| `--profiles LIST` | `cpu,heap`; comma-delimited `cpu`, `heap`, and/or `off-cpu` (duplicates are removed). |
| `--duration DURATION` | `60s`; humantime syntax, or `0` for no deadline. |
| `--window DURATION` | `60s`; must be greater than zero. The final window may be shorter. |
| `--unwind MODE` | `auto`; `auto`, `fp`, or `dwarf`. |
| `--cpu-frequency HZ` | `49`; 1 through 999 target-CPU samples per second. |
| `--alloc-interval BYTES` | `512 KiB` (524288 bytes); positive byte-size syntax. |
| `--allocator FAMILY` | `auto`; `auto`, `rust`, or `system`. |
| `--output DIR` | `.`; directory for profile and diagnostics windows. |
| `--keep-windows N` | `60`; positive number of windows retained for this recording session. |
| `--max-stacks N` | `65,536`; maximum distinct stacks in each CPU, off-CPU, or heap output window. Existing stacks continue accumulating; new stacks after the cap are omitted. |
| `--max-pending-events N` | `262,144`; bounded timestamp-ordering buffer for CPU perf events. |
| `--event-reorder-window DURATION` | `100ms`; maximum timestamp skew tolerated while ordering CPU perf events. It cannot exceed `--window`; off-CPU uses a separate bounded interval queue. |
| `--max-timeline-samples N` | `65,536`; maximum timestamped samples retained for each Firefox output or OTLP timeline window. Excess samples are omitted from the enabled timeline output and counted in diagnostics. |
| `--otlp-timeline` | Off; with OTLP enabled, send the bounded timestamped CPU timeline instead of an aggregated CPU source. The local CPU pprof remains available. |
| `--firefox-profile FORMAT` | Off; also write one per-window Firefox processed profile as `json` or `jslb`. |
| `--svg` | Off; also write self-contained static SVG flame graphs for completed CPU, off-CPU, and heap windows. |
| `--allow-partial` | Off by default; permit supported subsets or leaf-only CPU data (see below). |
| `--symbol-dir DIR` | Repeatable additional symbol/debug-file search directory. |
| `--debuginfod URL` | No network lookup unless explicitly supplied. |
| `--otlp-endpoint URL` | Optional OTLP/HTTP Profiles endpoint; no export when omitted. |
| `--otlp-header KEY=VALUE` | Repeatable OTLP HTTP header. Header values are not written to diagnostics. |
| `--otlp-timeout DURATION` | Timeout per OTLP attempt; environment values use milliseconds; default `10s`. |
| `--otlp-compression none\|gzip` | Request compression; default `gzip`. |
| `--otlp-ca PATH` | Additional PEM CA file for the OTLP HTTPS endpoint. |
| `--resource-attribute KEY=VALUE` | Repeatable OTLP resource attribute. |

There is also a hidden `--max-threads` perf-backend setting (default 1024).
The preflight check still rejects a target over the default 1024-thread limit.

### `launch`, `import`, and `serve`

`launch` accepts the same collection options as `record`, followed by a
command. The child is stopped with `SIGSTOP` before preflight and collector
attachment, then resumed. On cgroup v2, rustprofile creates a short-lived
launch cgroup and moves the child into it before resuming, so descendants are
included in CPU/off-CPU collection. If cgroup creation is unavailable,
`--allow-partial` falls back to the exact child PID. Heap probes currently
remain scoped to the launched container/init process; diagnostics expose this
as `mixed_process_and_cgroup` when applicable.

`import --input PATH` reads a regular Linux `perf.data` or simpleperf capture
using the timestamped samples and callchains it contains. `--format auto` is
the default; `perf-data` and `simpleperf` are explicit hints. Imported pprof
profiles preserve raw addresses and PID/TID labels, while `--firefox-profile`
also emits a per-window processed profile. `--max-stacks` bounds distinct
attributed stacks per imported window, and `--max-timeline-samples` bounds its
Firefox timeline; at most four timestamp windows are kept pending, and thread
state is capped at 65,536 PID/TID pairs. Import does not attach probes or
perform live DWARF unwinding.

`serve` requires exactly one of `--profile PATH` and `--directory DIR`.
`--profile` serves the selected Firefox JSON/JSLB gzip file at
`GET /profile.json` (`application/json` for JSON and
`application/octet-stream` for JSLB); `--directory` scans at most 16,384
directory entries named `firefox-*.json.gz` or `firefox-*.jslb.gz` and returns
at most 4,096 profiles in the built-in gallery at `GET /`. The gallery lists
windows at `GET /api/profiles` and decodes a selected window at
`GET /api/profile/{sha256-filename-id}`. `GET /healthz` remains available.
Compressed input is capped at 512 MiB and decompressed profile data at 128 MiB;
diagnostics larger than 1 MiB are ignored. Viewer samples/stacks are capped at
65,536, functions at 262,144, and threads at 4,096. Use `--symbol-dir` and
explicit `--debuginfod` to provide debug files; POST bodies are capped at 8 MiB
and responses at 32 MiB. CORS is disabled by default; `--cors-origin ORIGIN`
enables an exact origin and its preflight (otherwise `OPTIONS` is 405).
Non-loopback listeners must set `--bearer-token`.

## Unwinding and automatic fallback

`fp` uses the kernel's user stack capture and validates every captured address
against executable mappings. `dwarf` captures registers plus up to 16 KiB of
user stack and unwinds with the target's ELF unwind information. A stack is not
counted as usable when it is empty, cyclic, outside executable mappings, or
otherwise fails validation. Individual samples can be truncated at 127 frames.

`auto` first runs a frame-pointer calibration, for up to ten seconds and until
64 samples are available. It accepts FP only when all of these hold:

- at least 64 samples;
- at least 90% of captured addresses are in executable mappings;
- at least 70% of samples reach three frames; and
- no sample contains an address cycle.

If calibration fails and the target has `.eh_frame` or `.debug_frame`, the
recorder starts in DWARF mode and records the rejection reason in diagnostics.
Without unwind tables, calibration failure is fatal unless `--allow-partial`
was supplied. In partial mode, failed DWARF unwinds may emit a leaf instruction
pointer rather than a full stack.

An `exec` observed during this initial calibration refreshes preflight and
restarts calibration for the new image; it is not treated as an automatic
calibration failure. A target exit during calibration remains fatal.

Even after a successful FP calibration, `auto` switches permanently to DWARF
when a completed window has fewer than 90% usable stacks (the minimum of CPU
and heap stack quality when both are collected). The switch is recorded in
`cpu.fallback_reason`. Heap probes are reattached for this transition, so
heap in-use state restarts at that boundary.

Explicit `--unwind fp` never performs this fallback. Explicit `--unwind dwarf`
requires unwind information unless partial leaf-only output is enabled.

## Heap sampling and allocator support

Heap collection attaches uprobes to allocator symbols in the target process.
`--alloc-interval` is a mean allocation interval: the eBPF program probabilistically
keeps smaller allocations and applies a power-of-two weight so exported counts
and bytes remain sampled estimates. The BPF live-sample map is bounded; drops,
evictions, unfinished returns, and stack failures are reported in diagnostics.

The supported allocator families are deliberately narrow:

- `rust`: the complete Rust shim symbol family
  `__rust_alloc`, `__rust_alloc_zeroed`, `__rust_realloc`, and
  `__rust_dealloc`.
- `system`: a mapped glibc/libc or dynamic musl (`ld-musl`) module whose ELF
  symbols include a complete, defined `malloc`, `calloc`, `realloc`, and
  `free` family. A target executable containing that same complete defined
  family is also accepted, which covers a statically linked system layer.
  When present, `aligned_alloc` and `posix_memalign` are additionally probed.

`auto` prefers the Rust family and falls back to the supported mapped libc.
Custom allocators, arbitrary shared libraries, and allocator APIs without
these symbols are unsupported. Selecting an unavailable family fails preflight
unless `--allow-partial` is used.

Heap `alloc_objects`/`alloc_space` are weighted sampled allocations observed in
the current window. `inuse_objects`/`inuse_space` are weighted sampled
allocations still live at the window snapshot. They include only allocations
observed after rustprofile attached; allocations that were already live at
attach are unknown and excluded. The heap pprof carries the same statement in
its comment field, and diagnostics set `heap.since_attach` to `true`.

Heap state is cleared when the target executes a new image or when probes are
reattached after an FP-to-DWARF transition. Therefore in-use values restart at
those boundaries as well.

## Output files and retention

Each completed window writes the following files under `--output`:

```text
cpu-<session-id>-<window-index>-<start-unix-nanos>.pb.gz
cpu-<session-id>-<window-index>-<start-unix-nanos>.svg       # with --svg
heap-<session-id>-<window-index>-<start-unix-nanos>.pb.gz
heap-<session-id>-<window-index>-<start-unix-nanos>.svg       # with --svg
off-cpu-<session-id>-<window-index>-<start-unix-nanos>.pb.gz
off-cpu-<session-id>-<window-index>-<start-unix-nanos>.svg       # with --svg
firefox-<session-id>-<window-index>-<start-unix-nanos>.json.gz
firefox-<session-id>-<window-index>-<start-unix-nanos>.jslb.gz
diagnostics-<session-id>-<window-index>-<start-unix-nanos>.json
```

The CPU profile has `samples/count` and `cpu/nanoseconds` sample values. The
heap profile has `alloc_objects/count`, `alloc_space/bytes`,
`inuse_objects/count`, and `inuse_space/bytes`; its default sample type is
`inuse_space` and its period is the allocation interval. The profiles are
gzip-compressed pprof protobufs. Samples carry `process.pid` and, for a
container target, the container/Kubernetes identity labels. Diagnostics JSON
is schema version 3 and includes session/PID/timestamps, the structured
`target` metadata, requested and written profile kinds, output paths, warnings,
and a structured top-level `allocator_probe` report (`requested`, `detected`,
`module`, `complete`, and `reason`). CPU diagnostics also expose
`cpu_nanoseconds` (the exported CPU nanosecond total) and `malformed_samples`
separately from `lost_samples`, plus `aggregation_dropped_samples` and
`aggregation_dropped_nanoseconds` for valid CPU samples omitted after
`--max-stacks` was reached. Heap diagnostics expose the four exported
totals—`alloc_objects`, `alloc_space`, `inuse_objects`, and `inuse_space`—alongside
`aggregation_dropped_alloc_objects`, `aggregation_dropped_alloc_space`,
`aggregation_dropped_inuse_objects`, and `aggregation_dropped_inuse_space`.
The `event_order`, `off_cpu`, `firefox`, `jit`, and `scope` objects expose
bounded ordering pressure, off-CPU interval quality, Firefox output counts,
JIT artifact discovery, and the effective process/cgroup scope.
When the stack cap is reached, the window warnings also identify the aggregation
drops. Heap live/free state continues to be tracked even when a new stack is
omitted. When OTLP is enabled, `diagnostics.otlp` reports
`pending`, `exported`, `partial`, `failed`, or `dropped` and includes attempts,
rejected profile count, and a sanitized error. With `--otlp-timeline`, it also
reports `timeline_enabled`, encoded `timeline_samples`,
`timeline_dropped_samples`, and `timeline_timestamp_errors`.
Firefox timeline drops are reported in `firefox.dropped_samples` and
`event_order.timeline_events_dropped`; import diagnostics use
`timeline_dropped_samples`.

`--max-stacks` applies independently to each CPU, off-CPU, and heap output window. Once
the cap is full, existing stacks continue to accumulate while only newly
observed distinct stacks are omitted from pprof, OTLP, and optional SVG output.

With `--svg`, the asynchronous output worker also atomically generates a
self-contained static flame graph for each requested CPU, off-CPU, or heap
profile. CPU and off-CPU frame widths use nanoseconds; heap frame widths use
`inuse_space/bytes`.
SVGs contain no scripts and are derived visualizations only: pprof and OTLP
remain the authoritative machine-readable formats. Rendering is capped at
100,000 frames/nodes; when exceeded, only the SVG is truncated and the graph
marks the truncation, without changing pprof or OTLP. SVGs are part of the
window output set and are retained or removed with the other files by
`--keep-windows`. SVG rendering streams directly into the atomic temporary
file; it does not first construct the complete SVG text in memory.

The output worker keeps collection ahead of slow output. If a previous window
is still being written when the next window is submitted, it sheds only derived
outputs for that next window: optional SVG files and Firefox output are skipped,
and a configured OTLP export is marked `dropped`. CPU/off-CPU/heap pprof files
and diagnostics remain authoritative and are still written. The diagnostics
`output_backpressure` object records `derived_outputs_shed`, the pending-window
count, the number of skipped derived files, and whether OTLP was skipped. A
full OTLP export queue is reported separately as `otlp.status: dropped`; local
files are never removed because an export is unavailable.

Every output file is written through a temporary file in the destination directory,
`fsync`ed, atomically renamed into place, and followed by a directory sync.
Window publication is transactional: if CPU/off-CPU/heap pprof, optional SVG, or
diagnostics generation fails, any files already published for that window are
removed. `wrote` lines are printed only after retention succeeds.
Retention is session-scoped: once more than `--keep-windows` windows have been
written in this invocation, all files belonging to the oldest window (CPU,
off-CPU, and heap when requested, optional SVGs, plus diagnostics) are removed.
Files from older
invocations are not pruned.

### Profiling output example

With `--svg`, rustprofile writes a self-contained flame graph alongside the
machine-readable pprof and diagnostics files. Frame width represents the share
of sampled CPU time (or heap in-use bytes for a heap profile); hover a frame in
an SVG viewer to see its label and percentage.

![Illustrative rustprofile CPU flame graph](docs/profiling-example.svg)

The image is an illustrative 10-second window showing the shape of the actual
renderer output; production SVGs are generated from the selected target's real
samples.

The browser-based profile viewer has a separate static UI preview:

![rustprofile profile viewer UI preview](docs/profile-ui-preview.svg)

*This is a static UI preview of the rustprofile profile viewer—not a captured
profile output; real sessions populate the same layout from selected profile
data.*

## Docker and Kubernetes deployment

The included [Dockerfile](Dockerfile) builds a Linux release image. A Docker
profiler must see the host PID namespace and the Docker API socket:

```sh
docker build -t rustprofile:0.2.1 .
docker run --rm --privileged --pid=host \
  --mount type=bind,src=/var/run/docker.sock,dst=/var/run/docker.sock,readonly \
  --mount type=bind,src="$PWD/profiles",dst=/profiles \
  rustprofile:0.2.1 check --docker-container my-api --json
docker run --rm --privileged --pid=host \
  --mount type=bind,src=/var/run/docker.sock,dst=/var/run/docker.sock,readonly \
  --mount type=bind,src="$PWD/profiles",dst=/profiles \
  rustprofile:0.2.1 record --docker-container my-api \
  --duration 10m --window 60s --output /profiles
```

The Docker socket is a host-control boundary: even a read-only bind mount of
the socket lets software with access to the API request operations that are
effectively equivalent to root on the Docker host. Use a dedicated image and
restrict who can start it. `--privileged` and `--pid=host` are also host-level
permissions required by perf/eBPF and host process inspection. On Linux,
`record` checks for tracefs at `/sys/kernel/tracing`; when it is missing, a
root invocation tries to mount it there. The mount requires `CAP_SYS_ADMIN`,
which the example's `--privileged` supplies. Without that capability, or when
the mount fails, the command exits with an explicit error such as
`tracefs is not mounted; run rustprofile with CAP_SYS_ADMIN/--privileged or
mount tracefs at /sys/kernel/tracing`.

For Kubernetes, build or load the image on every node and apply
`deploy/kubernetes/rustprofile.yaml`. It creates the `rustprofile-system`
namespace, a ServiceAccount, a ClusterRole limited to `get` on Pods, and an
idle DaemonSet. The DaemonSet injects `NODE_NAME`, uses `hostPID`, privileged
and Unconfined seccomp settings, and mounts the node-local
`/var/lib/rustprofile` directory at `/profiles`:

```sh
kubectl apply -f deploy/kubernetes/rustprofile.yaml
kubectl get pods -n rustprofile-system -l app.kubernetes.io/name=rustprofile -o wide
# Select the profiler Pod scheduled on the target Pod's node.
kubectl exec -n rustprofile-system "$PROFILER_POD" -- \
  rustprofile check --k8s-pod default/api --container app --json
kubectl exec -n rustprofile-system "$PROFILER_POD" -- \
  rustprofile record --k8s-pod default/api --container app \
  --duration 10m --window 60s --output /profiles
```

To configure OTLP for the DaemonSet, edit the endpoint, compression, timeout,
and authentication values in `deploy/kubernetes/otel-config.example.yaml`
before applying it. The example uses a ConfigMap for non-secret settings and a
Secret for headers; replace its placeholder credential and keep real secrets
outside source control. For a private receiver CA, create the optional Secret
expected by the DaemonSet (`rustprofile-otel-ca`) with a `ca.crt` PEM key and
set `OTEL_EXPORTER_OTLP_PROFILES_CERTIFICATE=/etc/rustprofile/otel/ca.crt` in
the DaemonSet environment.

The selected Pod must be on the same node as `$PROFILER_POD`; the target
resolver rejects cross-node selection. It fixes the initial Pod UID and
container identity. A restart of the same container or Pod UID is followed
and attached to with its new host PID; a deleted/replaced Pod with a new UID is
not followed. A target must be running for the initial `check` or `record`
resolution. `--duration 0` waits indefinitely for a same-identity restart until
the command receives SIGINT/SIGTERM or the logical target is gone. Init and
ephemeral containers are not selected; for a multi-container application Pod,
always pass `--container`.

Docker inspect and Kubernetes Pod API control-plane requests use a 5-second
timeout and reject responses larger than 4 MiB. These bounds apply to target
identity resolution, not to the local profile files or OTLP payloads.

The DaemonSet has no control HTTP endpoint. Use `kubectl exec` for each explicit
check or recording, and collect node-local files from the host path. Keep its
`privileged: true` setting (which supplies `CAP_SYS_ADMIN`) or otherwise
provide that capability and a usable tracefs mount; a missing tracefs mount
causes `record` to fail with an explicit mount error. Its privileged/host-PID
configuration and Pod API access should be treated as node-level operational
permissions.

## OTLP Profiles export

OTLP export is optional and runs alongside local pprof/diagnostics output. The
implementation is pinned to `opentelemetry-proto v1.11.0` and sends the
Development Profiles signal over OTLP/HTTP `http/protobuf` to
`/v1development/profiles`. It emits `application/x-protobuf`; requests use
gzip by default and can be sent uncompressed with `--otlp-compression none`.
The Profiles signal is Development/Alpha and its wire contract can change;
keep the receiver compatible with v1.11.0.

Configuration precedence is CLI, Profiles-specific environment variable, then
generic OTLP environment variable:

| Setting | CLI | Profiles env | Generic env / default |
| --- | --- | --- | --- |
| Endpoint | `--otlp-endpoint URL` | `OTEL_EXPORTER_OTLP_PROFILES_ENDPOINT` | `OTEL_EXPORTER_OTLP_ENDPOINT` plus `/v1development/profiles` |
| Protocol | — | `OTEL_EXPORTER_OTLP_PROFILES_PROTOCOL` | `OTEL_EXPORTER_OTLP_PROTOCOL`; only `http/protobuf` |
| Headers | repeat `--otlp-header KEY=VALUE` | `OTEL_EXPORTER_OTLP_PROFILES_HEADERS` | `OTEL_EXPORTER_OTLP_HEADERS` |
| Timeout | `--otlp-timeout DURATION` | `OTEL_EXPORTER_OTLP_PROFILES_TIMEOUT` | `OTEL_EXPORTER_OTLP_TIMEOUT`; `10s` default |
| Compression | `--otlp-compression none\|gzip` | `OTEL_EXPORTER_OTLP_PROFILES_COMPRESSION` | `OTEL_EXPORTER_OTLP_COMPRESSION`; `gzip` default |
| Extra CA | `--otlp-ca PATH` | `OTEL_EXPORTER_OTLP_PROFILES_CERTIFICATE` | `OTEL_EXPORTER_OTLP_CERTIFICATE` |
| Resource attrs | repeat `--resource-attribute KEY=VALUE` | — | `OTEL_RESOURCE_ATTRIBUTES` |
| Service name | — | — | `OTEL_SERVICE_NAME` |

Environment header and resource-attribute values use comma-delimited
`KEY=VALUE` pairs; CLI entries are repeatable. Header values are passed to the
receiver but are not written to diagnostics. HTTPS uses system roots and can
append a PEM CA file; client-certificate/mTLS configuration is not provided.
Endpoint URLs must be `http://` or `https://` and must not include embedded
credentials. An endpoint is required to enable export; without one, no network
request is made.

Each completed local window is encoded as one OTLP request containing one
Profile per pprof sample type, sharing a dictionary. Resource attributes
include `service.name`, executable path/name, integer `process.pid`, target
kind, and available Docker/Kubernetes identity fields. Profile IDs are 16-byte
random values. The exporter uses a bounded queue of four windows and never
removes the local files if export fails. Only transient transport failures (I/O,
timeout, DNS, or HTTP protocol/connection failure) are retried; invalid
URI/header and TLS/certificate configuration errors fail immediately. HTTP 408,
429, 502, 503, and 504 are retried up to five attempts with interruptible
exponential backoff; an integer-seconds `Retry-After` is honored but capped at
30 seconds.
OTLP response bodies are capped at 1 MiB. The gzip request body is prepared once
per window and reused across retries. Non-retryable HTTP errors or an oversized
response end the export for that window. A successful response with rejected
profiles is recorded as `partial`; a full queue is `dropped`. Normal shutdown
drains queued windows using the configured retry policy. An abnormal teardown
cancels retries and marks any remaining queued windows `failed`; local files remain.
There is no durable on-disk OTLP spool or automatic later replay; retain the
local pprof and diagnostics files for recovery.

`--otlp-timeline` changes only the OTLP CPU source: the bounded timeline is sent
as one `cpu/nanoseconds` profile, and the aggregated CPU source is not sent a
second time. Each timeline sample has aligned `values` and
`timestamps_unix_nano` plus pprof labels decoded as attributes such as
`process.pid`, `thread.id`, and `thread.name`. Raw perf timestamps are converted
to Unix nanoseconds inside the window; samples that cannot be converted are
omitted and counted by `timeline_timestamp_errors`. The cap from
`--max-timeline-samples` applies to this OTLP timeline as well as Firefox, and
does not require a Firefox output file.

## Symbols and debug information

The symbolizer first tries symbols/DWARF embedded in each mapped ELF. It then
searches external files using, in order, GNU `.gnu_debuglink` locations,
`/usr/lib/debug` paths, build-ID paths under `/usr/lib/debug/.build-id`, and
the supplied `--symbol-dir` directories (including matching module names and
build-ID trees). Rust and C++ names are demangled when possible. Mapping
records preserve the module path and build ID.

`--debuginfod URL` is explicit opt-in. For a module with a build ID, after local
searches fail it requests `URL/buildid/<build-id>/debuginfo` within a shared
30-second budget for the Symbolizer initialization, streams each response into
a temporary cache file, and enforces a 512 MiB per-file limit before publishing
it. There is no default endpoint or network lookup when the flag is omitted. A
failed or oversized fetch is non-fatal; affected locations remain at
module/offset level or unsymbolized. Mapping changes are noticed during
recording and cause symbol/unwinder reloads.

Stripped binaries can therefore still produce address/mapping data, but useful
function/file/line names require embedded or discoverable debug information.

## Target lifecycle, signals, and partial mode

- `record --pid` follows one explicit host process and does not implicitly
  follow its children. Docker and Kubernetes targets resolve their container
  cgroup; CPU and off-CPU collectors reconcile the cgroup's processes and
  threads. Sampled heap remains attached to the container init process, and
  each diagnostics window records the effective scope and any degradation.
- `launch` is the opt-in child/descendant workflow. It uses a cgroup-v2
  boundary when available and otherwise requires `--allow-partial` to fall
  back to the exact child PID.
- `off-cpu` uses a bounded sched-switch eBPF collector. Intervals crossing a
  window boundary are split, and ring-buffer drops, incomplete intervals, and
  aggregation limits are reported in diagnostics. CPU perf event ordering uses
  bounded pending buffers controlled by `--max-pending-events` and
  `--event-reorder-window`; off-CPU intervals use a separate bounded queue.
- A pidfd detects the current target process exit. The current window is
  finalized. A PID target then returns; a Docker/Kubernetes target waits for a
  new process belonging to the same fixed container ID or Pod UID and starts a
  new window in the same session. The new host PID and process start time are
  recorded, and heap in-use state restarts. A removed container or replaced Pod
  is not followed.
- The executable identity is checked about once per second. On `exec`, the
  current window ends with a warning, the process is re-preflighted, unwind mode
  and symbols are reselected, and collectors are reopened for the new image.
- Linux 5.8 and newer use lifecycle eBPF events when they can be attached.
  Linux 5.4-5.7, or a newer host where that optional attachment fails, uses the
  same one-second procfs reconciliation and records the fallback in diagnostics.
- An `exec` during the initial `auto` FP calibration is handled similarly:
  preflight is refreshed and calibration restarts for the replacement image
  instead of failing the recording as a calibration error.
- SIGINT and SIGTERM stop at the polling boundary after the current window is
  finalized. If an interrupt arrives during the initial automatic FP
  calibration, calibration exits with an interruption error instead.

`--allow-partial` applies only to optional profile capabilities. It can disable
heap or off-CPU when detection/attach fails, permit leaf-only CPU output when
DWARF/CFI is unavailable, and let `launch` fall back from cgroup descendants
to the exact child PID. Preflight errors (non-root, a kernel older than 5.4,
architecture mismatch, excessive threads, or failed perf access) remain fatal;
if every requested profile is disabled, recording also fails.

## Supported scope and validation limits

The implemented runtime scope is Linux 5.4+ CPU profiling, Linux 5.8+ off-CPU
profiling when the sched-switch eBPF probe is available, and Linux 5.12+ heap
profiling on native x86_64/aarch64. It includes explicit process/container
targets, opt-in launch cgroup tracking, local pprof/Firefox/diagnostics files,
perf.data import, a local symbol API server, and optional OTLP/HTTP Profiles
export. It does not claim support for other operating systems, other
architectures, arbitrary allocators, SDK instrumentation, OTLP gRPC, mTLS, or
unverified production kernels. Heap remains init-process scoped when CPU/off-
CPU are collecting a container cgroup.

The development host is macOS arm64, where the Linux runtime and its perf/eBPF
facilities are unavailable. Luna MAX has validated CPU FP, DWARF fallback,
the stripped/no-CFI matrix, system heap, and protobuf decoding as root in an
OrbStack Linux/aarch64 environment. Those results are useful Linux/aarch64
evidence but do not close the final release gates: independent native x86_64
validation and an independent native aarch64 validation are still incomplete.
Cross-compilation or an arm64 OrbStack compile is not evidence that perf ring
buffers, allocator uprobes, or aarch64 PAC/TBI address normalization work on
the other architecture. The Linux 5.4 compatibility path still needs
independent runtime validation on a native 5.4 kernel.
