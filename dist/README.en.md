# rustprofile 0.2.1

Linux continuous CPU, off-CPU, and sampled-heap profiling for an existing
native process, Docker container, or Kubernetes application container. This
document is intended to ship with the release archives. `launch` starts a
command suspended before attaching collectors, `import` converts perf.data or
simpleperf captures, and `serve` exposes Firefox profile symbol APIs.
Completed windows can also be sent as OTLP Profiles over HTTP/protobuf when an
endpoint is configured; local pprof and diagnostics files remain authoritative.

## Support and prerequisites

- Linux 5.4 or newer for CPU profiling. Heap profiling requires Linux 5.12 or
  newer. On Linux 5.4-5.11, explicitly request `--profiles cpu`.
- Off-CPU profiling requires the sched-switch eBPF probe; `launch` descendant
  tracking requires cgroup v2. Heap remains scoped to the container/init
  process when CPU/off-CPU use a container cgroup.
- Run `check`, `record`, and `launch` as `root`; `import` and `serve` are
  user-space workflows.
- The binary architecture must match the target process: use the aarch64
  archive for an aarch64 process and the x86_64 archive for an x86_64 process.
- The release binary is a dynamic GNU/Linux PIE, not a static binary. It
  requires glibc 2.31 or newer and the runtime shared libraries `libelf.so.1`,
  `libz.so.1`, `libzstd.so.1`, `libgcc_s.so.1`, the matching libc, and the
  dynamic loader. The archives do not bundle these system libraries; the
  relevant kernel baseline and root are necessary but not sufficient. Check
  the resolved runtime dependencies before installation with
  `ldd ./rustprofile` and resolve any `not found` entries in the host image.
- `record --pid` follows one host PID. Docker/Kubernetes CPU and off-CPU
  collection follows the resolved container cgroup; heap remains init-process
  scoped. `launch` is the explicit child/descendant workflow.
- Docker selection requires host PID visibility, the privileges needed by
  perf/eBPF, and access to `/var/run/docker.sock`.
- Kubernetes selection requires the supplied privileged, `hostPID` DaemonSet;
  see the deployment section below.

## Release files and SHA256

The release archives are:

```text
rustprofile-0.2.1-linux-aarch64.tar.gz
rustprofile-0.2.1-linux-x86_64.tar.gz
```

`SHA256SUMS` is shipped with the archives. From the directory containing the
files, verify the download before extraction:

```sh
sha256sum -c SHA256SUMS
```

## Extract and install

Choose the archive matching both the host and target process architecture:

```sh
tar -xzf rustprofile-0.2.1-linux-aarch64.tar.gz
sudo install -m 0755 rustprofile /usr/local/bin/rustprofile
```

Use the x86_64 filename for x86_64 systems. If the archive was unpacked into a
directory, run `install` from the archive root. After extraction, that root
contains `rustprofile`, `README.en.md`, and `README.zh-CN.md`.

## Quick start

Inspect a target before recording:

```sh
sudo rustprofile check --pid "$PID"
sudo rustprofile check --pid "$PID" --json > check.json
```

Record the default CPU and heap profiles in 60-second windows:

```sh
sudo rustprofile record \
  --pid "$PID" \
  --duration 10m \
  --window 60s \
  --output ./profiles
```

`--duration 0` records until the target exits or the recorder receives
SIGINT/SIGTERM.
Add `--profiles cpu` on Linux 5.4-5.11.

Start a command suspended, attach, then continue it. On cgroup-v2 hosts this
also includes descendants in CPU/off-CPU collection:

```sh
sudo rustprofile launch --profiles cpu,off-cpu --firefox-profile json \
  --duration 10m --window 60s --output ./profiles \
  -- ./my-api --port 8080
```

Import an existing capture and optionally create a Firefox profile:

```sh
rustprofile import --input perf.data --format auto --window 60s \
  --firefox-profile jslb --output ./imported
```

Serve a Firefox JSON/JSLB profile and its symbol/source/assembly API:

