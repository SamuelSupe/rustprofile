use std::{
    collections::HashSet,
    ffi::CString,
    path::Path,
    process::{Child, Command as ProcessCommand},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::{
    cli::{Cli, Command, LaunchArgs, RecordArgs, SymbolArgs, TargetArgs},
    config::AllocatorChoice,
    otlp::OtlpConfig,
    process,
    symbol::jit_artifact_counts,
    target::TargetResolver,
};

pub fn run(cli: Cli) -> Result<()> {
    try_raise_memlock_limit();
    match cli.command {
        Command::Check(args) => {
            ensure_tracefs()?;
            let resolver = TargetResolver::resolve_initial(&args.target)?;
            let resolved = resolver.current();
            let mut report = process::inspect(
                resolved.pid,
                AllocatorChoice::Auto,
                resolved.metadata.clone(),
            )?;
            resolver.validate_current()?;
            if let Some(path) = resolver.cgroup_path()? {
                report.capabilities.container_cgroup = path.join("cgroup.procs").is_file();
                report.capabilities.cgroup_path = Some(path);
            }
            let (perf_maps, jitdumps) = jit_artifact_counts(report.pid);
            report.capabilities.perf_map = perf_maps != 0;
            report.capabilities.jitdump = jitdumps != 0;
            report
                .errors
                .extend(symbol_configuration_errors(&args.symbols));
            if report.errors.is_empty() {
                if let Err(error) = perf::probe_access(report.pid) {
                    report
                        .errors
                        .push(format!("perf access probe failed: {error:#}"));
                } else {
                    report.capabilities.perf = true;
                }
                match lifecycle::LifecycleNotifier::probe_load(report.pid, &report.kernel_release) {
                    Ok(enabled) => report.capabilities.lifecycle_bpf = enabled,
                    Err(error) => report
                        .warnings
                        .push(format!("lifecycle eBPF is unavailable: {error:#}")),
                }
                match off_cpu::OffCpuCollector::probe_load() {
                    Ok(()) => report.capabilities.off_cpu_bpf = true,
                    Err(error) => report
                        .warnings
                        .push(format!("off-CPU eBPF is unavailable: {error:#}")),
                }
                if report.allocator.complete
                    && let Err(error) =
                        heap::HeapCollector::probe_load(report.pid, &report.allocator)
                {
                    let reason = format!("allocator/eBPF load probe failed: {error:#}");
                    report.allocator.complete = false;
                    report.allocator.reason = Some(reason.clone());
                    report.warnings.push(reason);
                } else if report.allocator.complete {
                    report.capabilities.heap_bpf = true;
                }
            }
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_check_report(&report);
            }
            if report.is_recordable() {
                Ok(())
            } else {
                bail!("process {} is not recordable", report.pid)
            }
        }
        Command::Record(mut args) => {
            ensure_tracefs()?;
            validate_record_args(&mut args)?;
            let resolver = TargetResolver::resolve_initial(&args.target)?;
            args.pid = resolver.current().pid;
            let report = process::inspect(
                args.pid,
                args.allocator,
                resolver.current().metadata.clone(),
            )?;
            resolver.validate_current()?;
            if !report.is_recordable() {
                bail!("preflight failed: {}", report.errors.join("; "))
            }
            if args.profiles.contains(&crate::ProfileKind::Heap)
                && !report.allocator.complete
                && !args.allow_partial
            {
                bail!(
                    "heap profiling is unavailable: {}",
                    report
                        .allocator
                        .reason
                        .as_deref()
                        .unwrap_or("allocator could not be detected")
                )
            }
            record::record(args, report, resolver).context("recording failed")
        }
        Command::Launch(args) => {
            ensure_tracefs()?;
            launch(args)
        }
        Command::Import(args) => import::run(args),
        Command::Serve(args) => serve::run(args),
    }
}

