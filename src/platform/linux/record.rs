use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    fs::{self, File},
    os::fd::{AsRawFd, FromRawFd},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use crossbeam_channel::TrySendError;
use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded, unbounded};

use super::{
    event::PerfEventSorter,
    heap::HeapCollector,
    lifecycle::LifecycleNotifier,
    off_cpu::OffCpuCollector,
    perf::{FpQuality, PerfBatch, PerfCollector, PerfSampleData},
};
use crate::otlp::{ExportClient, ExportPayload, MappingHashCache, OtlpConfig, encode_profiles};
use crate::{
    cli::RecordArgs,
    config::{FirefoxProfileFormat, ProfileKind, UnwindMode},
    diagnostics::{
        CheckReport, CpuWindowDiagnostics, EventOrderDiagnostics, FirefoxOutputDiagnostics,
        HeapWindowDiagnostics, JitDiagnostics, OffCpuWindowDiagnostics, OtlpExportDiagnostics,
        OtlpExportStatus, OutputBackpressureDiagnostics, TargetScopeDiagnostics, WindowDiagnostics,
    },
    firefox::write_firefox_profile,
    maps::{ExecutableRanges, MapEntry, read_process_maps},
    pprof::{
        build_cpu_timeline_profile, sync_directory, write_cpu_profile, write_heap_profile,
        write_json_atomic, write_off_cpu_profile, write_raw_cpu_profile, write_raw_off_cpu_profile,
    },
    process::{self, file_identity},
    profile::{
        AttributedStack, CpuValues, Frame, HeapValues, OffCpuValues, Stack, TimedStackSample,
        has_address_cycle,
    },
    svg::{FlameValue, write_flamegraph},
    symbol::{Symbolizer, jit_artifact_counts},
    target::{TargetResolver, TargetState},
    unwind::{DwarfUnwinder, require_native_architecture},
};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);
const FP_CALIBRATION_LIMIT: Duration = Duration::from_secs(10);

