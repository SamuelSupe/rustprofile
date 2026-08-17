#[cfg(target_os = "linux")]
fn main() {
    use std::{env, path::PathBuf};

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("target architecture is set by Cargo");
    let (target, system_include) = match arch.as_str() {
        "x86_64" => ("-D__TARGET_ARCH_x86", "-I/usr/include/x86_64-linux-gnu"),
        "aarch64" => ("-D__TARGET_ARCH_arm64", "-I/usr/include/aarch64-linux-gnu"),
        other => panic!("unsupported Linux target architecture: {other}"),
    };

    let mut builder = libbpf_cargo::SkeletonBuilder::new();
    builder
        .source("bpf/heap.bpf.c")
        .obj(out.join("heap.bpf.o"))
        .clang_args([target, system_include, "-Ibpf/include"])
        .build()
        .expect("failed to build heap eBPF program");

    let mut lifecycle = libbpf_cargo::SkeletonBuilder::new();
    lifecycle
        .source("bpf/lifecycle.bpf.c")
        .obj(out.join("lifecycle.bpf.o"))
        .clang_args([target, system_include, "-Ibpf/include"])
        .build()
        .expect("failed to build lifecycle eBPF program");

    let mut off_cpu = libbpf_cargo::SkeletonBuilder::new();
    off_cpu
        .source("bpf/off_cpu.bpf.c")
        .obj(out.join("off_cpu.bpf.o"))
        .clang_args([target, system_include, "-Ibpf/include"])
        .build()
        .expect("failed to build off-CPU eBPF program");

    println!("cargo:rerun-if-changed=bpf/heap.bpf.c");
    println!("cargo:rerun-if-changed=bpf/lifecycle.bpf.c");
    println!("cargo:rerun-if-changed=bpf/off_cpu.bpf.c");
    println!("cargo:rerun-if-changed=bpf/include");
}

#[cfg(not(target_os = "linux"))]
fn main() {}
