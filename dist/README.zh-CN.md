# rustprofile 0.1.0

[English](README.en.md) | [简体中文](README.zh-CN.md)

面向单个已存在 native 进程、Docker 容器或 Kubernetes 应用容器的 Linux
持续 CPU 与采样堆分析器。本文件随发行包交付。配置 endpoint 后，每个完成的
窗口还可通过 OTLP/HTTP protobuf 输出 Profiles；本地 pprof 与 diagnostics
文件始终是权威结果。

## 支持范围与前置条件

- Linux 5.8 或更新版本。
- `check` 与 `record` 都必须以 `root` 运行。
- 二进制架构必须与目标进程一致：aarch64 进程使用 aarch64 发行包，x86_64
  进程使用 x86_64 发行包。
- 发行二进制是动态 GNU/Linux PIE，不是静态二进制。运行时需要 glibc 2.38
  或更新版本，以及 `libelf.so.1`、`libz.so.1`、`libzstd.so.1`、
  `libgcc_s.so.1`、匹配的 libc 和动态加载器。发行归档不捆绑这些系统库；
  Linux 5.8 与 root 只是必要条件，并不充分。安装前可用 `ldd ./rustprofile`
  检查解析到的运行时依赖，并在主机镜像中补齐任何 `not found` 项。
- 本发行物只跟踪一个明确目标；不会启动进程，也不会跟踪子进程树或 cgroup。
- Docker 目标需要 host PID 可见性、perf/eBPF 所需权限以及
  `/var/run/docker.sock` 访问。
- Kubernetes 目标需要使用下文提供的 privileged、`hostPID` DaemonSet。

## 发行文件与 SHA256

发行归档文件名为：

```text
rustprofile-0.1.0-linux-aarch64.tar.gz
rustprofile-0.1.0-linux-x86_64.tar.gz
```

归档随附 `SHA256SUMS`。在包含这些文件的目录中，解压前先校验下载内容：

```sh
sha256sum -c SHA256SUMS
```

## 解压与安装

选择同时匹配主机和目标进程架构的归档：

```sh
tar -xzf rustprofile-0.1.0-linux-aarch64.tar.gz
sudo install -m 0755 rustprofile /usr/local/bin/rustprofile
```

x86_64 系统使用对应的 x86_64 文件名。如果归档解压到了目录，请在归档根目录
中执行 `install`。解压后，该根目录直接包含 `rustprofile`、`README.en.md` 和
`README.zh-CN.md`。

## 快速开始

先检查目标进程：

```sh
sudo rustprofile check --pid "$PID"
sudo rustprofile check --pid "$PID" --json > check.json
```

使用默认的 CPU 与堆配置，每 60 秒写出一个窗口：

```sh
sudo rustprofile record \
  --pid "$PID" \
  --duration 10m \
  --window 60s \
  --output ./profiles
```

`--duration 0` 会持续记录，直到目标退出或记录器收到 SIGINT/SIGTERM。

Docker 目标可以使用容器 ID 或名称：

```sh
sudo rustprofile check --docker-container my-api --json
sudo rustprofile record --docker-container my-api \
  --duration 10m --window 60s --output ./profiles
```

Kubernetes 目标使用 `NAMESPACE/NAME`；命令必须在与目标 Pod 同节点的 profiler
Pod 中执行。只有一个应用容器时可以省略 `--container`，多容器 Pod 必须明确
指定：

```sh
kubectl exec -n rustprofile-system "$PROFILER_POD" -- \
  rustprofile check --k8s-pod default/api --container app --json
kubectl exec -n rustprofile-system "$PROFILER_POD" -- \
  rustprofile record --k8s-pod default/api --container app \
  --duration 10m --window 60s --output /profiles
```

## 主要选项

### `check`

```text
rustprofile check (--pid PID | --docker-container ID_OR_NAME |
                   --k8s-pod NAMESPACE/NAME [--container NAME]) [--json]
                   [--symbol-dir DIR]... [--debuginfod URL]
```

