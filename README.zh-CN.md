# rustprofile

[English](README.md) | [简体中文](README.zh-CN.md)

`rustprofile` 是面向 Linux 的持续 CPU 与采样堆分析器，支持已有 native
进程、Docker 容器和 Kubernetes 应用容器。它输出 gzip 压缩的 pprof profile、
诊断 JSON，并可选导出静态 SVG 火焰图和 OTLP Profiles。

## 核心能力

- 按线程采集 CPU 样本和 Rust/libc allocator 的采样堆数据。
- 支持 `--pid`、`--docker-container` 和 `--k8s-pod` 三种明确目标。
- `auto` frame-pointer 校准失败时自动回退到 DWARF。
- 每个窗口原子发布，支持有界保留和目标进程生命周期变化。
- `--svg` 输出无脚本的静态火焰图；本地 pprof 和 diagnostics 始终是权威结果。
- 可选通过 OTLP/HTTP Profiles 输出到兼容的观测系统。

## 前置条件

- Linux 5.8 或更新版本。
- native x86_64 或 aarch64，且 profiler 架构必须与目标进程一致。
- `check` 和 `record` 需要以 `root` 运行。
- 从源码构建需要 Rust/Cargo 1.88+、clang/LLVM、Linux UAPI headers 和 libelf。

## 快速开始

```sh
cargo build --release

sudo target/release/rustprofile check --pid "$PID" --json
sudo target/release/rustprofile record \
  --pid "$PID" \
  --duration 10m \
  --window 60s \
  --svg \
  --output ./profiles
```

Docker 容器使用容器 ID 或名称：

```sh
sudo target/release/rustprofile check --docker-container my-api --json
sudo target/release/rustprofile record \
  --docker-container my-api \
  --duration 10m --window 60s --output ./profiles
```

Kubernetes 使用同节点上的 profiler DaemonSet，并以 `NAMESPACE/NAME` 指定 Pod：

```sh
kubectl exec -n rustprofile-system "$PROFILER_POD" -- \
  rustprofile record --k8s-pod default/api --container app \
  --duration 10m --window 60s --svg --output /profiles
```

## 输出文件

每个完成的窗口可以写出：

```text
cpu-<session>-<index>-<start-unix-nanos>.pb.gz
cpu-<session>-<index>-<start-unix-nanos>.svg
heap-<session>-<index>-<start-unix-nanos>.pb.gz
heap-<session>-<index>-<start-unix-nanos>.svg
diagnostics-<session>-<index>-<start-unix-nanos>.json
```

启用 `--svg` 时，frame 宽度表示采样 CPU 时间或 heap in-use bytes。示例：

![rustprofile CPU flame graph](docs/profiling-example.svg)

## 部署与限制

Docker profiler 需要 host PID namespace、Docker socket、`--privileged` 以及
perf/eBPF 所需权限。Kubernetes DaemonSet 使用 `hostPID`、privileged security
context 和只读 Pod RBAC；这些都是节点级权限边界。

工具只跟踪一个明确目标，不启动目标进程，不跟踪子进程树或 cgroup，也不采集
off-CPU 时间和 kernel stacks。自定义 allocator、OTLP gRPC、mTLS 和未验证的
生产内核不在当前支持范围内。

完整的命令选项、OTLP 配置、生命周期语义和发行包安装说明见
[发行包中文手册](dist/README.zh-CN.md)。
