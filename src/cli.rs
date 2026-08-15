use std::{path::PathBuf, time::Duration};

use clap::{ArgGroup, Args, Parser, Subcommand};

use crate::config::{
    AllocatorChoice, DEFAULT_ALLOC_INTERVAL, DEFAULT_CPU_FREQUENCY, DEFAULT_MAX_STACKS,
    DEFAULT_MAX_THREADS, ProfileKind, UnwindMode, parse_byte_size, parse_duration,
};

#[derive(Debug, Parser)]
#[command(name = "rustprofile", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Inspect whether a process can be profiled without attaching probes.
    Check(CheckArgs),
    /// Continuously record CPU and sampled heap profiles.
    Record(RecordArgs),
}

#[derive(Clone, Debug, Args)]
pub struct SymbolArgs {
    /// Additional directory containing ELF or separate debug files.
    #[arg(long = "symbol-dir")]
    pub symbol_dirs: Vec<PathBuf>,

    /// Explicit debuginfod base URL. No network lookup occurs when omitted.
    #[arg(long)]
    pub debuginfod: Option<String>,
}

#[derive(Clone, Debug, Args)]
#[command(group(
    ArgGroup::new("target")
        .required(true)
        .multiple(false)
        .args(["pid", "docker_container", "k8s_pod"])
))]
pub struct TargetArgs {
    /// Existing host process ID to profile.
    #[arg(long, value_parser = clap::value_parser!(i32).range(1..))]
    pub pid: Option<i32>,

    /// Docker container ID or name.
    #[arg(long)]
    pub docker_container: Option<String>,

    /// Kubernetes pod in NAMESPACE/NAME form.
    #[arg(long)]
    pub k8s_pod: Option<String>,

    /// Kubernetes application container name; required for multi-container pods.
    #[arg(
        long,
        requires = "k8s_pod",
        conflicts_with_all = ["pid", "docker_container"]
    )]
    pub container: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct OtlpArgs {
    /// OTLP/HTTP Profiles endpoint. Export is disabled when no endpoint is configured.
    #[arg(long)]
    pub otlp_endpoint: Option<String>,

    /// Additional OTLP HTTP header in KEY=VALUE form.
    #[arg(long = "otlp-header")]
    pub otlp_headers: Vec<String>,

    /// Timeout for each OTLP export attempt.
    #[arg(long, value_parser = parse_duration)]
    pub otlp_timeout: Option<Duration>,

    /// OTLP request compression.
    #[arg(long, value_enum)]
    pub otlp_compression: Option<crate::otlp::OtlpCompression>,

    /// Additional PEM certificate authority for the OTLP endpoint.
    #[arg(long)]
    pub otlp_ca: Option<PathBuf>,

    /// Additional OpenTelemetry resource attribute in KEY=VALUE form.
    #[arg(long = "resource-attribute")]
    pub resource_attributes: Vec<String>,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    /// Emit the report as JSON.
    #[arg(long)]
    pub json: bool,

    #[command(flatten)]
    pub symbols: SymbolArgs,
}

#[derive(Debug, Args)]
pub struct RecordArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    #[arg(skip)]
    pub pid: i32,

    /// Profiles to collect.
    #[arg(long, value_enum, value_delimiter = ',', default_values_t = [ProfileKind::Cpu, ProfileKind::Heap])]
    pub profiles: Vec<ProfileKind>,

    /// Total recording duration. Zero records until interrupted or target exit.
    #[arg(long, default_value = "60s", value_parser = parse_duration)]
    pub duration: Duration,

    /// Output window duration.
    #[arg(long, default_value = "60s", value_parser = parse_duration)]
    pub window: Duration,

    /// User stack unwinding strategy.
    #[arg(long, value_enum, default_value_t = UnwindMode::Auto)]
    pub unwind: UnwindMode,

    /// CPU samples per second of target CPU time.
    #[arg(long, default_value_t = DEFAULT_CPU_FREQUENCY, value_parser = clap::value_parser!(u32).range(1..=999))]
    pub cpu_frequency: u32,

    /// Mean allocation sampling interval in bytes.
    #[arg(long, default_value_t = DEFAULT_ALLOC_INTERVAL, value_parser = parse_byte_size)]
    pub alloc_interval: u64,

    /// Allocator probe family.
    #[arg(long, value_enum, default_value_t = AllocatorChoice::Auto)]
    pub allocator: AllocatorChoice,

    /// Directory for pprof and diagnostics windows.
    #[arg(long, default_value = ".")]
    pub output: PathBuf,

    /// Number of windows retained for this recording session.
    #[arg(long, default_value_t = 60, value_parser = parse_positive_usize)]
    pub keep_windows: usize,

    /// Maximum distinct stacks retained in each CPU or heap output window.
    #[arg(long, default_value_t = DEFAULT_MAX_STACKS, value_parser = parse_positive_usize)]
    pub max_stacks: usize,

    /// Also write self-contained SVG flame graphs for completed profile windows.
    #[arg(long)]
    pub svg: bool,

    /// Continue with supported profile types or leaf-only CPU data.
    #[arg(long)]
    pub allow_partial: bool,

    /// Maximum target thread count accepted by the per-thread perf backend.
    #[arg(long, default_value_t = DEFAULT_MAX_THREADS, hide = true)]
    pub max_threads: usize,

    #[command(flatten)]
    pub symbols: SymbolArgs,

    #[command(flatten)]
    pub otlp: OtlpArgs,
}

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    let value = value.parse::<usize>().map_err(|error| error.to_string())?;
    if value == 0 {
        return Err("value must be greater than zero".to_owned());
    }
    Ok(value)
}