- 目标选择器三选一：`--pid PID` 选择主机进程，`--docker-container
  ID_OR_NAME` 选择 Docker 容器，`--k8s-pod NAMESPACE/NAME` 选择 Kubernetes
  Pod。`--container NAME` 只能与 `--k8s-pod` 一起使用，多容器 Pod 必填。
- `--json`：以 JSON 输出带 schema 版本的预检报告。
- `--symbol-dir DIR`：可重复指定外部 ELF/调试文件目录。
- `--debuginfod URL`：显式指定 `http://` 或 `https://` 基础 URL；省略时不
  发起查询。启用后，所有 debuginfod 查询共享 Symbolizer 初始化的 30 秒总
  预算，响应流式写入临时缓存，并限制单文件最大 512 MiB。

`check` 会校验 root、内核、架构、线程数以及 perf/eBPF 访问，并报告映射
模块、build ID、unwind 段、符号和 allocator 选择；它不会启动记录会话。

### `record`

- `--pid PID`、`--docker-container ID_OR_NAME` 或 `--k8s-pod
  NAMESPACE/NAME`：三选一指定目标；Kubernetes 目标可附加
  `--container NAME`。
- `--profiles cpu,heap`：逗号分隔的 profile 类型，默认 `cpu,heap`。
- `--duration DURATION`：默认 `60s`；使用 humantime 语法，`0` 表示无限制。
- `--window DURATION`：默认 `60s`；必须大于零。
- `--unwind auto|fp|dwarf`：默认 `auto`。
- `--cpu-frequency HZ`：目标 CPU 每秒采样数，默认 `49`，范围 `1..=999`。
- `--alloc-interval BYTES`：平均堆采样间隔，默认 `512 KiB`。
- `--allocator auto|rust|system`：默认 `auto`。
- `--output DIR`：输出目录，默认 `.`。
- `--keep-windows N`：本次会话保留的窗口数，默认 `60`。
- `--max-stacks N`：默认 `65,536`；分别限制每个 CPU/heap 输出窗口中的 distinct
  stack 数。达到上限后已有 stack 继续累加，新 stack 省略。
- `--svg`：默认关闭；为每个完成的 CPU 和 heap window 写出自包含的静态 SVG
  火焰图。
- `--allow-partial`：profile 能力不可用时允许继续输出支持的子集或仅 CPU
  叶子数据。
- `--symbol-dir DIR` 与 `--debuginfod URL`：与 `check` 相同的符号选项。
- `--otlp-endpoint URL`：可选 OTLP/HTTP Profiles endpoint；省略则不导出。
- `--otlp-header KEY=VALUE`：可重复指定 OTLP header。
- `--otlp-timeout DURATION`：每次 OTLP 请求超时，默认 `10s`；环境变量值使用
  毫秒。
- `--otlp-compression none|gzip`：请求压缩，默认 `gzip`。
- `--otlp-ca PATH`：OTLP HTTPS endpoint 的附加 PEM CA 文件。
- `--resource-attribute KEY=VALUE`：可重复指定 OTLP resource attribute。

非 root、内核过旧、架构不匹配或 perf 访问失败等预检错误仍然是致命错误。
如果 partial 模式禁用了所有请求的 profile，记录也会失败。

## 输出文件

每个完成的窗口会在 `--output` 下写出类似以下文件：

```text
cpu-<session>-<index>-<start-unix-nanos>.pb.gz
cpu-<session>-<index>-<start-unix-nanos>.svg       （使用 --svg 时）
heap-<session>-<index>-<start-unix-nanos>.pb.gz
heap-<session>-<index>-<start-unix-nanos>.svg       （使用 --svg 时）
diagnostics-<session>-<index>-<start-unix-nanos>.json
```