```sh
rustprofile serve --profile ./profiles/firefox-session-000000-123.json.gz \
  --listen 127.0.0.1:8080
```

Serve all Firefox windows in a directory with the built-in gallery:

```sh
rustprofile serve --directory ./profiles --listen 127.0.0.1:8080
```

Docker targets can be selected by container ID or name:

```sh
sudo rustprofile check --docker-container my-api --json
sudo rustprofile record --docker-container my-api \
  --duration 10m --window 60s --output ./profiles
```

Kubernetes targets use `NAMESPACE/NAME`; run the command in a profiler Pod on
the target Pod's node. Omit `--container` only when the Pod has one application
container:

```sh
kubectl exec -n rustprofile-system "$PROFILER_POD" -- \
  rustprofile check --k8s-pod default/api --container app --json
kubectl exec -n rustprofile-system "$PROFILER_POD" -- \
  rustprofile record --k8s-pod default/api --container app \
  --duration 10m --window 60s --output /profiles
```

## Main options

### `check`

```text
rustprofile check (--pid PID | --docker-container ID_OR_NAME |
                   --k8s-pod NAMESPACE/NAME [--container NAME]) [--json]
                   [--symbol-dir DIR]... [--debuginfod URL]
```

Exactly one target selector is required. `--pid PID` selects a host process,
`--docker-container ID_OR_NAME` selects Docker, and `--k8s-pod
NAMESPACE/NAME` selects Kubernetes. `--container NAME` is only valid with
`--k8s-pod` and is required for multi-container Pods.
- `--json`: print the schema-versioned preflight report as JSON.
- `--symbol-dir DIR`: repeatable directory for external ELF/debug files.
- `--debuginfod URL`: explicit `http://` or `https://` base URL; no lookup is
  attempted when omitted. Debuginfod lookups share a 30-second Symbolizer
  initialization budget, stream into temporary cache files, and enforce a
  512 MiB per-file limit.

`check` validates root/kernel/architecture/thread and perf access, probes
lifecycle and off-CPU eBPF loading, and probes heap eBPF loading when the
kernel meets the heap baseline, and
reports mapped modules, build IDs, unwind sections, symbols, and allocator
selection. It does not start a recording.

### `record`

- `--pid PID`, `--docker-container ID_OR_NAME`, or `--k8s-pod
  NAMESPACE/NAME`: choose exactly one target. Kubernetes also accepts
  `--container NAME`.
- `--profiles cpu,heap,off-cpu`: comma-delimited profile types; default
  `cpu,heap`.
- `--duration DURATION`: default `60s`; humantime syntax, or `0` for no limit.
- `--window DURATION`: default `60s`; must be greater than zero.
- `--unwind auto|fp|dwarf`: default `auto`.
- `--cpu-frequency HZ`: target-CPU samples per second; default `49`, range
  `1..=999`.
- `--alloc-interval BYTES`: mean heap sampling interval; default `512 KiB`.
- `--allocator auto|rust|system`: default `auto`.
- `--output DIR`: output directory; default `.`.
- `--keep-windows N`: session retention count; default `60`.
- `--max-stacks N`: default `65,536`; maximum distinct stacks in each CPU,
  off-CPU, or heap output window. Existing stacks continue accumulating; new
  stacks after the cap are omitted.
- `--max-pending-events N`: default `262,144`; bounded timestamp-ordering
  buffer for perf/eBPF events.
- `--event-reorder-window DURATION`: default `100ms`; timestamp skew tolerated
  while ordering event sources, and it cannot exceed `--window`.
- `--max-timeline-samples N`: default `65,536`; maximum timestamped samples
  retained for each Firefox output or OTLP timeline window. Excess samples are
  omitted from the enabled timeline output and counted in diagnostics.
- `--otlp-timeline`: disabled by default; with OTLP enabled, send the bounded
  timestamped CPU timeline instead of an aggregated CPU source. The local CPU
  pprof remains available.
- `--firefox-profile json|jslb`: write one bounded Firefox processed profile per
  completed window.