pub fn record(
    mut args: RecordArgs,
    mut preflight: CheckReport,
    mut target: TargetResolver,
) -> Result<()> {
    require_native_architecture(&preflight.architecture)?;
    fs::create_dir_all(&args.output)
        .with_context(|| format!("failed to create {}", args.output.display()))?;
    let wants_cpu = args.profiles.contains(&ProfileKind::Cpu);
    let wants_heap = args.profiles.contains(&ProfileKind::Heap);
    let wants_off_cpu = args.profiles.contains(&ProfileKind::OffCpu);
    let otlp_config = OtlpConfig::from_args(&args.otlp)?;
    let otlp_enabled = otlp_config.is_some();
    let mut pidfd = PidFd::open(args.pid)?;
    let stopped = signal_flag()?;
    let session_id = format!("{:x}-{}", unix_nanos(), args.pid);
    let mut window_index = 0_u64;
    let mut target_exited = false;
    let mut cgroup_path = args.launch_cgroup.clone().or(target.cgroup_path()?);
    let launch_auto = args.resume_pid.is_some() && args.unwind == UnwindMode::Auto;

    let (mut selected_mode, mut fallback_reason) = if launch_auto {
        (
            UnwindMode::Fp,
            Some(
                "launch starts with frame-pointer sampling and calibrates in completed windows"
                    .to_owned(),
            ),
        )
    } else {
        select_unwind_mode(&args, &mut preflight, &pidfd, &stopped)?
    };
    let symbolizer = Symbolizer::for_process(
        args.pid,
        &args.symbols.symbol_dirs,
        args.symbols.debuginfod.as_deref(),
    )?;
    let mut output_writer = OutputWriter::start(symbolizer, args.keep_windows, otlp_config)?;
    let mut collectors = Collectors::start(
        &args,
        &preflight,
        selected_mode,
        wants_cpu,
        wants_heap,
        wants_off_cpu,
        cgroup_path.clone(),
    )?;
    if let Some(pid) = args.resume_pid.take() {
        // SAFETY: kill with SIGCONT does not access memory and targets the child PID
        // created by the launch path after all collectors have attached.
        if unsafe { libc::kill(pid, libc::SIGCONT) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to continue suspended launch target");
        }
    }
    let recording_started = Instant::now();
    let recording_deadline = if args.duration.is_zero() {
        None
    } else {
        Some(
            recording_started
                .checked_add(args.duration)
                .context("--duration is too large")?,
        )
    };
    let mut exe_identity = executable_identity(args.pid)?;
    let mut maps = read_process_maps(args.pid)?;

    loop {
        if stopped.load(Ordering::Relaxed) || target_exited || deadline_reached(recording_deadline)
        {
            break;
        }

        let window_started_at = Instant::now();
        let clock_anchor = PerfClockAnchor::capture()?;
        let window_started_nanos = clock_anchor.unix_nanos;
        let natural_window_end = window_started_at
            .checked_add(args.window)
            .context("--window is too large")?;
        let window_deadline = recording_deadline
            .map(|deadline| deadline.min(natural_window_end))
            .unwrap_or(natural_window_end);
        let mut cpu_window = CpuWindow::new(
            args.pid,
            args.max_stacks,
            args.max_threads,
            args.firefox_profile.is_some() || args.otlp_timeline,
            args.max_timeline_samples,
        );
        let mut window_warnings = std::mem::take(&mut collectors.warnings);
        let mut last_reconcile = Instant::now();
        let mut exec_detected = false;

        while Instant::now() < window_deadline && !stopped.load(Ordering::Relaxed) {
            output_writer.check_completed()?;
            if pidfd.exited()? {
                target_exited = true;
                window_warnings.push(
                    "target process exited; this window ended at the process boundary".to_owned(),
                );
                break;
            }
            let lifecycle = collectors.lifecycle.consume()?;
            if lifecycle.exec {
                exec_detected = true;
                window_warnings.push(
                    "target executed a new image; this window ended at the exec boundary"
                        .to_owned(),
                );
                break;
            }
            if lifecycle.thread_change
                && let Err(error) = reconcile_collectors(
                    collectors.cpu.as_mut(),
                    collectors.off_cpu.as_mut(),
                    cgroup_path.as_deref(),
                    &args,
                )
            {
                if pidfd.exited()? {
                    target_exited = true;
                    break;
                }
                return Err(error);
            }
            if let Some(cpu) = collectors.cpu.as_mut() {
                cpu.wait_and_ingest(&mut cpu_window, args.allow_partial, args.cpu_frequency)?;
            } else {
                thread::sleep(POLL_INTERVAL);
            }
            if let Some(heap) = collectors.heap.as_mut() {
                heap.drain()?;
            }
            if let Some(off_cpu) = collectors.off_cpu.as_mut() {
                off_cpu.drain()?;
            }

            if last_reconcile.elapsed() >= RECONCILE_INTERVAL {
                last_reconcile = Instant::now();
                let identity = match executable_identity(args.pid) {
                    Ok(identity) => identity,
                    Err(_) if pidfd.exited()? => {
                        target_exited = true;
                        break;
                    }
                    Err(error) => return Err(error),
                };
                if identity != exe_identity {
                    exec_detected = true;
                    window_warnings.push(
                        "target executed a new image; this window ended at the exec boundary"
                            .to_owned(),
                    );
                    break;
                }
                if let Err(error) = reconcile_collectors(
                    collectors.cpu.as_mut(),
                    collectors.off_cpu.as_mut(),
                    cgroup_path.as_deref(),
                    &args,
                ) {
                    if pidfd.exited()? {
                        target_exited = true;
                        break;
                    }
                    return Err(error);
                }
                let refreshed_maps = match read_process_maps(args.pid) {
                    Ok(maps) => maps,
                    Err(_) if pidfd.exited()? => {
                        target_exited = true;
                        break;
                    }
                    Err(error) => return Err(error),
                };
                if refreshed_maps != maps {
                    let unwind_maps_changed = unwind_maps_changed(&maps, &refreshed_maps);
                    maps = refreshed_maps;
                    if unwind_maps_changed {
                        let refreshed_symbolizer = (|| {
                            if let Some(cpu) = collectors.cpu.as_mut() {
                                cpu.refresh_root(args.pid, &maps)?;
                            }
                            if let Some(heap) = collectors.heap.as_mut() {
                                heap.refresh_unwinder(args.pid, &maps)?;
                            }
                            Symbolizer::from_maps(
                                args.pid,
                                &maps,
                                &args.symbols.symbol_dirs,
                                args.symbols.debuginfod.as_deref(),
                            )
                        })();
                        let refreshed_symbolizer = match refreshed_symbolizer {
                            Ok(symbolizer) => symbolizer,
                            Err(_) if pidfd.exited()? => {
                                target_exited = true;
                                break;
                            }
                            Err(error) => return Err(error),
                        };
                        output_writer.replace_symbolizer(refreshed_symbolizer)?;
                    }
                }
            }
        }

        if let Some(cpu) = collectors.cpu.as_mut() {
            cpu.drain_and_ingest(&mut cpu_window, args.allow_partial, args.cpu_frequency)?;
        }
        let (heap_profile, mut heap_diagnostics) = match collectors.heap.as_mut() {
            Some(heap) => {
                let (profile, diagnostics) = heap.snapshot_window()?;
                (Some(profile), diagnostics)
            }
            None => (
                None,
                HeapWindowDiagnostics {
                    allocator: preflight.allocator.detected.clone(),
                    ..Default::default()
                },
            ),
        };
        let (off_cpu_profile, off_cpu_diagnostics) = match collectors.off_cpu.as_mut() {
            Some(off_cpu) => {
                let (profile, diagnostics) = off_cpu.snapshot_window()?;
                (Some(profile), diagnostics)
            }
            None => (
                None,
                OffCpuWindowDiagnostics {
                    requested: wants_off_cpu,
                    reason: collectors.off_cpu_failure.clone(),
                    ..Default::default()
                },
            ),
        };
        if let Some(profile) = heap_profile.as_ref() {
            for values in profile.values() {
                heap_diagnostics.alloc_objects = heap_diagnostics
                    .alloc_objects
                    .saturating_add(values.alloc_objects);
                heap_diagnostics.alloc_space = heap_diagnostics
                    .alloc_space
                    .saturating_add(values.alloc_space);
                heap_diagnostics.inuse_objects = heap_diagnostics
                    .inuse_objects
                    .saturating_add(values.inuse_objects);
                heap_diagnostics.inuse_space = heap_diagnostics
                    .inuse_space
                    .saturating_add(values.inuse_space);
            }
        }
        let window_ended_nanos = unix_nanos();
        let duration_nanos = window_ended_nanos.saturating_sub(window_started_nanos);
        let mut cpu_diagnostics =
            cpu_window.diagnostics(args.unwind, selected_mode, fallback_reason.clone());
        cpu_diagnostics.attributed_series = cpu_window.samples.len() as u64;
        if cpu_diagnostics.aggregation_dropped_samples != 0 {
            window_warnings.push(format!(
                "CPU aggregation reached --max-stacks={}; {} valid samples were omitted from profile output",
                args.max_stacks, cpu_diagnostics.aggregation_dropped_samples
            ));
        }
        let heap_aggregation_drops = heap_diagnostics
            .aggregation_dropped_alloc_objects
            .saturating_add(heap_diagnostics.aggregation_dropped_inuse_objects);
        if heap_aggregation_drops != 0 {
            window_warnings.push(format!(
                "heap aggregation reached --max-stacks={}; omitted values are reported in heap aggregation_dropped_* diagnostics",
                args.max_stacks
            ));
        }
        let heap_fp_ratio = (heap_diagnostics.stack_samples != 0)
            .then(|| heap_diagnostics.usable_stacks as f64 / heap_diagnostics.stack_samples as f64);
        let fp_usable_ratio = [
            (cpu_window.total_samples != 0)
                .then(|| cpu_window.usable_samples as f64 / cpu_window.total_samples as f64),
            heap_fp_ratio,
        ]
        .into_iter()
        .flatten()
        .reduce(f64::min);
        let low_fp_quality = args.unwind == UnwindMode::Auto
            && selected_mode == UnwindMode::Fp
            && fp_usable_ratio.is_some_and(|ratio| ratio < 0.90);

        let basename = format!(
            "{}-{:06}-{}",
            session_id, window_index, window_started_nanos
        );
        let cpu_path = args.output.join(format!("cpu-{basename}.pb.gz"));
        let heap_path = args.output.join(format!("heap-{basename}.pb.gz"));
        let off_cpu_path = args.output.join(format!("off-cpu-{basename}.pb.gz"));
        let cpu_svg_path = args
            .svg
            .then(|| args.output.join(format!("cpu-{basename}.svg")));
        let heap_svg_path = args
            .svg
            .then(|| args.output.join(format!("heap-{basename}.svg")));
        let off_cpu_svg_path = args
            .svg
            .then(|| args.output.join(format!("off-cpu-{basename}.svg")));
        let diagnostics_path = args.output.join(format!("diagnostics-{basename}.json"));
        let firefox_path = args.firefox_profile.map(|format| {
            let extension = match format {
                FirefoxProfileFormat::Json => "json.gz",
                FirefoxProfileFormat::Jslb => "jslb.gz",
            };
            args.output.join(format!("firefox-{basename}.{extension}"))
        });
        let mut outputs = Vec::new();
        let mut written = Vec::new();
        let timeline_samples = std::mem::take(&mut cpu_window.timeline);
        let cpu_output = wants_cpu.then(|| {
            outputs.push(cpu_path.clone());
            if let Some(path) = cpu_svg_path.as_ref() {
                outputs.push(path.clone());
            }
            written.push(ProfileKind::Cpu);
            (
                cpu_path,
                cpu_svg_path,
                cpu_window.samples,
                args.cpu_frequency,
                cgroup_path.is_some(),
            )
        });
        let heap_output = heap_profile.map(|profile| {
            outputs.push(heap_path.clone());
            if let Some(path) = heap_svg_path.as_ref() {
                outputs.push(path.clone());
            }
            written.push(ProfileKind::Heap);
            (heap_path, heap_svg_path, profile, args.alloc_interval)
        });
        let off_cpu_output = off_cpu_profile.map(|profile| {
            outputs.push(off_cpu_path.clone());
            if let Some(path) = off_cpu_svg_path.as_ref() {
                outputs.push(path.clone());
            }
            written.push(ProfileKind::OffCpu);
            (
                off_cpu_path,
                off_cpu_svg_path,
                profile,
                cgroup_path.is_some(),
            )
        });
        let firefox_output = firefox_path.map(|path| {
            outputs.push(path.clone());
            (path, args.firefox_profile.expect("path requires a format"))
        });
        outputs.push(diagnostics_path.clone());
        let mut allocator_probe = preflight.allocator.clone();
        if let Some(reason) = collectors.heap_failure.as_ref() {
            allocator_probe.complete = false;
            allocator_probe.reason = Some(reason.clone());
        }
        let (perf_map_files, jitdump_files) = jit_artifact_counts(args.pid);
        let (peak_pending_events, forced_flushes) = collectors
            .cpu
            .as_mut()
            .map(CpuCollector::take_sorter_stats)
            .unwrap_or_default();
        let diagnostics = WindowDiagnostics {
            schema_version: 3,
            session_id: session_id.clone(),
            pid: args.pid,
            target: preflight.target.clone(),
            started_unix_nanos: window_started_nanos,
            ended_unix_nanos: window_ended_nanos,
            profiles_requested: args.profiles.clone(),
            profiles_written: written,
            allocator_probe,
            cpu: cpu_diagnostics,
            heap: heap_diagnostics,
            off_cpu: off_cpu_diagnostics,
            event_order: EventOrderDiagnostics {
                reorder_window_nanos: u64::try_from(args.event_reorder_window.as_nanos())
                    .unwrap_or(u64::MAX),
                max_pending_events: args.max_pending_events,
                peak_pending_events,
                forced_flushes,
                timeline_events_dropped: cpu_window.timeline_dropped,
                ..Default::default()
            },
            firefox: FirefoxOutputDiagnostics {
                enabled: args.firefox_profile.is_some(),
                format: args.firefox_profile.map(|format| match format {
                    FirefoxProfileFormat::Json => "json".to_owned(),
                    FirefoxProfileFormat::Jslb => "jslb".to_owned(),
                }),
                samples: if args.firefox_profile.is_some() {
                    cpu_window.timeline_samples
                } else {
                    0
                },
                dropped_samples: if args.firefox_profile.is_some() {
                    cpu_window.timeline_dropped
                } else {
                    0
                },
                error: None,
            },
            jit: JitDiagnostics {
                perf_map_files,
                jitdump_files,
                mappings_loaded: 0,
                mappings_dropped: 0,
            },
            scope: TargetScopeDiagnostics {
                requested: match preflight.target.kind {
                    crate::TargetKind::Process => "process".to_owned(),
                    crate::TargetKind::Docker | crate::TargetKind::Kubernetes => {
                        "container_cgroup".to_owned()
                    }
                },
                effective: if cgroup_path.is_some() && wants_heap {
                    "mixed_process_and_cgroup".to_owned()
                } else if cgroup_path.is_some() {
                    "container_cgroup".to_owned()
                } else {
                    "process".to_owned()
                },
                cgroup_path: cgroup_path.clone(),
                degraded_reason: (cgroup_path.is_some() && wants_heap)
                    .then(|| {
                        "CPU and off-CPU use the container cgroup; sampled heap currently uses the container init process"
                            .to_owned()
                    }),
            },
            output_backpressure: OutputBackpressureDiagnostics::default(),
            otlp: if otlp_enabled {
                OtlpExportDiagnostics {
                    status: OtlpExportStatus::Pending,
                    profiles: 0,
                    attempts: 0,
                    rejected_profiles: 0,
                    timeline_enabled: args.otlp_timeline,
                    timeline_samples: 0,
                    timeline_dropped_samples: if args.otlp_timeline {
                        cpu_window.timeline_dropped
                    } else {
                        0
                    },
                    timeline_timestamp_errors: 0,
                    error: None,
                }
            } else {
                OtlpExportDiagnostics::disabled()
            },
            outputs,
            warnings: window_warnings,
        };
        output_writer.submit(WindowOutput {
            cpu: cpu_output,
            heap: heap_output,
            off_cpu: off_cpu_output,
            firefox: firefox_output,
            timeline: timeline_samples,
            timeline_frequency: args.cpu_frequency,
            timeline_raw: cgroup_path.is_some(),
            otlp_timeline: args.otlp_timeline,
            clock_anchor,
            diagnostics_path,
            diagnostics,
            started_unix_nanos: window_started_nanos,
            duration_nanos,
            target_labels: profile_labels(&preflight.target),
            executable: preflight.executable.clone(),
            export_otlp: true,
        })?;
        window_index += 1;

        if stopped.load(Ordering::Relaxed) || deadline_reached(recording_deadline) {
            break;
        }

        if target_exited {
            if !target.supports_restart() {
                break;
            }
            let previous = preflight.target.clone();
            drop(collectors);
            let Some(resolved) =
                wait_for_restarted_target(&mut target, &previous, recording_deadline, &stopped)?
            else {
                break;
            };
            args.pid = resolved.pid;
            cgroup_path = target.cgroup_path()?;
            preflight = process::inspect(args.pid, args.allocator, resolved.metadata.clone())?;
            target.validate_current()?;
            if !preflight.is_recordable() {
                bail!(
                    "preflight failed after target restart: {}",
                    preflight.errors.join("; ")
                );
            }
            require_native_architecture(&preflight.architecture)?;
            pidfd = PidFd::open(args.pid)?;
            let selected = if launch_auto {
                (
                    UnwindMode::Fp,
                    Some(
                        "launch exec boundary restarted frame-pointer window calibration"
                            .to_owned(),
                    ),
                )
            } else {
                select_unwind_mode(&args, &mut preflight, &pidfd, &stopped)?
            };
            selected_mode = selected.0;
            fallback_reason = selected.1;
            output_writer.replace_symbolizer(Symbolizer::for_process(
                args.pid,
                &args.symbols.symbol_dirs,
                args.symbols.debuginfod.as_deref(),
            )?)?;
            collectors = Collectors::start(
                &args,
                &preflight,
                selected_mode,
                wants_cpu,
                wants_heap,
                wants_off_cpu,
                cgroup_path.clone(),
            )?;
            collectors.warnings.push(format!(
                "target restarted with host PID {}; collectors were reattached and heap inuse values restarted",
                args.pid
            ));
            exe_identity = executable_identity(args.pid)?;
            maps = read_process_maps(args.pid)?;
            target_exited = false;
            continue;
        }

        if exec_detected {
            if let Some(heap) = collectors.heap.as_mut() {
                heap.clear_for_exec();
            }
            drop(collectors);
            refresh_preflight_after_exec(&args, &mut preflight)?;
            let selected = if launch_auto {
                (
                    UnwindMode::Fp,
                    Some(
                        "launch exec boundary restarted frame-pointer window calibration"
                            .to_owned(),
                    ),
                )
            } else {
                select_unwind_mode(&args, &mut preflight, &pidfd, &stopped)?
            };
            selected_mode = selected.0;
            fallback_reason = selected.1;
            output_writer.replace_symbolizer(Symbolizer::for_process(
                args.pid,
                &args.symbols.symbol_dirs,
                args.symbols.debuginfod.as_deref(),
            )?)?;
            collectors = Collectors::start(
                &args,
                &preflight,
                selected_mode,
                wants_cpu,
                wants_heap,
                wants_off_cpu,
                cgroup_path.clone(),
            )?;
            if collectors.heap.is_some() {
                collectors.warnings.push(
                    "heap probes were reattached after target exec; inuse values restart at that boundary"
                        .to_owned(),
                );
            }
            exe_identity = executable_identity(args.pid)?;
            maps = read_process_maps(args.pid)?;
        } else if low_fp_quality {
            let reason = format!(
                "formal FP window had only {:.1}% usable stacks; switched permanently to DWARF",
                fp_usable_ratio.unwrap_or_default() * 100.0
            );
            if !preflight.has_unwind_info && !args.allow_partial {
                bail!("{reason}, but no usable unwind table exists");
            }
            selected_mode = UnwindMode::Dwarf;
            fallback_reason = Some(reason);
            drop(collectors);
            collectors = Collectors::start(
                &args,
                &preflight,
                selected_mode,
                wants_cpu,
                wants_heap,
                wants_off_cpu,
                cgroup_path.clone(),
            )?;
            if wants_heap {
                collectors.warnings.push(
                    "heap probes were reattached during the FP-to-DWARF transition; inuse values restart at that boundary"
                        .to_owned(),
                );
            }
        }
    }

    output_writer.finish()
}

