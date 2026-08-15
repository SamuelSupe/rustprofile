use std::{collections::HashSet, ffi::CString, path::Path, thread, time::Duration};

use anyhow::{Context, Result, bail};

use crate::{
    cli::{Cli, Command, SymbolArgs},
    config::AllocatorChoice,
    process,
    target::TargetResolver,
};

pub fn run(cli: Cli) -> Result<()> {
    ensure_tracefs()?;
    try_raise_memlock_limit();
    match cli.command {
        Command::Check(args) => {
            let resolver = TargetResolver::resolve_initial(&args.target)?;
            let resolved = resolver.current();
            let mut report = process::inspect(
                resolved.pid,
                AllocatorChoice::Auto,
                resolved.metadata.clone(),
            )?;
            resolver.validate_current()?;
            report
                .errors
                .extend(symbol_configuration_errors(&args.symbols));
            if report.errors.is_empty() {
                if let Err(error) = perf::probe_access(report.pid) {
                    report
                        .errors
                        .push(format!("perf/eBPF access probe failed: {error:#}"));
                }
                if report.allocator.complete
                    && let Err(error) =
                        heap::HeapCollector::probe_load(report.pid, &report.allocator)
                {
                    let reason = format!("allocator/eBPF load probe failed: {error:#}");
                    report.allocator.complete = false;
                    report.allocator.reason = Some(reason.clone());
                    report.warnings.push(reason);
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
            let symbol_errors = symbol_configuration_errors(&args.symbols);
            if !symbol_errors.is_empty() {
                bail!("invalid symbol configuration: {}", symbol_errors.join("; "));
            }
            let mut seen = HashSet::new();
            args.profiles.retain(|profile| seen.insert(*profile));
            if args.window.is_zero() {
                bail!("--window must be greater than zero")
            }
            if args.profiles.is_empty() {
                bail!("at least one profile type is required")
            }
            if args.max_threads == 0 {
                bail!("--max-threads must be greater than zero")
            }
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
    }
}

fn ensure_tracefs() -> Result<()> {
    const TRACEFS: &str = "/sys/kernel/tracing";
    if tracefs_events_available() {
        return Ok(());
    }
    // Non-root callers receive the existing preflight root diagnostic instead
    // of an unrelated mount error.
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }

    let source = CString::new("tracefs").expect("tracefs contains no NUL bytes");
    let target = CString::new(TRACEFS).expect("tracefs path contains no NUL bytes");
    // SAFETY: source, target, and filesystem type are valid NUL-terminated
    // strings; the null data pointer is permitted for a tracefs mount.
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
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    for error in &report.errors {
        println!("error: {error}");
    }
}

mod heap;
mod lifecycle;
mod perf;
mod record;