fn ensure_tracefs() -> Result<()> {
    const TRACEFS: &str = "/sys/kernel/tracing";
    if tracefs_events_available() {
        return Ok(());
    }
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }

    let source = CString::new("tracefs").expect("tracefs contains no NUL bytes");
    let target = CString::new(TRACEFS).expect("tracefs path contains no NUL bytes");
    // SAFETY: source, target, and filesystem type are valid NUL-terminated strings;
    // a null data pointer is permitted for a tracefs mount.
    let result = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            source.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EBUSY) {
            for _ in 0..10 {
                if tracefs_events_available() {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        return Err(error).context(
            "tracefs is not mounted; run rustprofile with CAP_SYS_ADMIN/--privileged or mount tracefs at /sys/kernel/tracing",
        );
    }
    if !tracefs_events_available() {
        bail!("tracefs mounted at {TRACEFS}, but its events directory is unavailable");
    }
    Ok(())
}

fn tracefs_events_available() -> bool {
    Path::new("/sys/kernel/tracing/events").is_dir()
        || Path::new("/sys/kernel/debug/tracing/events").is_dir()
}

fn launch(args: LaunchArgs) -> Result<()> {
    let otlp_configured = OtlpConfig::from_args(&args.otlp)?.is_some();
    validate_otlp_timeline(&args.profiles, args.otlp_timeline, otlp_configured)?;
    let mut command = ProcessCommand::new("/bin/sh");
    command
        .arg("-c")
        .arg("kill -STOP $$; exec \"$@\"")
        .arg("rustprofile-launch")
        .args(&args.command);
    let child = command.spawn().context("failed to spawn launch target")?;
    let pid = i32::try_from(child.id()).context("child PID does not fit i32")?;
    let mut guard = SuspendedChild::new(child, pid);
    guard.wait_stopped()?;
    let launch_cgroup = match LaunchCgroup::create(pid) {
        Ok(cgroup) => Some(cgroup),
        Err(error) if args.allow_partial => {
            eprintln!("warning: launch descendant tracking is unavailable: {error:#}");
            None
        }
        Err(error) => return Err(error),
    };
    let target = TargetArgs {
        pid: Some(pid),
        docker_container: None,
        k8s_pod: None,
        container: None,
    };
    let mut record_args = RecordArgs {
        target,
        pid,
        resume_pid: Some(pid),
        launch_cgroup: launch_cgroup.as_ref().map(|cgroup| cgroup.path.clone()),
        profiles: args.profiles,
        duration: args.duration,
        window: args.window,
        unwind: args.unwind,
        cpu_frequency: args.cpu_frequency,
        alloc_interval: args.alloc_interval,
        allocator: args.allocator,
        output: args.output,
        keep_windows: args.keep_windows,
        max_stacks: args.max_stacks,
        max_pending_events: args.max_pending_events,
        max_timeline_samples: args.max_timeline_samples,
        otlp_timeline: args.otlp_timeline,
        event_reorder_window: args.event_reorder_window,
        firefox_profile: args.firefox_profile,
        svg: args.svg,
        allow_partial: args.allow_partial,
        max_threads: args.max_threads,
        symbols: args.symbols,
        otlp: args.otlp,
    };
    validate_record_args(&mut record_args)?;
    let resolver = TargetResolver::resolve_initial(&record_args.target)?;
    let mut report = process::inspect(
        pid,
        record_args.allocator,
        resolver.current().metadata.clone(),
    )?;
    resolver.validate_current()?;
    report.warnings.push(
        "launch target was attached before exec; collectors will refresh at the exec boundary"
            .to_owned(),
    );
    let result = record::record(record_args, report, resolver).context("launched recording failed");
    let stopped_by_profiler = guard.child.try_wait()?.is_none();
    if let Some(cgroup) = launch_cgroup.as_ref() {
        cgroup.terminate();
    }
    if stopped_by_profiler {
        // SAFETY: this targets only the launched child after recording has stopped.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    let status = guard.child.wait()?;
    guard.disarm();
    drop(launch_cgroup);
    result?;
    if !stopped_by_profiler && !status.success() {
        bail!("launched command exited with {status}");
    }
    Ok(())
}

fn validate_record_args(args: &mut RecordArgs) -> Result<()> {
    let symbol_errors = symbol_configuration_errors(&args.symbols);
    if !symbol_errors.is_empty() {
        bail!("invalid symbol configuration: {}", symbol_errors.join("; "));
    }
    let mut seen = HashSet::new();
    args.profiles.retain(|profile| seen.insert(*profile));
    if args.window.is_zero() {
        bail!("--window must be greater than zero");
    }
    if args.event_reorder_window > args.window {
        bail!("--event-reorder-window must not exceed --window");
    }
    if args.profiles.is_empty() {
        bail!("at least one profile type is required");
    }
    if args.max_threads == 0 {
        bail!("--max-threads must be greater than zero");
    }
    let otlp_configured = OtlpConfig::from_args(&args.otlp)?.is_some();
    validate_otlp_timeline(&args.profiles, args.otlp_timeline, otlp_configured)?;
    Ok(())
}

fn validate_otlp_timeline(
    profiles: &[crate::ProfileKind],
    enabled: bool,
    otlp_configured: bool,
) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    if !profiles.contains(&crate::ProfileKind::Cpu) {
        bail!("--otlp-timeline requires --profiles cpu");
    }
    if !otlp_configured {
        bail!("--otlp-timeline requires an OTLP endpoint");
    }
    Ok(())
}

struct SuspendedChild {
    child: Child,
    pid: i32,
    armed: bool,
}

struct LaunchCgroup {
    path: std::path::PathBuf,
}

impl LaunchCgroup {
    fn create(pid: i32) -> Result<Self> {
        let root = std::path::Path::new("/sys/fs/cgroup");
        if !root.join("cgroup.controllers").is_file() {
            bail!("launch descendant tracking requires cgroup v2");
        }
        let path = root.join(format!("rustprofile-launch-{pid}"));
        std::fs::create_dir(&path)
            .with_context(|| format!("failed to create launch cgroup {}", path.display()))?;
        if let Err(error) = std::fs::write(path.join("cgroup.procs"), pid.to_string()) {
            let _ = std::fs::remove_dir(&path);
            return Err(error).context("failed to move launch target into its cgroup");
        }
        Ok(Self { path })
    }

    fn terminate(&self) {
        let kill = self.path.join("cgroup.kill");
        if kill.is_file() {
            if let Err(error) = std::fs::write(&kill, "1") {
                eprintln!(
                    "warning: failed to terminate launch cgroup {}: {error}",
                    self.path.display()
                );
            }
            return;
        }
        if let Ok(processes) = std::fs::read_to_string(self.path.join("cgroup.procs")) {
            for pid in processes.lines().filter_map(|pid| pid.parse::<i32>().ok()) {
                // SAFETY: the PID came from the private cgroup created for this launch.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
}

impl Drop for LaunchCgroup {
    fn drop(&mut self) {
        self.terminate();
        let mut last_error = None;
        for _ in 0..10 {
            match std::fs::remove_dir(&self.path) {
                Ok(()) => return,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(error) => last_error = Some(error),
            }
            thread::sleep(Duration::from_millis(10));
        }
        if let Some(error) = last_error {
            eprintln!(
                "warning: failed to remove launch cgroup {}: {error}",
                self.path.display()
            );
        }
    }
}

impl SuspendedChild {
    fn new(child: Child, pid: i32) -> Self {
        Self {
            child,
            pid,
            armed: true,
        }
    }

    fn wait_stopped(&mut self) -> Result<()> {
        let mut status = 0;
        // SAFETY: status is a valid writable pointer and pid identifies our child.
        let result = unsafe { libc::waitpid(self.pid, &mut status, libc::WUNTRACED) };
        if result < 0 {
            return Err(std::io::Error::last_os_error()).context("failed waiting for launch stop");
        }
        if !libc::WIFSTOPPED(status) {
            bail!("launch target did not stop before exec");
        }
        Ok(())
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SuspendedChild {
    fn drop(&mut self) {
        if self.armed {
            // SAFETY: this targets only the child created by this command.
            unsafe {
                libc::kill(self.pid, libc::SIGKILL);
            }
            let _ = self.child.wait();
        }
    }
}

fn symbol_configuration_errors(args: &SymbolArgs) -> Vec<String> {
    let mut errors = args
        .symbol_dirs
        .iter()
        .filter(|directory| !directory.is_dir())
        .map(|directory| {
            format!(
                "symbol directory {} does not exist or is not a directory",
                directory.display()
            )
        })
        .collect::<Vec<_>>();
    if args
        .debuginfod
        .as_deref()
        .is_some_and(|url| !(url.starts_with("https://") || url.starts_with("http://")))
    {
        errors.push("--debuginfod must be an http:// or https:// URL".to_owned());
    }
    errors
}

fn try_raise_memlock_limit() {
    let limit = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    // SAFETY: limit points to an initialized rlimit structure; failure is handled by the
    // subsequent BPF load, which produces a more actionable error on memcg-based kernels.
    unsafe {
        libc::setrlimit(libc::RLIMIT_MEMLOCK, &limit);
    }
}

fn print_check_report(report: &crate::CheckReport) {
    println!("pid: {}", report.pid);
    println!("target: {:?}", report.target.kind);
    println!("executable: {}", report.executable.display());
    println!("architecture: {}", report.architecture);
    println!(
        "kernel: {} ({})",
        report.kernel_release,
        if report.kernel_supported {
            "supported"
        } else {
            "unsupported"
        }
    );
    println!("root: {}", report.running_as_root);
    println!("threads: {}", report.thread_count);
    println!("unwind tables: {}", report.has_unwind_info);
    println!(
        "symbols: {} named symbols across {} modules",
        report
            .modules
            .iter()
            .map(|module| module.symbol_count)
            .sum::<usize>(),
        report.modules.len()
    );
    println!(
        "allocator: {}",
        report
            .allocator
            .detected
            .as_deref()
            .unwrap_or("unsupported")
    );
    println!(
        "capabilities: perf={} lifecycle_bpf={} heap_bpf={} off_cpu_bpf={} container_cgroup={} perf_map={} jitdump={}",
        report.capabilities.perf,
        report.capabilities.lifecycle_bpf,
        report.capabilities.heap_bpf,
        report.capabilities.off_cpu_bpf,
        report.capabilities.container_cgroup,
        report.capabilities.perf_map,
        report.capabilities.jitdump,
    );
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    for error in &report.errors {
        println!("error: {error}");
    }
}

mod event;
mod heap;
mod import;
mod lifecycle;
mod off_cpu;
mod perf;
mod record;
mod serve;
mod serve_gallery;
