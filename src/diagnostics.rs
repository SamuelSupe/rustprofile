use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::{AllocatorChoice, ProfileKind, UnwindMode};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Process,
    Docker,
    Kubernetes,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TargetMetadata {
    pub kind: TargetKind,
    pub pid: i32,
    pub process_start_time_ticks: u64,
    pub container_id: Option<String>,
    pub container_name: Option<String>,
    pub k8s_namespace: Option<String>,
    pub k8s_pod_name: Option<String>,
    pub k8s_pod_uid: Option<String>,
    pub k8s_container_name: Option<String>,
    pub k8s_node_name: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ModuleReport {
    pub path: PathBuf,
    pub build_id: Option<String>,
    pub has_eh_frame: bool,
    pub has_debug_frame: bool,
    pub symbol_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AllocatorReport {
    pub requested: AllocatorChoice,
    pub detected: Option<String>,
    pub module: Option<PathBuf>,
    pub complete: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CheckReport {
    pub schema_version: u32,
    pub pid: i32,
    pub target: TargetMetadata,
    pub executable: PathBuf,
    pub architecture: String,
    pub kernel_release: String,
    pub kernel_supported: bool,
    pub running_as_root: bool,
    pub thread_count: usize,
    pub modules: Vec<ModuleReport>,
    pub has_unwind_info: bool,
    pub allocator: AllocatorReport,
    pub capabilities: CapabilityReport,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CapabilityReport {
    pub perf: bool,
    pub lifecycle_bpf: bool,
    pub heap_bpf: bool,
    pub off_cpu_bpf: bool,
    pub container_cgroup: bool,
    pub cgroup_path: Option<PathBuf>,
    pub perf_map: bool,
    pub jitdump: bool,
}

impl CheckReport {
    pub fn is_recordable(&self) -> bool {
        self.errors.is_empty()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CpuWindowDiagnostics {
    pub requested_mode: Option<UnwindMode>,
    pub selected_mode: Option<UnwindMode>,
    pub fallback_reason: Option<String>,
    pub samples: u64,
    pub usable_samples: u64,
    pub cpu_nanoseconds: i64,
    pub lost_samples: u64,
    pub malformed_samples: u64,
    pub truncated_samples: u64,
    pub invalid_addresses: u64,
    pub aggregation_dropped_samples: u64,
    pub aggregation_dropped_nanoseconds: i64,
    pub average_depth: f64,
    pub symbolized_locations: u64,
    pub total_locations: u64,
    pub attributed_series: u64,
    pub thread_attribution_dropped_samples: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct EventOrderDiagnostics {
    pub reorder_window_nanos: u64,
    pub max_pending_events: usize,
    pub peak_pending_events: usize,
    pub forced_flushes: u64,
    pub late_events_dropped: u64,
    pub timeline_events_dropped: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct OffCpuWindowDiagnostics {
    pub requested: bool,
    pub enabled: bool,
    pub reason: Option<String>,
    pub switch_out_events: u64,
    pub completed_intervals: u64,
    pub incomplete_intervals: u64,
    pub nanoseconds: i64,
    pub aggregation_dropped_events: u64,
    pub ring_buffer_drops: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct FirefoxOutputDiagnostics {
    pub enabled: bool,
    pub format: Option<String>,
    pub samples: u64,
    pub dropped_samples: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct JitDiagnostics {
    pub perf_map_files: u64,
    pub jitdump_files: u64,
    pub mappings_loaded: u64,
    pub mappings_dropped: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TargetScopeDiagnostics {
    pub requested: String,
    pub effective: String,
    pub cgroup_path: Option<PathBuf>,
    pub degraded_reason: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct OutputBackpressureDiagnostics {
    pub derived_outputs_shed: bool,
    pub pending_windows: usize,
    pub files_skipped: u64,
    pub otlp_skipped: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct HeapWindowDiagnostics {
    pub allocator: Option<String>,
    pub allocation_events: u64,
    pub sampled_allocations: u64,
    pub sampled_frees: u64,
    pub alloc_objects: i64,
    pub alloc_space: i64,
    pub inuse_objects: i64,
    pub inuse_space: i64,
    pub aggregation_dropped_alloc_objects: i64,
    pub aggregation_dropped_alloc_space: i64,
    pub aggregation_dropped_inuse_objects: i64,
    pub aggregation_dropped_inuse_space: i64,
    pub live_samples: u64,
    pub ring_buffer_drops: u64,
    pub map_evictions: u64,
    pub map_update_failures: u64,
    pub pending_overwrites: u64,
    pub unfinished_returns: u64,
    pub stack_samples: u64,
    pub usable_stacks: u64,
    pub stack_failures: u64,
    pub average_depth: f64,
    pub symbolized_locations: u64,
    pub total_locations: u64,
    pub since_attach: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OtlpExportStatus {
    Disabled,
    Pending,
    Exported,
    Partial,
    Failed,
    Dropped,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OtlpExportDiagnostics {
    pub status: OtlpExportStatus,
    pub profiles: u32,
    pub attempts: u32,
    pub rejected_profiles: i64,
    pub timeline_enabled: bool,
    pub timeline_samples: u64,
    pub timeline_dropped_samples: u64,
    pub timeline_timestamp_errors: u64,
    pub error: Option<String>,
}

impl OtlpExportDiagnostics {
    pub fn disabled() -> Self {
        Self {
            status: OtlpExportStatus::Disabled,
            profiles: 0,
            attempts: 0,
            rejected_profiles: 0,
            timeline_enabled: false,
            timeline_samples: 0,
            timeline_dropped_samples: 0,
            timeline_timestamp_errors: 0,
            error: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WindowDiagnostics {
    pub schema_version: u32,
    pub session_id: String,
    pub pid: i32,
    pub target: TargetMetadata,
    pub started_unix_nanos: i64,
    pub ended_unix_nanos: i64,
    pub profiles_requested: Vec<ProfileKind>,
    pub profiles_written: Vec<ProfileKind>,
    pub allocator_probe: AllocatorReport,
    pub cpu: CpuWindowDiagnostics,
    pub heap: HeapWindowDiagnostics,
    pub off_cpu: OffCpuWindowDiagnostics,
    pub event_order: EventOrderDiagnostics,
    pub firefox: FirefoxOutputDiagnostics,
    pub jit: JitDiagnostics,
    pub scope: TargetScopeDiagnostics,
    pub output_backpressure: OutputBackpressureDiagnostics,
    pub otlp: OtlpExportDiagnostics,
    pub outputs: Vec<PathBuf>,
    pub warnings: Vec<String>,
}
