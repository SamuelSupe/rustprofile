# rustprofile

![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange?logo=rust)
![Linux](https://img.shields.io/badge/platform-Linux%205.8%2B-blue?logo=linux)
![License](https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-green)
[![Latest release](https://img.shields.io/github/v/release/SamuelSupe/rustprofile?label=release)](https://github.com/SamuelSupe/rustprofile/releases)

`rustprofile` is a Linux command-line profiler for one already-running native
process, Docker container, or Kubernetes application container. It records
per-thread CPU samples and sampled allocator activity into gzip-compressed
pprof profile protobufs, plus a JSON diagnostics file for every time window.
When configured, the same completed windows are also exported as OTLP Profiles
over HTTP/protobuf; local files remain the authoritative record.

[简体中文说明](dist/README.zh-CN.md) · [发行记录](CHANGELOG.md) · [GitHub Releases](https://github.com/SamuelSupe/rustprofile/releases)

This README describes the implemented CLI and its limits. Validation claims
are limited to the explicitly listed evidence; no broader production-kernel,
allocator, or deployment claim is implied.

## Requirements and build

- Linux 5.8 or newer. Both `check` and `record` require the invoking process
  to run as root.
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
and the selected allocator family. It also performs the perf/eBPF access and
allocator eBPF load probes used by the preflight path; it does not start a
recording or attach the allocator uprobes used by `record`.

Exactly one target selector is required. `--pid` selects a host process,
`--docker-container` selects a Docker container by ID or name, and `--k8s-pod`
selects a Kubernetes Pod in `NAMESPACE/NAME` form. `--container` is only valid
with `--k8s-pod` and is required when the Pod has multiple application
containers.

`--json` emits schema version 2 (`CheckReport`) with `target`, `warnings`, and
`errors`.
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
| `--profiles LIST` | `cpu,heap`; comma-delimited `cpu` and/or `heap` (duplicates are removed). |
| `--duration DURATION` | `60s`; humantime syntax, or `0` for no deadline. |
| `--window DURATION` | `60s`; must be greater than zero. The final window may be shorter. |
| `--unwind MODE` | `auto`; `auto`, `fp`, or `dwarf`. |
| `--cpu-frequency HZ` | `49`; 1 through 999 target-CPU samples per second. |
| `--alloc-interval BYTES` | `512 KiB` (524288 bytes); positive byte-size syntax. |
| `--allocator FAMILY` | `auto`; `auto`, `rust`, or `system`. |
| `--output DIR` | `.`; directory for profile and diagnostics windows. |
| `--keep-windows N` | `60`; positive number of windows retained for this recording session. |
| `--max-stacks N` | `65,536`; maximum distinct stacks in each CPU or heap output window. Existing stacks continue accumulating; new stacks after the cap are omitted. |
| `--svg` | Off; also write self-contained static SVG flame graphs for completed CPU and heap windows. |
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
diagnostics-<session-id>-<window-index>-<start-unix-nanos>.json
```

The CPU profile has `samples/count` and `cpu/nanoseconds` sample values. The
heap profile has `alloc_objects/count`, `alloc_space/bytes`,
`inuse_objects/count`, and `inuse_space/bytes`; its default sample type is
`inuse_space` and its period is the allocation interval. The profiles are
gzip-compressed pprof protobufs. Samples carry `process.pid` and, for a
container target, the container/Kubernetes identity labels. Diagnostics JSON
is schema version 2 and includes session/PID/timestamps, the structured
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
When the stack cap is reached, the window warnings also identify the aggregation
drops. Heap live/free state continues to be tracked even when a new stack is
omitted. When OTLP is enabled, `diagnostics.otlp` reports
`pending`, `exported`, `partial`, `failed`, or `dropped` and includes attempts,
rejected profile count, and a sanitized error.

`--max-stacks` applies independently to each CPU and heap output window. Once
the cap is full, existing stacks continue to accumulate while only newly
observed distinct stacks are omitted from pprof, OTLP, and optional SVG output.

With `--svg`, the asynchronous output worker also atomically generates a
self-contained static flame graph for each requested CPU or heap profile. CPU
frame widths use `cpu/nanoseconds`; heap frame widths use `inuse_space/bytes`.
SVGs contain no scripts and are derived visualizations only: pprof and OTLP
remain the authoritative machine-readable formats. Rendering is capped at
100,000 frames/nodes; when exceeded, only the SVG is truncated and the graph
marks the truncation, without changing pprof or OTLP. SVGs are part of the
window output set and are retained or removed with the other files by
`--keep-windows`. SVG rendering streams directly into the atomic temporary
file; it does not first construct the complete SVG text in memory.

Every output file is written through a temporary file in the destination directory,
`fsync`ed, atomically renamed into place, and followed by a directory sync.
Window publication is transactional: if CPU/heap pprof, optional SVG, or
diagnostics generation fails, any files already published for that window are
removed. `wrote` lines are printed only after retention succeeds.
Retention is session-scoped: once more than `--keep-windows` windows have been
written in this invocation, all files belonging to the oldest window (CPU and
heap when requested, optional SVGs, plus diagnostics) are removed. Files from older
invocations are not pruned.

## Docker and Kubernetes deployment

The included [Dockerfile](Dockerfile) builds a Linux release image. A Docker
profiler must see the host PID namespace and the Docker API socket:

```sh
docker build -t rustprofile:0.1.0 .
docker run --rm --privileged --pid=host \
  --mount type=bind,src=/var/run/docker.sock,dst=/var/run/docker.sock,readonly \
  --mount type=bind,src="$PWD/profiles",dst=/profiles \
  rustprofile:0.1.0 check --docker-container my-api --json
docker run --rm --privileged --pid=host \
  --mount type=bind,src=/var/run/docker.sock,dst=/var/run/docker.sock,readonly \
  --mount type=bind,src="$PWD/profiles",dst=/profiles \
  rustprofile:0.1.0 record --docker-container my-api \
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
profiles is recorded as `partial`; a full queue is `dropped`. At shutdown the
exporter stops retrying; local files remain, while queued windows that were not
flushed are marked `failed` for a bounded exit.
There is no durable on-disk OTLP spool or automatic later replay; retain the
local pprof and diagnostics files for recovery.

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

- The recorder follows one explicit target: a host PID, Docker container, or
  Kubernetes application container. It does not launch a process, follow a
  child tree or cgroup, collect off-CPU time, or collect kernel stacks.
- A pidfd detects the current target process exit. The current window is
  finalized. A PID target then returns; a Docker/Kubernetes target waits for a
  new process belonging to the same fixed container ID or Pod UID and starts a
  new window in the same session. The new host PID and process start time are
  recorded, and heap in-use state restarts. A removed container or replaced Pod
  is not followed.
- The executable identity is checked about once per second. On `exec`, the
  current window ends with a warning, the process is re-preflighted, unwind mode
  and symbols are reselected, and collectors are reopened for the new image.
- An `exec` during the initial `auto` FP calibration is handled similarly:
  preflight is refreshed and calibration restarts for the replacement image
  instead of failing the recording as a calibration error.
- SIGINT and SIGTERM stop at the polling boundary after the current window is
  finalized. If an interrupt arrives during the initial automatic FP
  calibration, calibration exits with an interruption error instead.

`--allow-partial` applies only to optional profile capabilities. It can disable
heap when allocator detection or attach fails, and it can permit leaf-only CPU
output when DWARF/CFI is unavailable. Preflight errors (non-root, old kernel,
architecture mismatch, excessive threads, or failed perf access) remain fatal;
if every requested profile is disabled, recording also fails.

## Supported scope and validation limits

The implemented runtime scope is Linux 5.8+, native x86_64/aarch64, one
explicit process/container target, user CPU sampling, sampled Rust/libc
allocator probes, local pprof/diagnostics files, and optional OTLP/HTTP Profiles
export. It does not claim support for other operating systems, other
architectures, arbitrary allocators, child/cgroup tracking, SDK instrumentation,
OTLP gRPC, mTLS, or unverified production kernels.

The development host is macOS arm64, where the Linux runtime and its perf/eBPF
facilities are unavailable. Luna MAX has validated CPU FP, DWARF fallback,
the stripped/no-CFI matrix, system heap, and protobuf decoding as root in an
OrbStack Linux/aarch64 environment. Those results are useful Linux/aarch64
evidence but do not close the final release gates: independent native x86_64
validation and an independent native aarch64 validation are still incomplete.
Cross-compilation or an arm64 OrbStack compile is not evidence that perf ring
buffers, allocator uprobes, or aarch64 PAC/TBI address normalization work on
the other architecture.