type CpuOutput = (
    PathBuf,
    Option<PathBuf>,
    HashMap<AttributedStack, CpuValues>,
    u32,
    bool,
);
type HeapOutput = (PathBuf, Option<PathBuf>, HashMap<Stack, HeapValues>, u64);
type OffCpuOutput = (
    PathBuf,
    Option<PathBuf>,
    HashMap<AttributedStack, OffCpuValues>,
    bool,
);
type FirefoxOutput = (PathBuf, FirefoxProfileFormat);

struct WindowOutput {
    cpu: Option<CpuOutput>,
    heap: Option<HeapOutput>,
    off_cpu: Option<OffCpuOutput>,
    firefox: Option<FirefoxOutput>,
    timeline: Vec<TimedStackSample>,
    timeline_frequency: u32,
    timeline_raw: bool,
    otlp_timeline: bool,
    clock_anchor: PerfClockAnchor,
    diagnostics_path: PathBuf,
    diagnostics: WindowDiagnostics,
    started_unix_nanos: i64,
    duration_nanos: i64,
    target_labels: Vec<(String, String)>,
    executable: PathBuf,
    export_otlp: bool,
}

impl WindowOutput {
    fn shed_derived_outputs(&mut self, pending_windows: usize) {
        let mut removed = HashSet::new();
        if let Some((_, svg, ..)) = self.cpu.as_mut()
            && let Some(path) = svg.take()
        {
            removed.insert(path);
        }
        if let Some((_, svg, ..)) = self.heap.as_mut()
            && let Some(path) = svg.take()
        {
            removed.insert(path);
        }
        if let Some((_, svg, ..)) = self.off_cpu.as_mut()
            && let Some(path) = svg.take()
        {
            removed.insert(path);
        }
        if let Some((path, _)) = self.firefox.take() {
            removed.insert(path);
            self.diagnostics.firefox.dropped_samples = self
                .diagnostics
                .firefox
                .dropped_samples
                .saturating_add(self.timeline.len() as u64);
            self.diagnostics.firefox.error = Some(
                "Firefox output skipped because the previous window was still being written"
                    .to_owned(),
            );
        }
        let mut shed = !removed.is_empty();
        if self.export_otlp && !matches!(self.diagnostics.otlp.status, OtlpExportStatus::Disabled) {
            self.export_otlp = false;
            shed = true;
            self.diagnostics.otlp.status = OtlpExportStatus::Dropped;
            self.diagnostics.otlp.error = Some(
                "OTLP output skipped because the previous window was still being written"
                    .to_owned(),
            );
        }
        if self.firefox.is_none()
            && (!self.export_otlp
                || matches!(self.diagnostics.otlp.status, OtlpExportStatus::Disabled))
        {
            self.timeline.clear();
        }
        if shed {
            self.diagnostics.output_backpressure = OutputBackpressureDiagnostics {
                derived_outputs_shed: true,
                pending_windows,
                files_skipped: removed.len() as u64,
                otlp_skipped: !self.export_otlp,
            };
            self.diagnostics
                .outputs
                .retain(|path| !removed.contains(path));
            self.diagnostics.warnings.push(
                "derived outputs were shed to keep continuous collection ahead of output backpressure"
                    .to_owned(),
            );
        }
    }

    fn write(
        self,
        symbolizer: &mut Symbolizer,
        retained: &mut VecDeque<Vec<PathBuf>>,
        keep_windows: usize,
        otlp_config: Option<&OtlpConfig>,
        mapping_hashes: &mut MappingHashCache,
    ) -> Result<Option<ExportJob>> {
        let outputs = self.diagnostics.outputs.clone();
        self.write_inner(
            symbolizer,
            retained,
            keep_windows,
            otlp_config,
            mapping_hashes,
        )
        .map_err(|error| rollback_incomplete_window(&outputs, error))
    }