CPU 和 heap 文件是 gzip 压缩的 pprof profile protobuf。CPU 样本包含
`samples/count` 与 `cpu/nanoseconds`；heap 样本包含 `alloc_objects/count`、
`alloc_space/bytes`、`inuse_objects/count`、`inuse_space/bytes`，其中 in-use
只包含 attach 之后观测到的采样分配。样本包含 `process.pid` 以及可用的
Docker/Kubernetes 身份 labels。diagnostics JSON 为 schema 版本 2，包含
`target`、输出路径、warnings、allocator probe 信息、CPU 丢失/格式错误计数、
CPU 纳秒总值、`aggregation_dropped_samples`、`aggregation_dropped_nanoseconds`、
heap 四类导出总值、`since_attach` 以及
`aggregation_dropped_alloc_objects`、`aggregation_dropped_alloc_space`、
`aggregation_dropped_inuse_objects`、`aggregation_dropped_inuse_space`。达到
stack 上限时，window warnings 也会标记 aggregation drop。即使新 stack 被
省略，heap live/free state 仍继续跟踪。启用 OTLP 时，`otlp.status` 还会报告
`pending`、`exported`、`partial`、`failed` 或 `dropped`，并包含尝试次数、拒绝数量和脱敏错误。

`--max-stacks` 分别作用于每个 CPU 和 heap 输出窗口。达到上限后已有 stack
继续累加，只有新出现的 distinct stack 会从 pprof、OTLP 和可选 SVG 输出中省略。

使用 `--svg` 时，异步 output worker 还会为每个请求的 CPU 或 heap profile
原子生成自包含的静态火焰图。CPU SVG 的帧宽度按 `cpu/nanoseconds` 分配，heap
SVG 的帧宽度按 `inuse_space/bytes` 分配。SVG 不含脚本，只是派生可视化；pprof
与 OTLP 仍是权威的机器可读格式。渲染最多保留 100,000 个帧/节点；超过上限时
只截断 SVG，并在图内标示截断，不影响 pprof 或 OTLP。SVG 随窗口输出集合一起由
`--keep-windows` 保留或删除。SVG 渲染会直接流式写入原子临时文件，不会先在内存中
构造完整 SVG 文本。

output worker 会以原子方式发布每个输出文件。如果 CPU/heap pprof、可选 SVG 或
diagnostics 任一生成失败，会删除该窗口已经发布的文件。只有 retention 成功后
才会打印 `wrote`；`--keep-windows` 只删除本次记录会话中过期的文件（包括可选
SVG），不会清理更早会话的文件。

## Docker 与 Kubernetes 部署

在源码 checkout 中使用 `Dockerfile` 构建 Linux 镜像。Docker profiler 必须看到主机
PID namespace 和 Docker API socket：

```sh
docker build -t rustprofile:0.1.0 .
docker run --rm --privileged --pid=host \
  --mount type=bind,src=/var/run/docker.sock,dst=/var/run/docker.sock,readonly \
  --mount type=bind,src="$PWD/profiles",dst=/profiles \
  rustprofile:0.1.0 record --docker-container my-api \
  --duration 10m --window 60s --output /profiles
```

Docker socket 是主机控制边界。即使以只读方式挂载 socket，拥有 socket 访问权
的软件仍可通过 Docker API 请求等同主机 root 的操作；`--privileged` 与
`--pid=host` 也属于 perf/eBPF 和主机进程检查所需的节点级权限。在 Linux 中，
`record` 会检查 `/sys/kernel/tracing` 的 tracefs；缺失时，root 会尝试在该路径
挂载。挂载需要 `CAP_SYS_ADMIN`，示例中的 `--privileged` 会提供该能力。缺少
能力或挂载失败时，命令会以明确错误退出，例如
`tracefs is not mounted; run rustprofile with CAP_SYS_ADMIN/--privileged or
mount tracefs at /sys/kernel/tracing`。

在源码 checkout 中使用 `deploy/kubernetes/rustprofile.yaml` 部署 Kubernetes
DaemonSet：

```sh
kubectl apply -f deploy/kubernetes/rustprofile.yaml
kubectl get pods -n rustprofile-system \
  -l app.kubernetes.io/name=rustprofile -o wide
```