- `--svg`: disabled by default; also write self-contained static SVG flame graphs
  for completed CPU, off-CPU, and heap windows.
- `--allow-partial`: allow supported subsets or leaf-only CPU data when a
  profile capability is unavailable.
- `--symbol-dir DIR` and `--debuginfod URL`: the same symbol options as
  `check`.
- `--otlp-endpoint URL`: optional OTLP/HTTP Profiles endpoint; no endpoint
  means no export.
- `--otlp-header KEY=VALUE`: repeatable OTLP header.
- `--otlp-timeout DURATION`: per-attempt timeout; defaults to `10s` (environment
  values are milliseconds).
- `--otlp-compression none|gzip`: defaults to `gzip`.
- `--otlp-ca PATH`: additional PEM CA for an OTLP HTTPS endpoint.
- `--resource-attribute KEY=VALUE`: repeatable OTLP resource attribute.

Preflight failures such as non-root execution, an old kernel, an architecture
mismatch, or failed perf access remain fatal. If partial mode disables every
requested profile, recording also fails.

## Output files

Each completed window writes files like these under `--output`:

```text
cpu-<session>-<index>-<start-unix-nanos>.pb.gz
cpu-<session>-<index>-<start-unix-nanos>.svg       (with --svg)
heap-<session>-<index>-<start-unix-nanos>.pb.gz
heap-<session>-<index>-<start-unix-nanos>.svg       (with --svg)
off-cpu-<session>-<index>-<start-unix-nanos>.pb.gz
off-cpu-<session>-<index>-<start-unix-nanos>.svg       (with --svg)
firefox-<session>-<index>-<start-unix-nanos>.json.gz
firefox-<session>-<index>-<start-unix-nanos>.jslb.gz
diagnostics-<session>-<index>-<start-unix-nanos>.json
```

CPU and heap files are gzip-compressed pprof profile protobufs. CPU samples
contain `samples/count` and `cpu/nanoseconds`. Heap samples contain
`alloc_objects/count`, `alloc_space/bytes`, `inuse_objects/count`, and
`inuse_space/bytes`; in-use values include only sampled allocations observed
since attach. Samples include `process.pid` and available container/Kubernetes
identity labels. The diagnostics JSON is schema version 3 and includes target
metadata, output paths, warnings, allocator probe information, CPU
loss/malformed counters, CPU nanoseconds, `aggregation_dropped_samples`, and
`aggregation_dropped_nanoseconds`; heap export totals, `since_attach`, and
`aggregation_dropped_alloc_objects`, `aggregation_dropped_alloc_space`,
`aggregation_dropped_inuse_objects`, and `aggregation_dropped_inuse_space`.
When the stack cap is reached, window warnings also identify the aggregation
drops. Heap live/free state continues to be tracked even when a new stack is
omitted. When enabled, diagnostics also include `otlp.status`, attempts,
rejected profiles, and a sanitized error. With `--otlp-timeline`, they also
include `timeline_enabled`, encoded `timeline_samples`,
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
marks the truncation, without changing pprof or OTLP. SVGs are retained or
removed with the other window files by `--keep-windows`. SVG rendering streams
directly into the atomic temporary file; it does not first construct the
complete SVG text in memory.

The output worker keeps collection ahead of slow output. If a previous window
is still being written when the next window is submitted, it sheds only derived
outputs for that next window: optional SVG files and Firefox output are skipped,
and a configured OTLP export is marked `dropped`. CPU/off-CPU/heap pprof files
and diagnostics remain authoritative and are still written. The diagnostics
`output_backpressure` object records `derived_outputs_shed`, the pending-window
count, the number of skipped derived files, and whether OTLP was skipped. A
full OTLP export queue is reported separately as `otlp.status: dropped`; local
files are never removed because an export is unavailable.

The output worker publishes every output file atomically. If CPU/off-CPU/heap pprof,
optional SVG, or diagnostics generation fails, already-published files for that
window are removed. `wrote` lines are printed only after retention succeeds.
`--keep-windows` removes expired files from this recording session only,
including optional SVGs; older sessions are not pruned.