    fn write_inner(
        mut self,
        symbolizer: &mut Symbolizer,
        retained: &mut VecDeque<Vec<PathBuf>>,
        keep_windows: usize,
        otlp_config: Option<&OtlpConfig>,
        mapping_hashes: &mut MappingHashCache,
    ) -> Result<Option<ExportJob>> {
        symbolizer.refresh_dynamic_symbols();
        let mut profiles = Vec::new();
        let mut timeline_profile = None;
        let wants_otlp = otlp_config.is_some() && self.export_otlp;
        if let Some((path, svg_path, samples, frequency, raw)) = self.cpu.take() {
            let (stats, profile) = if raw {
                (
                    Default::default(),
                    write_raw_cpu_profile(
                        &path,
                        &samples,
                        self.started_unix_nanos,
                        self.duration_nanos,
                        i64::from(1_000_000_000_u32 / frequency.max(1)),
                    )?,
                )
            } else {
                write_cpu_profile(
                    &path,
                    &samples,
                    symbolizer,
                    self.started_unix_nanos,
                    self.duration_nanos,
                    frequency,
                    &self.target_labels,
                )?
            };
            self.diagnostics.cpu.symbolized_locations = stats.symbolized_locations;
            self.diagnostics.cpu.total_locations = stats.total_locations;
            if let Some(path) = svg_path {
                write_flamegraph(
                    &path,
                    &profile,
                    1,
                    "CPU flame graph",
                    FlameValue::Nanoseconds,
                )?;
            }
            if wants_otlp && !self.otlp_timeline {
                profiles.push(profile);
            }
        }
        if let Some((path, svg_path, samples, allocation_interval)) = self.heap.take() {
            let (stats, profile) = write_heap_profile(
                &path,
                &samples,
                symbolizer,
                self.started_unix_nanos,
                self.duration_nanos,
                allocation_interval,
                &self.target_labels,
            )?;
            self.diagnostics.heap.symbolized_locations = stats.symbolized_locations;
            self.diagnostics.heap.total_locations = stats.total_locations;
            if let Some(path) = svg_path {
                write_flamegraph(
                    &path,
                    &profile,
                    3,
                    "Heap in-use flame graph",
                    FlameValue::Bytes,
                )?;
            }
            if wants_otlp {
                profiles.push(profile);
            }
        }
        if let Some((path, svg_path, samples, raw)) = self.off_cpu.take() {
            let profile = if raw {
                write_raw_off_cpu_profile(
                    &path,
                    &samples,
                    self.started_unix_nanos,
                    self.duration_nanos,
                )?
            } else {
                write_off_cpu_profile(
                    &path,
                    &samples,
                    symbolizer,
                    self.started_unix_nanos,
                    self.duration_nanos,
                    &self.target_labels,
                )?
                .1
            };
            if let Some(path) = svg_path {
                write_flamegraph(
                    &path,
                    &profile,
                    1,
                    "Off-CPU flame graph",
                    FlameValue::Nanoseconds,
                )?;
            }
            if wants_otlp {
                profiles.push(profile);
            }
        }
        if wants_otlp && self.otlp_timeline {
            let ended_unix_nanos = self.started_unix_nanos.saturating_add(self.duration_nanos);
            let timestamps = self
                .timeline
                .iter()
                .map(|sample| {
                    self.clock_anchor.to_unix_nanos(
                        sample.timestamp,
                        self.started_unix_nanos,
                        ended_unix_nanos,
                    )
                })
                .collect::<Vec<_>>();
            let timestamp_errors = timestamps
                .iter()
                .filter(|timestamp| timestamp.is_none())
                .count() as u64;
            let profile_symbolizer = if self.timeline_raw {
                None
            } else {
                Some(&mut *symbolizer)
            };
            let (_, profile, encoded_timestamps) = build_cpu_timeline_profile(
                &self.timeline,
                &timestamps,
                profile_symbolizer,
                self.started_unix_nanos,
                self.duration_nanos,
                self.timeline_frequency,
                &self.target_labels,
            )?;
            self.diagnostics.otlp.timeline_samples = encoded_timestamps.len() as u64;
            self.diagnostics.otlp.timeline_timestamp_errors = timestamp_errors;
            timeline_profile = Some((profile, encoded_timestamps));
        }
        if let Some((path, format)) = self.firefox.take() {
            let profile_symbolizer = if self.timeline_raw {
                None
            } else {
                Some(&mut *symbolizer)
            };
            write_firefox_profile(
                &path,
                &self.timeline,
                format,
                self.started_unix_nanos,
                self.timeline_frequency,
                self.executable
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("profiled process"),
                profile_symbolizer,
            )?;
        }
        self.diagnostics.jit.mappings_loaded = symbolizer.jit_mapping_count();
        let payload = if self.export_otlp
            && let Some(config) = otlp_config
        {
            match encode_profiles(
                &profiles.iter().collect::<Vec<_>>(),
                timeline_profile
                    .as_ref()
                    .map(|(profile, timestamps)| (profile, timestamps.as_slice())),
                &self.diagnostics.target,
                &self.executable,
                config,
                mapping_hashes,
            ) {
                Ok(payload) => {
                    self.diagnostics.otlp.profiles = payload.profiles;
                    Some(payload)
                }
                Err(error) => {
                    self.diagnostics.otlp.status = OtlpExportStatus::Failed;
                    self.diagnostics.otlp.error = Some(format!("OTLP encoding failed: {error:#}"));
                    None
                }
            }
        } else {
            None
        };
        write_json_atomic(&self.diagnostics_path, &self.diagnostics)?;
        let job = payload.map(|payload| ExportJob {
            payload,
            diagnostics_path: self.diagnostics_path.clone(),
            diagnostics: self.diagnostics.clone(),
        });
        retained.push_back(self.diagnostics.outputs);
        prune_session_windows(retained, keep_windows)?;
        if let Some(outputs) = retained.back() {
            for output in outputs {
                println!("wrote {}", output.display());
            }
        }
        Ok(job)
    }
}

fn rollback_incomplete_window(outputs: &[PathBuf], error: anyhow::Error) -> anyhow::Error {
    let mut directories = HashSet::new();
    let mut failures = Vec::new();
    for output in outputs {
        match fs::remove_file(output) {
            Ok(()) => {
                directories.insert(output.parent().unwrap_or_else(|| Path::new(".")).to_owned());
            }
            Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => {}
            Err(remove_error) => failures.push(format!("{}: {remove_error}", output.display())),
        }
    }
    for directory in directories {
        match sync_directory(&directory) {
            Ok(()) => {}
            Err(sync_error)
                if matches!(
                    sync_error.kind(),
                    std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
                ) => {}
            Err(sync_error) => failures.push(format!("{}: {sync_error}", directory.display())),
        }
    }
    if failures.is_empty() {
        error
    } else {
        error.context(format!(
            "failed to remove incomplete window outputs: {}",
            failures.join("; ")
        ))
    }
}

struct ExportJob {
    payload: ExportPayload,
    diagnostics_path: PathBuf,
    diagnostics: WindowDiagnostics,
}

enum WriterCommand {
    Window(Box<WindowOutput>),
    ReplaceSymbolizer(Symbolizer),
}

struct OutputWriter {
    commands: Option<Sender<WriterCommand>>,
    completions: Receiver<Result<()>>,
    pending: usize,
    handle: Option<thread::JoinHandle<()>>,
    exporter: Option<OtlpExporter>,
}

impl OutputWriter {
    fn start(
        symbolizer: Symbolizer,
        keep_windows: usize,
        otlp_config: Option<OtlpConfig>,
    ) -> Result<Self> {
        let (commands, command_receiver) = bounded::<WriterCommand>(1);
        let (completion_sender, completions) = unbounded::<Result<()>>();
        let exporter = otlp_config.clone().map(OtlpExporter::start).transpose()?;
        let exporter_sender = exporter.as_ref().map(|exporter| exporter.sender.clone());
        let handle = thread::Builder::new()
            .name("rustprofile-output".to_owned())
            .spawn(move || {
                let mut symbolizer = symbolizer;
                let mut retained = VecDeque::new();
                let mut mapping_hashes = MappingHashCache::default();
                while let Ok(command) = command_receiver.recv() {
                    match command {
                        WriterCommand::Window(window) => {
                            let result = window
                                .write(
                                    &mut symbolizer,
                                    &mut retained,
                                    keep_windows,
                                    otlp_config.as_ref(),
                                    &mut mapping_hashes,
                                )
                                .map(|job| {
                                    if let (Some(sender), Some(job)) =
                                        (exporter_sender.as_ref(), job)
                                    {
                                        submit_export(sender, job);
                                    }
                                });
                            if completion_sender.send(result).is_err() {
                                break;
                            }
                        }
                        WriterCommand::ReplaceSymbolizer(replacement) => {
                            symbolizer = replacement;
                        }
                    }
                }
            })
            .context("failed to start profile output worker")?;
        Ok(Self {
            commands: Some(commands),
            completions,
            pending: 0,
            handle: Some(handle),
            exporter,
        })
    }

    fn submit(&mut self, window: WindowOutput) -> Result<()> {
        self.check_completed()?;
        let mut window = window;
        if self.pending != 0 {
            window.shed_derived_outputs(self.pending);
        }
        self.send(WriterCommand::Window(Box::new(window)))?;
        self.pending += 1;
        Ok(())
    }

    fn replace_symbolizer(&mut self, symbolizer: Symbolizer) -> Result<()> {
        self.check_completed()?;
        self.send(WriterCommand::ReplaceSymbolizer(symbolizer))
    }

    fn send(&self, command: WriterCommand) -> Result<()> {
        self.commands
            .as_ref()
            .context("profile output worker is already closed")?
            .send(command)
            .map_err(|_| anyhow::anyhow!("profile output worker stopped unexpectedly"))
    }

