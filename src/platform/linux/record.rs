use std::{
    collections::{HashMap, HashSet, VecDeque},
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
    heap::HeapCollector,
    lifecycle::LifecycleNotifier,
    perf::{FpQuality, PerfBatch, PerfCollector, PerfSampleData},
};
use crate::otlp::{ExportClient, ExportPayload, MappingHashCache, OtlpConfig, encode_profiles};
use crate::{
    cli::RecordArgs,
    config::{ProfileKind, UnwindMode},
    diagnostics::{
        CheckReport, CpuWindowDiagnostics, HeapWindowDiagnostics, OtlpExportDiagnostics,
        OtlpExportStatus, WindowDiagnostics,
    },
    maps::{ExecutableRanges, MapEntry, read_process_maps},
    pprof::{write_cpu_profile, write_heap_profile, write_json_atomic},
    process::{self, file_identity},
    profile::{CpuValues, Frame, HeapValues, Stack, has_address_cycle},
    svg::{FlameValue, write_flamegraph},
    symbol::Symbolizer,
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
    let otlp_config = OtlpConfig::from_args(&args.otlp)?;
    let otlp_enabled = otlp_config.is_some();
    let mut pidfd = PidFd::open(args.pid)?;
    let stopped = signal_flag()?;
    let session_id = format!("{:x}-{}", unix_nanos(), args.pid);
    let mut window_index = 0_u64;
    let mut target_exited = false;

    let (mut selected_mode, mut fallback_reason) =
        select_unwind_mode(&args, &mut preflight, &pidfd, &stopped)?;
    let symbolizer = Symbolizer::for_process(
        args.pid,
        &args.symbols.symbol_dirs,
        args.symbols.debuginfod.as_deref(),
    )?;
    let mut output_writer = OutputWriter::start(symbolizer, args.keep_windows, otlp_config)?;
    let mut collectors =
        Collectors::start(&args, &preflight, selected_mode, wants_cpu, wants_heap)?;
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
    let mut executable_ranges = ExecutableRanges::from_maps(&maps);
    let mut cpu_batch = PerfBatch::default();

    loop {
        if stopped.load(Ordering::Relaxed) || target_exited || deadline_reached(recording_deadline)
        {
            break;
        }

        let window_started_at = Instant::now();
        let window_started_nanos = unix_nanos();
        let natural_window_end = window_started_at
            .checked_add(args.window)
            .context("--window is too large")?;
        let window_deadline = recording_deadline
            .map(|deadline| deadline.min(natural_window_end))
            .unwrap_or(natural_window_end);
        let mut cpu_window = CpuWindow::new(args.max_stacks);
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
                && let Some(cpu) = collectors.cpu.as_mut()
                && let Err(error) = cpu.reconcile_threads()
            {
                if pidfd.exited()? {
                    target_exited = true;
                    break;
                }
                return Err(error);
            }
            if let Some(cpu) = collectors.cpu.as_mut() {
                cpu.wait_and_drain_into(&mut cpu_batch, POLL_INTERVAL)?;
                ingest_cpu_batch(
                    &mut cpu_batch,
                    &mut cpu_window,
                    collectors.dwarf.as_mut(),
                    &executable_ranges,
                    args.allow_partial,
                    args.cpu_frequency,
                )?;
            } else {
                thread::sleep(POLL_INTERVAL);
            }
            if let Some(heap) = collectors.heap.as_mut() {
                heap.drain()?;
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
                if let Some(cpu) = collectors.cpu.as_mut()
                    && let Err(error) = cpu.reconcile_threads()
                {
                    if pidfd.exited()? {
                        target_exited = true;
                        break;
                    }
                    return Err(error);
                }
                let refreshed_maps = read_process_maps(args.pid)?;
                if refreshed_maps != maps {
                    let unwind_maps_changed = unwind_maps_changed(&maps, &refreshed_maps);
                    maps = refreshed_maps;
                    executable_ranges = ExecutableRanges::from_maps(&maps);
                    if unwind_maps_changed {
                        if collectors.dwarf.is_some() {
                            collectors.dwarf = Some(DwarfUnwinder::for_process(args.pid)?);
                        }
                        if let Some(heap) = collectors.heap.as_mut() {
                            heap.refresh_unwinder(args.pid)?;
                        }
                        output_writer.replace_symbolizer(Symbolizer::for_process(
                            args.pid,
                            &args.symbols.symbol_dirs,
                            args.symbols.debuginfod.as_deref(),
                        )?)?;
                    }
                }
            }
        }

        if !exec_detected && let Some(cpu) = collectors.cpu.as_mut() {
            cpu.drain_into(&mut cpu_batch);
            ingest_cpu_batch(
                &mut cpu_batch,
                &mut cpu_window,
                collectors.dwarf.as_mut(),
                &executable_ranges,
                args.allow_partial,
                args.cpu_frequency,
            )?;
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
        let cpu_diagnostics =
            cpu_window.diagnostics(args.unwind, selected_mode, fallback_reason.clone());
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
        let cpu_svg_path = args
            .svg
            .then(|| args.output.join(format!("cpu-{basename}.svg")));
        let heap_svg_path = args
            .svg
            .then(|| args.output.join(format!("heap-{basename}.svg")));
        let diagnostics_path = args.output.join(format!("diagnostics-{basename}.json"));
        let mut outputs = Vec::new();
        let mut written = Vec::new();
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
        outputs.push(diagnostics_path.clone());
        let mut allocator_probe = preflight.allocator.clone();
        if let Some(reason) = collectors.heap_failure.as_ref() {
            allocator_probe.complete = false;
            allocator_probe.reason = Some(reason.clone());
        }
        let diagnostics = WindowDiagnostics {
            schema_version: 2,
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
            otlp: if otlp_enabled {
                OtlpExportDiagnostics {
                    status: OtlpExportStatus::Pending,
                    profiles: 0,
                    attempts: 0,
                    rejected_profiles: 0,
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
            diagnostics_path,
            diagnostics,
            started_unix_nanos: window_started_nanos,
            duration_nanos,
            target_labels: profile_labels(&preflight.target),
            executable: preflight.executable.clone(),
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
            let selected = select_unwind_mode(&args, &mut preflight, &pidfd, &stopped)?;
            selected_mode = selected.0;
            fallback_reason = selected.1;
            output_writer.replace_symbolizer(Symbolizer::for_process(
                args.pid,
                &args.symbols.symbol_dirs,
                args.symbols.debuginfod.as_deref(),
            )?)?;
            collectors =
                Collectors::start(&args, &preflight, selected_mode, wants_cpu, wants_heap)?;
            collectors.warnings.push(format!(
                "target restarted with host PID {}; collectors were reattached and heap inuse values restarted",
                args.pid
            ));
            exe_identity = executable_identity(args.pid)?;
            maps = read_process_maps(args.pid)?;
            executable_ranges = ExecutableRanges::from_maps(&maps);
            target_exited = false;
            continue;
        }

        if exec_detected {
            if let Some(heap) = collectors.heap.as_mut() {
                heap.clear_for_exec();
            }
            drop(collectors);
            refresh_preflight_after_exec(&args, &mut preflight)?;
            let selected = select_unwind_mode(&args, &mut preflight, &pidfd, &stopped)?;
            selected_mode = selected.0;
            fallback_reason = selected.1;
            output_writer.replace_symbolizer(Symbolizer::for_process(
                args.pid,
                &args.symbols.symbol_dirs,
                args.symbols.debuginfod.as_deref(),
            )?)?;
            collectors =
                Collectors::start(&args, &preflight, selected_mode, wants_cpu, wants_heap)?;
            if collectors.heap.is_some() {
                collectors.warnings.push(
                    "heap probes were reattached after target exec; inuse values restart at that boundary"
                        .to_owned(),
                );
            }
            exe_identity = executable_identity(args.pid)?;
            maps = read_process_maps(args.pid)?;
            executable_ranges = ExecutableRanges::from_maps(&maps);
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
            collectors =
                Collectors::start(&args, &preflight, selected_mode, wants_cpu, wants_heap)?;
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

type CpuOutput = (PathBuf, Option<PathBuf>, HashMap<Stack, CpuValues>, u32);
type HeapOutput = (PathBuf, Option<PathBuf>, HashMap<Stack, HeapValues>, u64);

struct WindowOutput {
    cpu: Option<CpuOutput>,
    heap: Option<HeapOutput>,
    diagnostics_path: PathBuf,
    diagnostics: WindowDiagnostics,
    started_unix_nanos: i64,
    duration_nanos: i64,
    target_labels: Vec<(String, String)>,
    executable: PathBuf,
}

impl WindowOutput {
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
        let mut profiles = Vec::new();
        let wants_otlp = otlp_config.is_some();
        if let Some((path, svg_path, samples, frequency)) = self.cpu.take() {
            let (stats, profile) = write_cpu_profile(
                &path,
                &samples,
                symbolizer,
                self.started_unix_nanos,
                self.duration_nanos,
                frequency,
                &self.target_labels,
            )?;
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
            if wants_otlp {
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
        let payload = if let Some(config) = otlp_config {
            match encode_profiles(
                &profiles.iter().collect::<Vec<_>>(),
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
    let failures = outputs
        .iter()
        .filter_map(|output| match fs::remove_file(output) {
            Ok(()) => None,
            Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => None,
            Err(remove_error) => Some(format!("{}: {remove_error}", output.display())),
        })
        .collect::<Vec<_>>();
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
            let _ = exporter.finish();
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
        self.stopping.store(true, Ordering::Relaxed);
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

struct Collectors {
    lifecycle: LifecycleNotifier,
    cpu: Option<PerfCollector>,
    dwarf: Option<DwarfUnwinder>,
    heap: Option<HeapCollector>,
    heap_failure: Option<String>,
    warnings: Vec<String>,
}

impl Collectors {
    fn start(
        args: &RecordArgs,
        preflight: &CheckReport,
        mode: UnwindMode,
        wants_cpu: bool,
        wants_heap: bool,
    ) -> Result<Self> {
        if mode == UnwindMode::Dwarf && !preflight.has_unwind_info && !args.allow_partial {
            bail!(
                "DWARF unwind was selected but no .eh_frame or .debug_frame is available; use --allow-partial for leaf-only output"
            );
        }
        let lifecycle = LifecycleNotifier::attach(args.pid)?;
        let cpu = wants_cpu
            .then(|| PerfCollector::new(args.pid, mode, args.cpu_frequency, args.max_threads))
            .transpose()?;
        let dwarf = (wants_cpu && mode == UnwindMode::Dwarf)
            .then(|| DwarfUnwinder::for_process(args.pid))
            .transpose()?;
        let mut warnings = Vec::new();
        let mut heap_failure = None;
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
        if cpu.is_none() && heap.is_none() {
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
            dwarf,
            heap,
            heap_failure,
            warnings,
        })
    }
}

struct CpuWindow {
    samples: HashMap<Stack, CpuValues>,
    max_stacks: usize,
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
    fn new(max_stacks: usize) -> Self {
        Self {
            samples: HashMap::new(),
            max_stacks,
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
        }
    }
}

fn ingest_cpu_batch(
    batch: &mut PerfBatch,
    window: &mut CpuWindow,
    mut dwarf: Option<&mut DwarfUnwinder>,
    executable_ranges: &ExecutableRanges,
    allow_partial: bool,
    frequency: u32,
) -> Result<()> {
    window.lost_samples = window.lost_samples.saturating_add(batch.lost_samples);
    window.malformed_samples = window
        .malformed_samples
        .saturating_add(batch.malformed_records);
    let mut samples = std::mem::take(&mut batch.samples);
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
        let stack = Stack(
            frames
                .into_iter()
                .map(|address| Frame { address })
                .collect(),
        );
        let period = if sample.period == 0 {
            1_000_000_000_u64 / u64::from(frequency.max(1))
        } else {
            sample.period
        };
        let period = i64::try_from(period).unwrap_or(i64::MAX);
        let at_capacity = window.samples.len() >= window.max_stacks;
        let values = match window.samples.entry(stack) {
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
        let _ = (sample.tid, sample.time);
    }
    batch.samples = samples;
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
            let lifecycle = LifecycleNotifier::attach(args.pid)?;
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

    use super::rollback_incomplete_window;

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
}