## Docker and Kubernetes deployment

From the source checkout, build the included image on a Linux builder:

```sh
docker build -t rustprofile:0.2.1 .
docker run --rm --privileged --pid=host \
  --mount type=bind,src=/var/run/docker.sock,dst=/var/run/docker.sock,readonly \
  --mount type=bind,src="$PWD/profiles",dst=/profiles \
  rustprofile:0.2.1 record --docker-container my-api \
  --duration 10m --window 60s --output /profiles
```

The Docker socket is a host-control boundary. Read-only mounting the socket
does not make the Docker API unprivileged: code with socket access can request
operations equivalent to root on the Docker host. The privileged and host-PID
settings are likewise node-level permissions required by perf/eBPF. On Linux,
`record` checks for tracefs at `/sys/kernel/tracing` and, when it is missing,
root attempts to mount it there. This requires `CAP_SYS_ADMIN`, supplied by
the example's `--privileged`. Without it, or when mounting fails, the command
exits with an explicit error such as `tracefs is not mounted; run rustprofile
with CAP_SYS_ADMIN/--privileged or mount tracefs at /sys/kernel/tracing`.

From the source checkout, apply `deploy/kubernetes/rustprofile.yaml` for a
node-local profiler:

```sh
kubectl apply -f deploy/kubernetes/rustprofile.yaml
kubectl get pods -n rustprofile-system \
  -l app.kubernetes.io/name=rustprofile -o wide
```

For DaemonSet OTLP settings, edit `deploy/kubernetes/otel-config.example.yaml`
before applying it. Replace its placeholder header credential and keep real
secrets out of source control. For a private receiver CA, create the optional
`rustprofile-otel-ca` Secret with a `ca.crt` PEM key and set
`OTEL_EXPORTER_OTLP_PROFILES_CERTIFICATE=/etc/rustprofile/otel/ca.crt` in the
DaemonSet environment.

Select the profiler Pod scheduled on the same node as the target, then use
`kubectl exec` as shown above. The manifest creates a read-only Pod `get` RBAC
rule, injects `NODE_NAME`, uses `hostPID`, privileged/Unconfined seccomp, and
mounts host `/var/lib/rustprofile` as `/profiles`. It has no control HTTP
endpoint. A running target is required for initial resolution. The manifest's
`privileged: true` setting supplies `CAP_SYS_ADMIN`, which is required if
`record` must mount missing tracefs at `/sys/kernel/tracing`; keep it or
provide the capability and a usable tracefs mount. Otherwise the command exits
with an explicit mount error. The resolver
fixes a Docker container ID or Kubernetes Pod UID; a restart with the same
identity is reattached with a new host PID, while a removed/replaced target is
not followed. With `--duration 0`, recording waits until interrupted or the
logical target is gone.

Docker inspect and Kubernetes Pod API control-plane requests have a 5-second
timeout and reject responses larger than 4 MiB. These bounds apply to target
identity resolution, not to local profile files or OTLP payloads.

## Launch, import, serve, and scope

`launch` sends `SIGSTOP` to the child before preflight and collector
attachment, then resumes it. It creates a temporary cgroup-v2 boundary when
possible so CPU/off-CPU collectors reconcile descendants. If cgroup creation
is unavailable, `--allow-partial` permits an exact-child-PID fallback. Heap
probes remain scoped to the init/root process and diagnostics report
`mixed_process_and_cgroup` for that combination.

`import --input PATH` parses the timestamped IP/callchain samples from a
regular `perf.data` or simpleperf file. `--format auto` is the default, with
`perf-data` and `simpleperf` available as explicit hints. Imported pprof keeps
raw addresses and PID/TID labels; no live probe or DWARF unwind is attached.
`--max-stacks` bounds distinct attributed stacks per imported window, and
`--max-timeline-samples` bounds its Firefox timeline. Import keeps at most four
timestamp windows pending and caps tracked PID/TID state at 65,536 pairs.