    fn check_completed(&mut self) -> Result<()> {
        let mut first_error = None;
        loop {
            match self.completions.try_recv() {
                Ok(result) => {
                    self.pending = self.pending.saturating_sub(1);
                    if let Err(error) = result
                        && first_error.is_none()
                    {
                        first_error = Some(error);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.pending != 0 && first_error.is_none() {
                        first_error = Some(anyhow::anyhow!(
                            "profile output worker stopped with {} window(s) pending",
                            self.pending
                        ));
                    }
                    break;
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn finish(mut self) -> Result<()> {
        self.commands.take();
        let panicked = self
            .handle
            .take()
            .is_some_and(|handle| handle.join().is_err());
        self.check_completed()?;
        if panicked {
            bail!("profile output worker panicked");
        }
        if self.pending != 0 {
            bail!(
                "profile output worker stopped with {} window(s) pending",
                self.pending
            );
        }
        if let Some(exporter) = self.exporter.take() {
            exporter.finish()?;
        }
        Ok(())
    }
}

impl Drop for OutputWriter {
    fn drop(&mut self) {
        self.commands.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        if let Some(exporter) = self.exporter.take() {
            exporter.cancel();
        }
    }
}

struct OtlpExporter {
    sender: Sender<ExportJob>,
    handle: Option<thread::JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
}

impl OtlpExporter {
    fn start(config: OtlpConfig) -> Result<Self> {
        let client = ExportClient::new(config)?;
        let (sender, receiver) = bounded::<ExportJob>(4);
        let stopping = Arc::new(AtomicBool::new(false));
        let thread_stopping = Arc::clone(&stopping);
        let handle = thread::Builder::new()
            .name("rustprofile-otlp".to_owned())
            .spawn(move || export_loop(client, receiver, thread_stopping))
            .context("failed to start OTLP exporter")?;
        Ok(Self {
            sender,
            handle: Some(handle),
            stopping,
        })
    }

    fn finish(mut self) -> Result<()> {
        drop(self.sender);
        if self
            .handle
            .take()
            .is_some_and(|handle| handle.join().is_err())
        {
            bail!("OTLP exporter worker panicked");
        }
        Ok(())
    }

    fn cancel(mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        drop(self.sender);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn submit_export(sender: &Sender<ExportJob>, job: ExportJob) {
    match sender.try_send(job) {
        Ok(()) => {}
        Err(TrySendError::Full(mut job)) => {
            job.diagnostics.otlp.status = OtlpExportStatus::Dropped;
            job.diagnostics.otlp.error = Some("OTLP export queue is full".to_owned());
            persist_export_diagnostics(&job);
        }
        Err(TrySendError::Disconnected(mut job)) => {
            job.diagnostics.otlp.status = OtlpExportStatus::Failed;
            job.diagnostics.otlp.error = Some("OTLP exporter stopped".to_owned());
            persist_export_diagnostics(&job);
        }
    }
}

fn export_loop(client: ExportClient, receiver: Receiver<ExportJob>, stopping: Arc<AtomicBool>) {
    while let Ok(mut job) = receiver.recv() {
        if stopping.load(Ordering::Relaxed) {
            job.diagnostics.otlp.status = OtlpExportStatus::Failed;
            job.diagnostics.otlp.error =
                Some("OTLP export was not flushed before shutdown".to_owned());
            persist_export_diagnostics(&job);
            continue;
        }
        let outcome = client.export(&job.payload, &stopping);
        job.diagnostics.otlp.attempts = outcome.attempts;
        job.diagnostics.otlp.rejected_profiles = outcome.rejected_profiles;
        job.diagnostics.otlp.error = outcome.error;
        job.diagnostics.otlp.status = if !outcome.delivered {
            OtlpExportStatus::Failed
        } else if outcome.rejected_profiles != 0 || job.diagnostics.otlp.error.is_some() {
            OtlpExportStatus::Partial
        } else {
            OtlpExportStatus::Exported
        };
        persist_export_diagnostics(&job);
    }
}

fn persist_export_diagnostics(job: &ExportJob) {
    if let Err(error) = update_export_diagnostics(job) {
        eprintln!(
            "failed to update OTLP diagnostics {}: {error:#}",
            job.diagnostics_path.display()
        );
    }
}

fn update_export_diagnostics(job: &ExportJob) -> Result<()> {
    if job.diagnostics_path.exists() {
        write_json_atomic(&job.diagnostics_path, &job.diagnostics)?;
    }
    Ok(())
}

struct CpuProcessCollector {
    pid: i32,
    perf: PerfCollector,
    batch: PerfBatch,
    dwarf: Option<DwarfUnwinder>,
    maps: Vec<MapEntry>,
    executable_ranges: ExecutableRanges,
    sorter: PerfEventSorter,
}

impl CpuProcessCollector {
    fn new(
        pid: i32,
        args: &RecordArgs,
        mode: UnwindMode,
        max_pending_events: usize,
    ) -> Result<Self> {
        let maps = read_process_maps(pid)?;
        let executable_ranges = ExecutableRanges::from_maps(&maps);
        let dwarf = (mode == UnwindMode::Dwarf)
            .then(|| DwarfUnwinder::from_maps(pid, &maps))
            .transpose()?;
        Ok(Self {
            pid,
            perf: PerfCollector::new(pid, mode, args.cpu_frequency, args.max_threads)?,
            batch: PerfBatch::default(),
            dwarf,
            maps,
            executable_ranges,
            sorter: PerfEventSorter::new(
                u64::try_from(args.event_reorder_window.as_nanos()).unwrap_or(u64::MAX),
                max_pending_events,
            ),
        })
    }

    fn reconcile(&mut self, threads: Option<&BTreeSet<i32>>, refresh_maps: bool) -> Result<()> {
        if let Some(threads) = threads {
            self.perf.reconcile_thread_ids(threads)?;
        } else {
            self.perf.reconcile_threads()?;
        }
        if refresh_maps {
            let maps = read_process_maps(self.pid)?;
            if maps != self.maps {
                self.executable_ranges = ExecutableRanges::from_maps(&maps);
                if self.dwarf.is_some() {
                    self.dwarf = Some(DwarfUnwinder::from_maps(self.pid, &maps)?);
                }
                self.maps = maps;
            }
        }
        Ok(())
    }

    fn ingest(
        &mut self,
        window: &mut CpuWindow,
        allow_partial: bool,
        frequency: u32,
        wait: bool,
        flush: bool,
    ) -> Result<()> {
        if wait {
            self.perf
                .wait_and_drain_into(&mut self.batch, POLL_INTERVAL)?;
        } else {
            self.perf.drain_into(&mut self.batch);
        }
        ingest_cpu_batch(
            &mut self.batch,
            window,
            &mut self.sorter,
            flush,
            self.dwarf.as_mut(),
            &self.executable_ranges,
            allow_partial,
            frequency,
        )
    }
}

enum CpuCollector {
    Process(CpuProcessCollector),
    Cgroup {
        path: PathBuf,
        processes: HashMap<i32, CpuProcessCollector>,
        retired: Vec<CpuProcessCollector>,
        mode: UnwindMode,
    },
}

impl CpuCollector {
    fn new(args: &RecordArgs, mode: UnwindMode, cgroup_path: Option<PathBuf>) -> Result<Self> {
        if let Some(path) = cgroup_path {
            let mut collector = Self::Cgroup {
                path,
                processes: HashMap::new(),
                retired: Vec::new(),
                mode,
            };
            collector.reconcile_with_args(args, None)?;
            Ok(collector)
        } else {
            Ok(Self::Process(CpuProcessCollector::new(
                args.pid,
                args,
                mode,
                args.max_pending_events,
            )?))
        }
    }

    fn reconcile_with_args(
        &mut self,
        args: &RecordArgs,
        snapshot: Option<&CgroupThreadSnapshot>,
    ) -> Result<()> {
        match self {
            Self::Process(process) => process.reconcile(None, false),
            Self::Cgroup {
                path,
                processes,
                retired,
                mode,
            } => {
                let owned_snapshot;
                let snapshot = match snapshot {
                    Some(snapshot) => snapshot,
                    None => {
                        owned_snapshot = CgroupThreadSnapshot::read(path)?;
                        &owned_snapshot
                    }
                };
                let current = snapshot
                    .threads_by_pid
                    .keys()
                    .copied()
                    .collect::<HashSet<_>>();
                let total_threads = snapshot.total_threads;
                let cgroup_thread_limit = args.max_threads.min(args.max_pending_events);
                if total_threads > cgroup_thread_limit {
                    bail!(
                        "target cgroup has {total_threads} threads, exceeding the bounded collector limit {cgroup_thread_limit}"
                    );
                }
                let per_process_pending = args
                    .max_pending_events
                    .checked_div(args.max_threads.max(1))
                    .unwrap_or_default()
                    .max(1);
                let exited = processes
                    .keys()
                    .filter(|pid| !current.contains(pid))
                    .copied()
                    .collect::<Vec<_>>();
                for pid in exited {
                    if let Some(process) = processes.remove(&pid) {
                        retired.push(process);
                    }
                }
                for pid in current {
                    if let Some(process) = processes.get_mut(&pid) {
                        if let Err(error) =
                            process.reconcile(snapshot.threads_by_pid.get(&pid), pid != args.pid)
                            && Path::new(&format!("/proc/{pid}")).exists()
                        {
                            return Err(error);
                        }
                    } else {
                        match CpuProcessCollector::new(pid, args, *mode, per_process_pending) {
                            Ok(process) => {
                                processes.insert(pid, process);
                            }
                            Err(_) if !Path::new(&format!("/proc/{pid}")).exists() => {}
                            Err(error) => return Err(error),
                        }
                    }
                }
                Ok(())
            }
        }
    }

    fn wait_and_ingest(
        &mut self,
        window: &mut CpuWindow,
        allow_partial: bool,
        frequency: u32,
    ) -> Result<()> {
        match self {
            Self::Process(process) => process.ingest(window, allow_partial, frequency, true, false),
            Self::Cgroup {
                processes, retired, ..
            } => {
                if processes.is_empty() {
                    for process in retired.iter_mut() {
                        process.ingest(window, allow_partial, frequency, false, true)?;
                    }
                    retired.clear();
                    thread::sleep(POLL_INTERVAL);
                    return Ok(());
                }
                for (index, process) in processes.values_mut().enumerate() {
                    process.ingest(window, allow_partial, frequency, index == 0, false)?;
                }
                for process in retired.iter_mut() {
                    process.ingest(window, allow_partial, frequency, false, true)?;
                }
                retired.clear();
                Ok(())
            }
        }
    }

    fn drain_and_ingest(
        &mut self,
        window: &mut CpuWindow,
        allow_partial: bool,
        frequency: u32,
    ) -> Result<()> {
        match self {
            Self::Process(process) => process.ingest(window, allow_partial, frequency, false, true),
            Self::Cgroup {
                processes, retired, ..
            } => {
                for process in processes.values_mut() {
                    process.ingest(window, allow_partial, frequency, false, true)?;
                }
                for process in retired.iter_mut() {
                    process.ingest(window, allow_partial, frequency, false, true)?;
                }
                retired.clear();
                Ok(())
            }
        }
    }

    fn refresh_root(&mut self, pid: i32, maps: &[MapEntry]) -> Result<()> {
        let process = match self {
            Self::Process(process) if process.pid == pid => Some(process),
            Self::Cgroup { processes, .. } => processes.get_mut(&pid),
            _ => None,
        };
        if let Some(process) = process {
            process.maps = maps.to_vec();
            process.executable_ranges = ExecutableRanges::from_maps(maps);
            if process.dwarf.is_some() {
                process.dwarf = Some(DwarfUnwinder::from_maps(pid, maps)?);
            }
        }
        Ok(())
    }

    fn take_sorter_stats(&mut self) -> (usize, u64) {
        match self {
            Self::Process(process) => process.sorter.take_stats(),
            Self::Cgroup {
                processes, retired, ..
            } => processes.values_mut().chain(retired.iter_mut()).fold(
                (0_usize, 0_u64),
                |(peak, forced), process| {
                    let (process_peak, process_forced) = process.sorter.take_stats();
                    (
                        peak.saturating_add(process_peak),
                        forced.saturating_add(process_forced),
                    )
                },
            ),
        }
    }
}

struct CgroupThreadSnapshot {
    threads_by_pid: HashMap<i32, BTreeSet<i32>>,
    total_threads: usize,
}

impl CgroupThreadSnapshot {
    fn read(path: &Path) -> Result<Self> {
        let pids = fs::read_to_string(path.join("cgroup.procs"))
            .with_context(|| format!("failed to read {}", path.join("cgroup.procs").display()))?;
        let mut threads_by_pid = HashMap::new();
        let mut total_threads = 0_usize;
        for pid in pids.lines().filter_map(|pid| pid.parse::<i32>().ok()) {
            match process::read_threads(pid) {
                Ok(threads) => {
                    total_threads = total_threads.saturating_add(threads.len());
                    threads_by_pid.insert(pid, threads);
                }
                Err(_) if !Path::new(&format!("/proc/{pid}")).exists() => {}
                Err(error) => return Err(error),
            }
        }
        Ok(Self {
            threads_by_pid,
            total_threads,
        })
    }
}

fn reconcile_collectors(
    mut cpu: Option<&mut CpuCollector>,
    mut off_cpu: Option<&mut OffCpuCollector>,
    cgroup_path: Option<&Path>,
    args: &RecordArgs,
) -> Result<()> {
    if cpu.is_none() && off_cpu.is_none() {
        return Ok(());
    }
    let snapshot = cgroup_path.map(CgroupThreadSnapshot::read).transpose()?;
    if let Some(cpu) = cpu.take() {
        cpu.reconcile_with_args(args, snapshot.as_ref())?;
    }
    if let Some(off_cpu) = off_cpu.take() {
        if let Some(snapshot) = snapshot.as_ref() {
            off_cpu.reconcile_thread_snapshot(&snapshot.threads_by_pid)?;
        } else {
            off_cpu.reconcile_threads()?;
        }
    }
    Ok(())
}

struct Collectors {
    lifecycle: LifecycleNotifier,
    cpu: Option<CpuCollector>,
    heap: Option<HeapCollector>,
    off_cpu: Option<OffCpuCollector>,
    heap_failure: Option<String>,
    off_cpu_failure: Option<String>,
    warnings: Vec<String>,
}

impl Collectors {
    fn start(
        args: &RecordArgs,
        preflight: &CheckReport,
        mode: UnwindMode,
        wants_cpu: bool,
        wants_heap: bool,
        wants_off_cpu: bool,
        cgroup_path: Option<PathBuf>,
    ) -> Result<Self> {
        if mode == UnwindMode::Dwarf && !preflight.has_unwind_info && !args.allow_partial {
            bail!(
                "DWARF unwind was selected but no .eh_frame or .debug_frame is available; use --allow-partial for leaf-only output"
            );
        }
        let lifecycle = LifecycleNotifier::attach(args.pid, &preflight.kernel_release)?;
        let cpu = wants_cpu
            .then(|| CpuCollector::new(args, mode, cgroup_path.clone()))
            .transpose()?;
        let mut warnings = lifecycle
            .fallback_reason()
            .map(str::to_owned)
            .into_iter()
            .collect::<Vec<_>>();
        let mut heap_failure = None;
        let mut off_cpu_failure = None;
        let heap = if wants_heap && preflight.allocator.complete {
            match HeapCollector::attach(
                args.pid,
                &preflight.allocator,
                args.alloc_interval,
                mode,
                args.allow_partial,
                args.max_stacks,
            ) {
                Ok(heap) => Some(heap),
                Err(error) if args.allow_partial => {
                    let reason =
                        format!("heap profiling was disabled after attach failed: {error:#}");
                    warnings.push(reason.clone());
                    heap_failure = Some(reason);
                    None
                }
                Err(error) => return Err(error).context("heap profiling could not be enabled"),
            }
        } else if wants_heap && args.allow_partial {
            let reason = preflight
                .allocator
                .reason
                .clone()
                .unwrap_or_else(|| "heap allocator probes are unavailable".to_owned());
            warnings.push(reason.clone());
            heap_failure = Some(reason);
            None
        } else if wants_heap {
            bail!(
                "heap profiling is unavailable: {}",
                preflight
                    .allocator
                    .reason
                    .as_deref()
                    .unwrap_or("allocator probes are unavailable")
            );
        } else {
            None
        };
        let off_cpu = if wants_off_cpu {
            match OffCpuCollector::attach(args.pid, cgroup_path, args.max_stacks, args.max_threads)
            {
                Ok(collector) => Some(collector),
                Err(error) if args.allow_partial => {
                    let reason = format!("off-CPU profiling was disabled: {error:#}");
                    warnings.push(reason.clone());
                    off_cpu_failure = Some(reason);
                    None
                }
                Err(error) => return Err(error).context("off-CPU profiling could not be enabled"),
            }
        } else {
            None
        };
        if cpu.is_none() && heap.is_none() && off_cpu.is_none() {
            bail!("none of the requested profile types could be enabled");
        }
        if let Some(heap) = heap.as_ref() {
            warnings.push(format!(
                "attached {} allocator probes for heap sampling",
                heap.link_count()
            ));
        }
        Ok(Self {
            lifecycle,
            cpu,
            heap,
            off_cpu,
            heap_failure,
            off_cpu_failure,
            warnings,
        })
    }
}

struct CpuWindow {
    samples: HashMap<AttributedStack, CpuValues>,
    stack_interner: HashMap<Stack, Stack>,
    fallback_pid: u32,
    max_stacks: usize,
    max_thread_attributions: usize,
    timeline_enabled: bool,
    max_timeline_samples: usize,
    timeline: Vec<TimedStackSample>,
    timeline_samples: u64,
    timeline_dropped: u64,
    thread_names: HashMap<(u32, u32), Option<String>>,
    thread_attribution_dropped_samples: u64,
    total_samples: u64,
    usable_samples: u64,
    lost_samples: u64,
    malformed_samples: u64,
    truncated_samples: u64,
    invalid_addresses: u64,
    aggregation_dropped_samples: u64,
    aggregation_dropped_nanoseconds: i64,
    depth_sum: u64,
    seen_addresses: HashSet<u64>,
}

impl CpuWindow {
    fn new(
        pid: i32,
        max_stacks: usize,
        max_thread_attributions: usize,
        timeline_enabled: bool,
        max_timeline_samples: usize,
    ) -> Self {
        Self {
            samples: HashMap::new(),
            stack_interner: HashMap::new(),
            fallback_pid: u32::try_from(pid).unwrap_or_default(),
            max_stacks,
            max_thread_attributions,
            timeline_enabled,
            max_timeline_samples,
            timeline: Vec::new(),
            timeline_samples: 0,
            timeline_dropped: 0,
            thread_names: HashMap::new(),
            thread_attribution_dropped_samples: 0,
            total_samples: 0,
            usable_samples: 0,
            lost_samples: 0,
            malformed_samples: 0,
            truncated_samples: 0,
            invalid_addresses: 0,
            aggregation_dropped_samples: 0,
            aggregation_dropped_nanoseconds: 0,
            depth_sum: 0,
            seen_addresses: HashSet::new(),
        }
    }

    fn diagnostics(
        &self,
        requested: UnwindMode,
        selected: UnwindMode,
        fallback_reason: Option<String>,
    ) -> CpuWindowDiagnostics {
        CpuWindowDiagnostics {
            requested_mode: Some(requested),
            selected_mode: Some(selected),
            fallback_reason,
            samples: self.total_samples,
            usable_samples: self.usable_samples,
            cpu_nanoseconds: self.samples.values().fold(0_i64, |total, values| {
                total.saturating_add(values.nanoseconds)
            }),
            lost_samples: self.lost_samples,
            malformed_samples: self.malformed_samples,
            truncated_samples: self.truncated_samples,
            invalid_addresses: self.invalid_addresses,
            aggregation_dropped_samples: self.aggregation_dropped_samples,
            aggregation_dropped_nanoseconds: self.aggregation_dropped_nanoseconds,
            average_depth: if self.usable_samples == 0 {
                0.0
            } else {
                self.depth_sum as f64 / self.usable_samples as f64
            },
            symbolized_locations: 0,
            total_locations: 0,
            attributed_series: self.samples.len() as u64,
            thread_attribution_dropped_samples: self.thread_attribution_dropped_samples,
        }
    }

    fn thread_name(&mut self, pid: u32, tid: u32) -> Option<String> {
        let key = (pid, tid);
        if let Some(name) = self.thread_names.get(&key) {
            return name.clone();
        }
        if self.thread_names.len() >= self.max_thread_attributions {
            self.thread_attribution_dropped_samples =
                self.thread_attribution_dropped_samples.saturating_add(1);
            return None;
        }
        let name = fs::read_to_string(format!("/proc/{pid}/task/{tid}/comm"))
            .ok()
            .map(|name| name.trim_end().to_owned())
            .filter(|name| !name.is_empty());
        self.thread_names.insert(key, name.clone());
        name
    }

    fn intern_stack(&mut self, stack: Stack) -> Stack {
        if let Some(interned) = self.stack_interner.get(&stack) {
            return interned.clone();
        }
        if self.stack_interner.len() < self.max_stacks {
            self.stack_interner.insert(stack.clone(), stack.clone());
        }
        stack
    }
}

fn ingest_cpu_batch(
    batch: &mut PerfBatch,
    window: &mut CpuWindow,
    sorter: &mut PerfEventSorter,
    flush: bool,
    mut dwarf: Option<&mut DwarfUnwinder>,
    executable_ranges: &ExecutableRanges,
    allow_partial: bool,
    frequency: u32,
) -> Result<()> {
    window.lost_samples = window.lost_samples.saturating_add(batch.lost_samples);
    window.malformed_samples = window
        .malformed_samples
        .saturating_add(batch.malformed_records);
    let samples = std::mem::take(&mut batch.samples);
    let mut ready = Vec::with_capacity(samples.len());
    for sample in samples {
        sorter.push(sample, &mut ready);
    }
    if flush {
        sorter.flush(&mut ready);
    }
    let mut samples = ready;
    for sample in samples.drain(..) {
        window.total_samples += 1;
        let (frames, truncated) = match sample.data {
            PerfSampleData::FramePointer(frames) => {
                let valid = frames
                    .iter()
                    .filter(|address| executable_ranges.contains(**address))
                    .count();
                window.invalid_addresses = window
                    .invalid_addresses
                    .saturating_add((frames.len() - valid) as u64);
                let cyclic = has_address_cycle(&frames, &mut window.seen_addresses);
                if valid != frames.len() || frames.is_empty() || cyclic {
                    continue;
                }
                let truncated = frames.len() == crate::config::DEFAULT_MAX_FRAMES;
                (frames, truncated)
            }
            PerfSampleData::Dwarf(snapshot) => {
                let registers = snapshot.registers;
                if let Some(unwinder) = dwarf.as_deref_mut() {
                    let outcome = unwinder.unwind_bytes(registers, &snapshot.bytes);
                    batch.recycle_stack_buffer(snapshot.bytes);
                    if outcome.fatal {
                        bail!(
                            "address normalization could not safely resolve aarch64 PAC/TBI data: {}",
                            outcome.error.as_deref().unwrap_or("ambiguous code address")
                        );
                    }
                    if outcome.frames.is_empty()
                        || (!allow_partial && outcome.frames.len() == 1 && outcome.error.is_some())
                    {
                        window.invalid_addresses += 1;
                        continue;
                    }
                    (outcome.frames, outcome.truncated)
                } else if allow_partial && registers.ip != 0 {
                    batch.recycle_stack_buffer(snapshot.bytes);
                    (vec![registers.ip], false)
                } else {
                    batch.recycle_stack_buffer(snapshot.bytes);
                    window.invalid_addresses += 1;
                    continue;
                }
            }
        };
        if truncated {
            window.truncated_samples += 1;
        }
        let depth = frames.len() as u64;
        let stack = window.intern_stack(Stack::from(
            frames
                .into_iter()
                .map(|address| Frame { address })
                .collect::<Vec<_>>(),
        ));
        let period = if sample.period == 0 {
            1_000_000_000_u64 / u64::from(frequency.max(1))
        } else {
            sample.period
        };
        let period = i64::try_from(period).unwrap_or(i64::MAX);
        let pid = if sample.pid == 0 {
            window.fallback_pid
        } else {
            sample.pid
        };
        let thread_name = window.thread_name(pid, sample.tid);
        if window.timeline_enabled {
            window.timeline_samples = window.timeline_samples.saturating_add(1);
            if window.timeline.len() < window.max_timeline_samples {
                window.timeline.push(TimedStackSample {
                    stack: stack.clone(),
                    pid,
                    tid: sample.tid,
                    thread_name: thread_name.clone(),
                    timestamp: sample.time,
                    cpu_delta: u64::try_from(period).unwrap_or_default(),
                });
            } else {
                window.timeline_dropped = window.timeline_dropped.saturating_add(1);
            }
        }
        let key = AttributedStack {
            stack,
            pid,
            tid: sample.tid,
            thread_name,
        };
        let at_capacity = window.samples.len() >= window.max_stacks;
        let values = match window.samples.entry(key) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) if !at_capacity => {
                entry.insert(CpuValues::default())
            }
            std::collections::hash_map::Entry::Vacant(_) => {
                window.aggregation_dropped_samples =
                    window.aggregation_dropped_samples.saturating_add(1);
                window.aggregation_dropped_nanoseconds = window
                    .aggregation_dropped_nanoseconds
                    .saturating_add(period);
                continue;
            }
        };
        window.usable_samples += 1;
        window.depth_sum = window.depth_sum.saturating_add(depth);
        values.samples = values.samples.saturating_add(1);
        values.nanoseconds = values.nanoseconds.saturating_add(period);
    }
    Ok(())
}

fn select_unwind_mode(
    args: &RecordArgs,
    preflight: &mut CheckReport,
    pidfd: &PidFd,
    stopped: &Arc<AtomicBool>,
) -> Result<(UnwindMode, Option<String>)> {
    match args.unwind {
        UnwindMode::Fp => Ok((UnwindMode::Fp, None)),
        UnwindMode::Dwarf => Ok((UnwindMode::Dwarf, None)),
        UnwindMode::Auto => 'calibration: loop {
            let maps = read_process_maps(args.pid)?;
            let executable_ranges = ExecutableRanges::from_maps(&maps);
            let mut identity = executable_identity(args.pid)?;
            let mut collector = PerfCollector::new(
                args.pid,
                UnwindMode::Fp,
                args.cpu_frequency,
                args.max_threads,
            )?;
            let lifecycle = LifecycleNotifier::attach(args.pid, &preflight.kernel_release)?;
            let mut quality = FpQuality::default();
            let mut batch = PerfBatch::default();
            let started = Instant::now();
            let mut last_reconcile = Instant::now();
            while started.elapsed() < FP_CALIBRATION_LIMIT
                && quality.samples < 64
                && !stopped.load(Ordering::Relaxed)
            {
                if pidfd.exited()? {
                    bail!("target exited during frame-pointer calibration");
                }
                let events = lifecycle.consume()?;
                if events.exec {
                    refresh_preflight_after_exec(args, preflight)?;
                    continue 'calibration;
                }
                if events.thread_change {
                    collector.reconcile_threads()?;
                }
                collector.wait_and_drain_into(&mut batch, POLL_INTERVAL)?;
                for sample in batch.samples.drain(..) {
                    if let PerfSampleData::FramePointer(frames) = sample.data {
                        quality.observe(&frames, &executable_ranges);
                    }
                }
                if last_reconcile.elapsed() >= RECONCILE_INTERVAL {
                    collector.reconcile_threads()?;
                    let current_identity = executable_identity(args.pid)?;
                    if current_identity != identity {
                        refresh_preflight_after_exec(args, preflight)?;
                        continue 'calibration;
                    }
                    identity = current_identity;
                    last_reconcile = Instant::now();
                }
            }
            if stopped.load(Ordering::Relaxed) {
                bail!("recording was interrupted during frame-pointer calibration");
            }
            if quality.calibration_passes() {
                return Ok((UnwindMode::Fp, None));
            } else {
                let reason = quality.rejection_reason();
                if !preflight.has_unwind_info && !args.allow_partial {
                    bail!("FP calibration failed ({reason}) and no usable unwind table exists");
                }
                return Ok((UnwindMode::Dwarf, Some(reason)));
            }
        },
    }
}

fn refresh_preflight_after_exec(args: &RecordArgs, preflight: &mut CheckReport) -> Result<()> {
    let mut metadata = preflight.target.clone();
    metadata.pid = args.pid;
    metadata.process_start_time_ticks = crate::target::process_start_time(args.pid)?;
    *preflight = process::inspect(args.pid, args.allocator, metadata)?;
    if !preflight.is_recordable() {
        bail!(
            "preflight failed after exec: {}",
            preflight.errors.join("; ")
        );
    }
    require_native_architecture(&preflight.architecture)
}

fn wait_for_restarted_target(
    resolver: &mut TargetResolver,
    previous: &crate::TargetMetadata,
    deadline: Option<Instant>,
    stopped: &Arc<AtomicBool>,
) -> Result<Option<crate::target::ResolvedTarget>> {
    while !stopped.load(Ordering::Relaxed) && !deadline_reached(deadline) {
        match resolver.refresh()? {
            TargetState::Running => {
                let current = resolver.current();
                if current.pid != previous.pid
                    || current.metadata.process_start_time_ticks
                        != previous.process_start_time_ticks
                {
                    return Ok(Some(current.clone()));
                }
            }
            TargetState::Waiting => {}
            TargetState::Gone => return Ok(None),
        }
        thread::sleep(RECONCILE_INTERVAL);
    }
    Ok(None)
}

fn unwind_maps_changed(previous: &[MapEntry], current: &[MapEntry]) -> bool {
    let relevant = |mapping: &&MapEntry| mapping.inode != 0 && mapping.is_executable();
    !previous
        .iter()
        .filter(relevant)
        .eq(current.iter().filter(relevant))
}

fn profile_labels(target: &crate::TargetMetadata) -> Vec<(String, String)> {
    let mut labels = vec![("process.pid".to_owned(), target.pid.to_string())];
    for (key, value) in [
        ("container.id", target.container_id.as_ref()),
        ("container.name", target.container_name.as_ref()),
        ("k8s.namespace.name", target.k8s_namespace.as_ref()),
        ("k8s.pod.name", target.k8s_pod_name.as_ref()),
        ("k8s.pod.uid", target.k8s_pod_uid.as_ref()),
        ("k8s.container.name", target.k8s_container_name.as_ref()),
        ("k8s.node.name", target.k8s_node_name.as_ref()),
    ] {
        if let Some(value) = value {
            labels.push((key.to_owned(), value.clone()));
        }
    }
    labels
}

fn prune_session_windows(retained: &mut VecDeque<Vec<PathBuf>>, keep_windows: usize) -> Result<()> {
    while retained.len() > keep_windows {
        if let Some(outputs) = retained.pop_front() {
            for output in outputs {
                match fs::remove_file(&output) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to remove expired window {}", output.display())
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

fn executable_identity(pid: i32) -> Result<(u64, u64, i64, u64)> {
    file_identity(Path::new(&format!("/proc/{pid}/exe")))
}

fn unix_nanos() -> i64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    i64::try_from(nanos).unwrap_or(i64::MAX)
}

#[derive(Clone, Copy)]
struct PerfClockAnchor {
    monotonic_nanos: u64,
    unix_nanos: i64,
}

impl PerfClockAnchor {
    fn capture() -> Result<Self> {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: time points to an initialized writable timespec.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to read CLOCK_MONOTONIC for OTLP timeline calibration");
        }
        let seconds = u64::try_from(time.tv_sec).context("CLOCK_MONOTONIC returned before zero")?;
        let nanoseconds =
            u64::try_from(time.tv_nsec).context("CLOCK_MONOTONIC nanoseconds were negative")?;
        Ok(Self {
            monotonic_nanos: seconds
                .saturating_mul(1_000_000_000)
                .saturating_add(nanoseconds),
            unix_nanos: unix_nanos(),
        })
    }

    fn to_unix_nanos(
        self,
        perf_timestamp: u64,
        window_started_unix_nanos: i64,
        window_ended_unix_nanos: i64,
    ) -> Option<u64> {
        let delta = i128::from(perf_timestamp) - i128::from(self.monotonic_nanos);
        let timestamp = i128::from(self.unix_nanos).checked_add(delta)?;
        let timestamp = i64::try_from(timestamp).ok()?;
        if timestamp < window_started_unix_nanos || timestamp > window_ended_unix_nanos {
            return None;
        }
        u64::try_from(timestamp).ok()
    }
}

fn deadline_reached(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

fn signal_flag() -> Result<Arc<AtomicBool>> {
    let flag = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&flag))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&flag))?;
    Ok(flag)
}

struct PidFd(File);

impl PidFd {
    fn open(pid: i32) -> Result<Self> {
        // SAFETY: pidfd_open takes a numeric PID and zero flags and returns a new descriptor.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("pidfd_open failed for process {pid}"));
        }
        // SAFETY: pidfd_open returned a new owned descriptor.
        Ok(Self(unsafe { File::from_raw_fd(fd) }))
    }

    fn exited(&self) -> Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd and timeout zero does not block.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            return Err(std::io::Error::last_os_error()).context("poll on pidfd failed");
        }
        Ok(result != 0 && descriptor.revents & libc::POLLIN != 0)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{CpuWindow, PerfClockAnchor, rollback_incomplete_window};

    #[test]
    fn rollback_removes_existing_outputs_and_preserves_original_error() {
        let directory = tempdir().expect("temporary output directory");
        let existing = directory.path().join("cpu-partial.pb.gz");
        let missing = directory.path().join("diagnostics-missing.json");
        fs::write(&existing, b"partial").expect("write partial output");

        let error = rollback_incomplete_window(
            &[existing.clone(), missing],
            anyhow::anyhow!("profile writer failed"),
        );

        assert!(!existing.exists());
        assert_eq!(error.to_string(), "profile writer failed");
    }

    #[test]
    fn perf_clock_anchor_converts_only_timestamps_inside_the_window() {
        let anchor = PerfClockAnchor {
            monotonic_nanos: 1_000_000,
            unix_nanos: 1_700_000_000_000,
        };
        let started = 1_700_000_000_000 - 100;
        let ended = 1_700_000_000_300;

        assert_eq!(
            anchor.to_unix_nanos(anchor.monotonic_nanos, started, ended),
            Some(anchor.unix_nanos as u64),
            "the anchor timestamp must map to its captured Unix time"
        );
        assert_eq!(
            anchor.to_unix_nanos(anchor.monotonic_nanos - 100, started, ended),
            Some(started as u64)
        );
        assert_eq!(
            anchor.to_unix_nanos(anchor.monotonic_nanos + 300, started, ended),
            Some(ended as u64)
        );
        assert_eq!(
            anchor.to_unix_nanos(anchor.monotonic_nanos - 101, started, ended),
            None,
            "a timestamp before the window must be omitted"
        );
        assert_eq!(
            anchor.to_unix_nanos(anchor.monotonic_nanos + 301, started, ended),
            None,
            "a timestamp after the window must be omitted"
        );
    }

    #[test]
    fn cpu_window_bounds_thread_name_cache_and_counts_omitted_names() {
        let pid = std::process::id();
        let mut window = CpuWindow::new(pid as i32, 8, 1, false, 0);

        let first_name = window.thread_name(pid, pid);
        assert!(window.thread_names.contains_key(&(pid, pid)));
        assert_eq!(window.thread_name(pid, pid), first_name);
        assert_eq!(window.thread_attribution_dropped_samples, 0);

        let uncached_tid = pid.saturating_add(1);
        assert_eq!(window.thread_name(pid, uncached_tid), None);
        assert_eq!(window.thread_names.len(), 1);
        assert_eq!(window.thread_attribution_dropped_samples, 1);
    }
}