如需给 DaemonSet 配置 OTLP，请先编辑 `deploy/kubernetes/otel-config.example.yaml`
中的 endpoint、认证和其他值再应用。替换示例中的占位凭据，不要把真实 Secret
提交到源码。如果接收端使用私有 CA，可创建可选的 `rustprofile-otel-ca` Secret，
包含 `ca.crt` PEM key，并在 DaemonSet 环境中设置
`OTEL_EXPORTER_OTLP_PROFILES_CERTIFICATE=/etc/rustprofile/otel/ca.crt`。

选择与目标 Pod 同节点的 profiler Pod，再使用前文的 `kubectl exec`。清单创建
仅允许 `get` Pod 的只读 RBAC，注入 `NODE_NAME`，设置 `hostPID`、privileged、
Unconfined seccomp，并把主机 `/var/lib/rustprofile` 挂载为 `/profiles`。
DaemonSet 没有控制 HTTP endpoint；通过 `kubectl exec` 发起每次明确的检查或
记录。清单中的 `privileged: true` 提供在 `/sys/kernel/tracing` 挂载缺失 tracefs
所需的 `CAP_SYS_ADMIN`；请保留该设置，或自行提供该能力和可用的 tracefs 挂载，
否则 `record` 会以明确的挂载错误退出。初次解析要求目标正在运行。解析器固定 Docker container ID 或
Kubernetes Pod UID：同一身份重启时用新的 host PID 重新挂载；删除或替换后的
目标不会按名称跟随。`--duration 0` 会一直等待，直到中断或逻辑目标消失。

Docker inspect 与 Kubernetes Pod API 控制面请求的超时均为 5 秒，响应超过
4 MiB 会被拒绝。该限制只作用于目标身份解析，不限制本地 profile 文件或
OTLP payload。

## OTLP Profiles 输出

Exporter 固定使用 `opentelemetry-proto v1.11.0`，通过 OTLP/HTTP
`http/protobuf` 向 `/v1development/profiles` 发送 Development Profiles 信号，
Content-Type 为 `application/x-protobuf`。默认 gzip，使用
`--otlp-compression none` 可关闭压缩。Profiles 信号目前属于
Development/Alpha，接收端应与 v1.11.0 兼容。

配置优先级为 CLI、Profiles 专用环境变量、通用 OTLP 环境变量：

```text
--otlp-endpoint URL                         OTEL_EXPORTER_OTLP_PROFILES_ENDPOINT
                                            OTEL_EXPORTER_OTLP_ENDPOINT + /v1development/profiles
--otlp-header KEY=VALUE（可重复）            OTEL_EXPORTER_OTLP_PROFILES_HEADERS
                                            OTEL_EXPORTER_OTLP_HEADERS
--otlp-timeout DURATION                     OTEL_EXPORTER_OTLP_PROFILES_TIMEOUT
                                            OTEL_EXPORTER_OTLP_TIMEOUT（毫秒）
--otlp-compression none|gzip                OTEL_EXPORTER_OTLP_PROFILES_COMPRESSION
                                            OTEL_EXPORTER_OTLP_COMPRESSION
--otlp-ca PATH                              OTEL_EXPORTER_OTLP_PROFILES_CERTIFICATE
                                            OTEL_EXPORTER_OTLP_CERTIFICATE
--resource-attribute KEY=VALUE（可重复）      OTEL_RESOURCE_ATTRIBUTES
                                            OTEL_SERVICE_NAME
```

`OTEL_EXPORTER_OTLP_PROFILES_PROTOCOL` 或 `OTEL_EXPORTER_OTLP_PROTOCOL` 只接受
`http/protobuf`。header 与 resource attribute 环境变量使用逗号分隔的
`KEY=VALUE`，header 值不会写入 diagnostics。HTTPS 使用系统根证书，可追加
PEM CA；不支持 mTLS。endpoint 必须是没有内嵌凭据的 HTTP(S) URL；没有 endpoint
时不会发出网络请求。