`serve` requires exactly one of `--profile PATH` and `--directory DIR`.
`--profile` provides the legacy `GET /profile.json` (`application/json` for JSON
and `application/octet-stream` for JSLB); `--directory` scans at most 16,384
directory entries named `firefox-*.json.gz` or `firefox-*.jslb.gz` and returns
at most 4,096 profiles in the built-in gallery at `GET /`. The gallery lists
windows at `GET /api/profiles` and decodes a selected window at
`GET /api/profile/{sha256-filename-id}`. Both sources provide `GET /healthz`
and the Firefox Profiler/Samply-compatible symbol/source/assembly JSON POST
APIs. Compressed input is capped at 512 MiB and decompressed profile data at
128 MiB; diagnostics larger than 1 MiB are ignored. Viewer samples/stacks are
capped at 65,536, functions at 262,144, and threads at 4,096. POST bodies are
capped at 8 MiB and responses at 32 MiB. CORS is disabled by default;
`--cors-origin ORIGIN` enables an exact origin and its preflight (otherwise
`OPTIONS` is 405). Loopback listeners need no token, while non-loopback
listeners require `--bearer-token`.

For a running `record --pid` session, child processes are intentionally not
implicitly followed. Container targets use the resolved cgroup for CPU and
off-CPU, while heap remains init-process scoped. CPU perf ordering is bounded
by `--max-pending-events` and `--event-reorder-window`; off-CPU uses a separate
bounded interval queue. Drops and forced flushes are recorded in diagnostics.

## OTLP Profiles export

The exporter is pinned to `opentelemetry-proto v1.11.0` and sends the
Development Profiles signal via OTLP/HTTP `http/protobuf` to
`/v1development/profiles`. It uses `application/x-protobuf` and gzip by
default; `--otlp-compression none` disables compression. The Profiles signal is
Development/Alpha, so keep the receiver compatible with v1.11.0.

Endpoint precedence is CLI, Profiles-specific environment variable, then the
generic OTLP environment variable. Supported settings are:

```text
--otlp-endpoint URL                       OTEL_EXPORTER_OTLP_PROFILES_ENDPOINT
                                          OTEL_EXPORTER_OTLP_ENDPOINT + /v1development/profiles
--otlp-header KEY=VALUE (repeatable)      OTEL_EXPORTER_OTLP_PROFILES_HEADERS
                                          OTEL_EXPORTER_OTLP_HEADERS
--otlp-timeout DURATION                   OTEL_EXPORTER_OTLP_PROFILES_TIMEOUT
                                          OTEL_EXPORTER_OTLP_TIMEOUT (milliseconds)
--otlp-compression none|gzip              OTEL_EXPORTER_OTLP_PROFILES_COMPRESSION
                                          OTEL_EXPORTER_OTLP_COMPRESSION
--otlp-ca PATH                            OTEL_EXPORTER_OTLP_PROFILES_CERTIFICATE
                                          OTEL_EXPORTER_OTLP_CERTIFICATE
--resource-attribute KEY=VALUE (repeatable) OTEL_RESOURCE_ATTRIBUTES
                                          OTEL_SERVICE_NAME
```

Only `http/protobuf` is accepted for
`OTEL_EXPORTER_OTLP_PROFILES_PROTOCOL`/`OTEL_EXPORTER_OTLP_PROTOCOL`. Header
and resource environment values are comma-delimited `KEY=VALUE` pairs; header
values are not written to diagnostics. HTTPS uses system roots and can append
a PEM CA; mTLS is not supported. Endpoints must be HTTP(S) URLs without
embedded credentials.

