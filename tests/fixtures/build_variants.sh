#!/usr/bin/env bash
set -euo pipefail

# Build target binaries used for native Linux profiling checks. The profiler
# attaches to an existing PID, so each variant has the same busy workload but
# deliberately different unwind/symbol metadata.
source_file="$(cd "$(dirname "$0")" && pwd)/cpu_target.rs"
unwind_source="$(cd "$(dirname "$0")" && pwd)/unwind_target.c"
output_dir="${1:?usage: build_variants.sh OUTPUT_DIR}"
mkdir -p "$output_dir"

rustc --edition=2024 "$source_file" -C debuginfo=2 -C force-frame-pointers=yes \
  -o "$output_dir/fp-debug"
rustc --edition=2024 "$source_file" -C opt-level=3 -C debuginfo=0 \
  -C force-frame-pointers=no -o "$output_dir/default-release"
rustc --edition=2024 "$source_file" -C opt-level=3 -C debuginfo=0 \
  -C strip=symbols -C force-frame-pointers=no -o "$output_dir/stripped"
rustc --edition=2024 "$source_file" -C opt-level=1 -C debuginfo=0 \
  -C force-frame-pointers=no -C force-unwind-tables=no -o "$output_dir/no-unwind"

# Rust release fixtures intentionally retain the target toolchain's AArch64
# frame-pointer policy. The C pair below is a separate no-FP control used to
# prove explicit DWARF unwinding and the no-CFI rejection path without
# relabeling Rust release behavior.
cc_bin="$(command -v cc || command -v gcc || true)"
if [[ -z "$cc_bin" ]]; then
  echo "cc or gcc is required to build DWARF acceptance fixtures" >&2
  exit 1
fi
"$cc_bin" -O3 -g -fomit-frame-pointer "$unwind_source" \
  -o "$output_dir/dwarf-c"
"$cc_bin" -O3 -fomit-frame-pointer -fno-asynchronous-unwind-tables \
  -fno-unwind-tables "$unwind_source" -o "$output_dir/no-unwind-c"

# `force-unwind-tables=no` is not sufficient for every Rust/linker pair: the
# standard runtime can still contribute an .eh_frame section. Remove the
# unwind sections from this dedicated negative fixture so its check report is
# an unambiguous no-CFI case.
objcopy_bin="$(command -v objcopy || command -v llvm-objcopy || true)"
if [[ -z "$objcopy_bin" ]]; then
  echo "objcopy or llvm-objcopy is required to build no-unwind" >&2
  exit 1
fi
for no_unwind in "$output_dir/no-unwind" "$output_dir/no-unwind-c"; do
  "$objcopy_bin" \
    --remove-section .eh_frame \
    --remove-section .eh_frame_hdr \
    --remove-section .debug_frame \
    "$no_unwind"
done