每个已完成的本地窗口编码为一个 OTLP 请求，每个 pprof sample type 对应一个
OTLP Profile，并共享 dictionary。Resource 包含 `service.name`、可执行文件路径
与名称、整数型 `process.pid`、目标类型和可用的 Docker/Kubernetes 身份。仅瞬时
传输错误（I/O、超时、DNS，以及 HTTP 协议/连接失败）会重试；无效 URI/header、
TLS 或证书等确定性配置错误立即失败。HTTP 408、429、502、503、504 最多重试
五次并退避，整数秒格式的 `Retry-After` 会被遵守但最多等待 30 秒。OTLP 响应体上限为 1 MiB。每个窗口
的 gzip 请求体只准备一次，重试时复用同一请求体。队列最多容纳四个窗口；导出
失败不会删除本地文件，diagnostics 会记录 `pending`、`exported`、`partial`、
`failed` 或 `dropped`。关闭时停止重试，尚未 flush 的排队窗口标记为 `failed`，
以保证退出有界。
没有持久化的 OTLP 磁盘 spool，也不会自动稍后重放；请保留本地文件用于恢复。

## FP、DWARF 与 partial profile

- `fp` 捕获并校验用户态 frame-pointer 栈。
- `dwarf` 使用寄存器、用户栈字节和 ELF unwind 信息。
- `auto` 最多校准 10 秒且至少收集 64 个样本。只有地址有效率至少 90%、
  至少 70% 样本达到三层栈且没有循环时才接受 FP。校准失败且存在
  `.eh_frame` 或 `.debug_frame` 时回退到 DWARF。
- FP 窗口质量过低时，`auto` 可永久切换到 DWARF；重新挂载 heap probe
  后，in-use 状态会重新开始。
- 没有 unwind 表时无法得到完整 DWARF 栈；使用 `--allow-partial` 可以输出
  叶子指令指针，否则相关路径会失败。

## Allocator 支持边界

支持的 heap probe 家族范围是有意收窄的：

- `rust`：已定义的 `__rust_alloc`、`__rust_alloc_zeroed`、`__rust_realloc`
  与 `__rust_dealloc` 符号。
- `system`：映射的 glibc/libc 或动态 musl（`ld-musl`）模块，并且具有完整、
  已定义的 `malloc`、`calloc`、`realloc`、`free` 家族。目标可执行文件自身
  具备该家族时，也可覆盖静态链接的 system 层；存在时还会探测
  `aligned_alloc` 与 `posix_memalign`。

`auto` 优先 Rust 家族，再选择支持的 system 家族。自定义 allocator 以及
缺少这些符号的库不受支持。heap 值是采样估计，attach 之前已经存活的分配
不会计入 in-use。

## 退出、信号与 `exec`

- 通过 pidfd 检测目标退出；当前窗口会完成写出。PID 目标随后停止；Docker
  目标跟踪同一 container ID，Kubernetes 目标跟踪同一 Pod UID/container
  identity。目标以新的 host PID 返回时，在同一 session 中重新挂载，heap
  in-use 状态重新开始；被删除或替换的逻辑目标不会跟踪同名新目标。
- SIGINT 与 SIGTERM 会在完成当前窗口后停止记录。初始 `auto` 校准期间收到
  中断则返回中断错误。
- 目标执行新镜像时，当前窗口以 warning 结束；随后为新镜像刷新预检、unwind
  模式、符号和 collectors。`auto` 校准期间发生 `exec` 同样会重新预检并重启
  校准。

## 验证状态

已在 arm64 OrbStack Linux、以 `root` 运行时验证 runtime，包括 CPU FP、DWARF
回退、stripped/no-CFI 场景、system heap 和 pprof 解码。x86_64 目前只有发行构建
与 CLI smoke 证据；没有宣称已完成 native x86_64 perf/eBPF runtime 验证。