Each completed local window is encoded as one request with one OTLP Profile per
pprof sample type and a shared dictionary. Resource attributes include
`service.name`, executable path/name, integer `process.pid`, target kind, and
available Docker/Kubernetes identity. Only transient transport failures (I/O,
timeout, DNS, or HTTP protocol/connection failure) are retried; invalid
URI/header and TLS/certificate configuration errors fail immediately. HTTP 408,
429, 502, 503, and 504 retry up to five attempts with backoff; integer
`Retry-After` is honored but capped at 30 seconds. OTLP response bodies are
capped at 1 MiB.
The gzip request body is prepared once per window and reused across retries.
The bounded queue holds four windows. Export failures never remove local files:
diagnostics report `pending`, `exported`, `partial`, `failed`, or `dropped`.
Normal shutdown drains queued windows using the configured retry policy. An
abnormal teardown cancels retries and marks any remaining queued windows
`failed`. There is no durable on-disk OTLP spool or automatic later replay;
retain the local files for recovery.

`--otlp-timeline` changes only the OTLP CPU source: the bounded timeline is sent
as one `cpu/nanoseconds` profile, and the aggregated CPU source is not sent a
second time. Each timeline sample has aligned `values` and
`timestamps_unix_nano` plus pprof labels decoded as attributes such as
`process.pid`, `thread.id`, and `thread.name`. Raw perf timestamps are converted
to Unix nanoseconds inside the window; unconvertible samples are omitted and
counted by `timeline_timestamp_errors`. The `--max-timeline-samples` cap applies
to this OTLP timeline as well as Firefox and does not require a Firefox file.

## FP, DWARF, and partial profiles

- `fp` captures and validates user frame-pointer stacks.
- `dwarf` uses registers, user stack bytes, and ELF unwind information.
- `auto` calibrates FP for up to 10 seconds and at least 64 samples. It accepts
  FP only when address validity is at least 90%, at least 70% of samples reach
  three frames, and no cycles are observed. Failed calibration falls back to
  DWARF when `.eh_frame` or `.debug_frame` is available.
- After a low-quality FP window, `auto` can switch permanently to DWARF. Heap
  in-use state restarts when probes are reattached.
- Without unwind tables, full DWARF stacks are unavailable. `--allow-partial`
  may emit leaf instruction-pointer data; without it, the relevant path fails.

## Allocator boundary

The supported heap probe families are intentionally narrow:

- `rust`: defined `__rust_alloc`, `__rust_alloc_zeroed`, `__rust_realloc`, and
  `__rust_dealloc` symbols.
- `system`: a mapped glibc/libc or dynamic musl (`ld-musl`) module with a
  complete, defined `malloc`, `calloc`, `realloc`, and `free` family. The target
  executable itself may provide that family for a statically linked system
  layer. `aligned_alloc` and `posix_memalign` are probed when available.

`auto` prefers the Rust family, then the supported system family. Custom
allocators and libraries without these symbols are unsupported. Heap values are
sampled estimates, and allocations that were already live before attach are
not included in in-use values.

## Exit, signals, and `exec`

- A target exit is detected through its pidfd; the current window is finalized.
  A PID target then stops. A Docker target follows the same fixed container ID,
  and a Kubernetes target follows the same fixed Pod UID/container identity;
  when it returns with a new host PID, collectors reattach in the same session
  and heap in-use values restart. A removed/replaced logical target stops
  instead of following a same-name replacement.
- SIGINT and SIGTERM stop recording after the current window is finalized. An
  interrupt during initial `auto` calibration returns an interruption error.
- If the target executes a new image, the current window ends with a warning;
  preflight, unwind mode, symbols, and collectors are refreshed for the new
  image. Auto calibration also re-preflights and restarts after an `exec`.
- Linux 5.8 and newer prefer lifecycle eBPF events. Linux 5.4-5.7, or a newer
  kernel where lifecycle eBPF attachment fails, falls back to one-second
  procfs reconciliation and records a diagnostics warning.

## Validation status

The arm64 OrbStack Linux runtime has been validated as `root`, including the
CPU FP path, DWARF fallback, stripped/no-CFI cases, system heap, and pprof
decoding. The x86_64 release currently has release-build and CLI-smoke
evidence only; native x86_64 perf/eBPF runtime validation has not been claimed.
The Linux 5.4 compatibility path still needs independent runtime validation on
a native 5.4 kernel.
