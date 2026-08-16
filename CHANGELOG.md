# Changelog

All notable changes to `rustprofile` are documented here.

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
