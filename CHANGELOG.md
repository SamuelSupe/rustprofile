# Changelog

All notable changes to `rustprofile` are documented here.

## [0.2.1] - 2026-08-17

- Added bounded off-CPU profiling, including sched-switch collection,
  timestamp ordering, window diagnostics, and static SVG/Firefox outputs.
- Added `launch` for attach-before-run collection, with cgroup-v2 descendant
  tracking where available.
- Added `import` for existing perf.data/simpleperf captures and `serve` for
  Firefox profile viewing, symbol/source/assembly APIs, and profile galleries.
- Added bounded CPU timelines, Firefox JSON/JSLB output, richer capability and
  backpressure diagnostics, and the corresponding CLI/integration coverage.

## [0.2.0] - 2026-08-16

- Lowered the CPU profiling baseline to Linux 5.4 by making lifecycle eBPF
  notifications optional and retaining one-second procfs reconciliation as the
  compatibility path.
- Declared and enforced Linux 5.12 as the heap profiling baseline because the
  heap BPF program uses ring-buffer helpers and fetch-returning BPF atomics.
- Moved release builds to Debian bullseye so the distributed binaries can run
  with glibc 2.31 instead of inheriting a newer build-host ABI.

## [0.1.0] - 2026-08-16

Initial public release.

- Continuous per-thread CPU and sampled Rust/libc heap profiling on Linux.
- PID, Docker container, and Kubernetes application-container target selection.
- Automatic frame-pointer calibration with DWARF fallback and explicit partial mode.
- Gzip-compressed pprof profiles plus schema-versioned diagnostics JSON.
- Optional static SVG flame graphs and OTLP/HTTP Profiles export.
- Atomic window publication, bounded retention, target lifecycle handling, and
  explicit preflight checks for perf/eBPF, symbols, allocators, and privileges.
- Linux release images and prebuilt aarch64/x86_64 archives.

The 0.1 release is an early public version. Native perf/eBPF behavior remains
kernel-, architecture-, and deployment-sensitive; see the validation limits in
the [README](README.md) before using it in production.
