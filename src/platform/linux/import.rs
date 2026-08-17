use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::BufReader,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use linux_perf_data::{PerfFileReader, PerfFileRecord, linux_perf_event_reader::EventRecord};
use serde::Serialize;

use crate::{
    cli::ImportArgs,
    firefox::write_firefox_profile,
    pprof::{write_json_atomic, write_raw_cpu_profile},
    profile::{AttributedStack, CpuValues, Frame, Stack, TimedStackSample},
};

const FALLBACK_PERIOD_NANOS: u64 = 1_000_000;
const PERF_CONTEXT_MAX: u64 = u64::MAX - 4095;
const MAX_PENDING_WINDOWS: usize = 4;
const MAX_TRACKED_THREADS: usize = 65_536;

#[derive(Default)]
struct ImportWindow {
    samples: HashMap<AttributedStack, CpuValues>,
    stack_interner: HashMap<Stack, Stack>,
    timeline: Vec<TimedStackSample>,
    total_samples: u64,
    aggregation_dropped_samples: u64,
    timeline_dropped_samples: u64,
}

impl ImportWindow {
    fn intern_stack(&mut self, stack: Stack, max_stacks: usize) -> Stack {
        if let Some(interned) = self.stack_interner.get(&stack) {
            return interned.clone();
        }
        if self.stack_interner.len() < max_stacks {
            self.stack_interner.insert(stack.clone(), stack.clone());
        }
        stack
    }
}

#[derive(Serialize)]
struct ImportDiagnostics {
    schema_version: u32,
    source: String,
    format: String,
    started_unix_nanos: i64,
    ended_unix_nanos: i64,
    samples: u64,
    aggregation_dropped_samples: u64,
    timeline_dropped_samples: u64,
    outputs: Vec<String>,
}

pub fn run(args: ImportArgs) -> Result<()> {
    if args.window.is_zero() {
        bail!("--window must be greater than zero");
    }
    if !args.input.is_file() {
        bail!(
            "input {} does not exist or is not a file",
            args.input.display()
        );
    }
    std::fs::create_dir_all(&args.output)
        .with_context(|| format!("failed to create {}", args.output.display()))?;
    let file = File::open(&args.input)?;
    let modified = file
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .unwrap_or_else(SystemTime::now);
    let base_unix_nanos = modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
        .unwrap_or_default();
    let source_name = args
        .input
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("perf.data")
        .to_owned();
    let PerfFileReader {
        mut perf_file,
        mut record_iter,
    } = PerfFileReader::parse_file(BufReader::new(file)).context("failed to parse perf.data")?;
    let window_nanos = u64::try_from(args.window.as_nanos()).unwrap_or(u64::MAX);
    let mut first_timestamp = None;
    let mut thread_names = HashMap::<(u32, u32), String>::new();
    let mut last_thread_timestamp = HashMap::<(u32, u32), u64>::new();
    let mut windows = BTreeMap::<u64, ImportWindow>::new();
    let mut flushed_before = 0_u64;
    let mut late_samples_dropped = 0_u64;
    let mut thread_state_dropped = 0_u64;
    let mut wrote_windows = 0_u64;

    while let Some(record) = record_iter.next_record(&mut perf_file)? {
        let PerfFileRecord::EventRecord { record, .. } = record else {
            continue;
        };
        let Ok(record) = record.parse() else {
            continue;
        };
        match record {
            EventRecord::Comm(comm) => {
                if let (Ok(pid), Ok(tid)) = (u32::try_from(comm.pid), u32::try_from(comm.tid)) {
                    let key = (pid, tid);
                    if thread_names.contains_key(&key) || thread_names.len() < MAX_TRACKED_THREADS {
                        thread_names.insert(
                            key,
                            String::from_utf8_lossy(&comm.name.as_slice())
                                .trim_end_matches('\0')
                                .to_owned(),
                        );
                    }
                }
            }
            EventRecord::Sample(sample) => {
                let (Some(timestamp), Some(pid), Some(tid)) =
                    (sample.timestamp, sample.pid, sample.tid)
                else {
                    continue;
                };
                let (Ok(pid), Ok(tid)) = (u32::try_from(pid), u32::try_from(tid)) else {
                    continue;
                };
                let first = *first_timestamp.get_or_insert(timestamp);
                let index = timestamp.saturating_sub(first) / window_nanos;
                if index < flushed_before {
                    late_samples_dropped = late_samples_dropped.saturating_add(1);
                    continue;
                }
                let mut addresses = Vec::new();
                if let Some(ip) = sample.ip.filter(|ip| *ip != 0 && *ip < PERF_CONTEXT_MAX) {
                    addresses.push(ip);
                }
                if let Some(callchain) = sample.callchain {
                    for position in 0..callchain.len() {
                        if let Some(address) = callchain
                            .get(position)
                            .filter(|address| *address != 0 && *address < PERF_CONTEXT_MAX)
                            && addresses.last().copied() != Some(address)
                        {
                            addresses.push(address);
                        }
                    }
                }
                if addresses.is_empty() {
                    continue;
                }
                let stack = Stack::from(
                    addresses
                        .into_iter()
                        .map(|address| Frame { address })
                        .collect::<Vec<_>>(),
                );
                let name = thread_names.get(&(pid, tid)).cloned();
                let thread_key = (pid, tid);
                let delta = if last_thread_timestamp.contains_key(&thread_key)
                    || last_thread_timestamp.len() < MAX_TRACKED_THREADS
                {
                    last_thread_timestamp
                        .insert(thread_key, timestamp)
                        .map(|previous| timestamp.saturating_sub(previous).min(window_nanos))
                        .filter(|delta| *delta != 0)
                        .unwrap_or(FALLBACK_PERIOD_NANOS)
                } else {
                    thread_state_dropped = thread_state_dropped.saturating_add(1);
                    FALLBACK_PERIOD_NANOS
                };
                let window = windows.entry(index).or_default();
                let stack = window.intern_stack(stack, args.max_stacks);
                let key = AttributedStack {
                    stack: stack.clone(),
                    pid,
                    tid,
                    thread_name: name.clone(),
                };
                window.total_samples = window.total_samples.saturating_add(1);
                let at_capacity = window.samples.len() >= args.max_stacks;
                match window.samples.entry(key) {
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        let values = entry.get_mut();
                        values.samples = values.samples.saturating_add(1);
                        values.nanoseconds = values
                            .nanoseconds
                            .saturating_add(i64::try_from(delta).unwrap_or(i64::MAX));
                    }
                    std::collections::hash_map::Entry::Vacant(entry) if !at_capacity => {
                        entry.insert(CpuValues {
                            samples: 1,
                            nanoseconds: i64::try_from(delta).unwrap_or(i64::MAX),
                        });
                    }
                    std::collections::hash_map::Entry::Vacant(_) => {
                        window.aggregation_dropped_samples =
                            window.aggregation_dropped_samples.saturating_add(1);
                    }
                }
                if args.firefox_profile.is_some() {
                    if window.timeline.len() < args.max_timeline_samples {
                        window.timeline.push(TimedStackSample {
                            stack,
                            pid,
                            tid,
                            thread_name: name,
                            timestamp,
                            cpu_delta: delta,
                        });
                    } else {
                        window.timeline_dropped_samples =
                            window.timeline_dropped_samples.saturating_add(1);
                    }
                }
                while windows.len() > MAX_PENDING_WINDOWS {
                    let (flushed_index, window) =
                        windows.pop_first().expect("window map is non-empty");
                    write_import_window(
                        &args,
                        flushed_index,
                        window,
                        base_unix_nanos,
                        window_nanos,
                        &source_name,
                    )?;
                    flushed_before = flushed_index.saturating_add(1);
                    wrote_windows = wrote_windows.saturating_add(1);
                }
            }
            _ => {}
        }
    }

    if windows.is_empty() && wrote_windows == 0 {
        bail!("perf.data contains no samples with user callchains or instruction pointers");
    }
    for (index, window) in windows {
        write_import_window(
            &args,
            index,
            window,
            base_unix_nanos,
            window_nanos,
            &source_name,
        )?;
    }
    if late_samples_dropped != 0 {
        eprintln!(
            "warning: {late_samples_dropped} samples arrived after their bounded import windows were published"
        );
    }
    if thread_state_dropped != 0 {
        eprintln!(
            "warning: {thread_state_dropped} samples used the fallback period after the import thread-state limit was reached"
        );
    }
    Ok(())
}

fn write_import_window(
    args: &ImportArgs,
    index: u64,
    window: ImportWindow,
    base_unix_nanos: i64,
    window_nanos: u64,
    source_name: &str,
) -> Result<()> {
    let started = base_unix_nanos
        .saturating_add(i64::try_from(index.saturating_mul(window_nanos)).unwrap_or(i64::MAX));
    let ended = started.saturating_add(i64::try_from(window_nanos).unwrap_or(i64::MAX));
    let basename = format!("import-{index:06}-{started}");
    let cpu_path = args.output.join(format!("cpu-{basename}.pb.gz"));
    write_raw_cpu_profile(
        &cpu_path,
        &window.samples,
        started,
        ended.saturating_sub(started),
        i64::try_from(FALLBACK_PERIOD_NANOS).unwrap(),
    )?;
    let mut outputs = vec![cpu_path.display().to_string()];
    if let Some(format) = args.firefox_profile {
        let extension = match format {
            crate::FirefoxProfileFormat::Json => "json.gz",
            crate::FirefoxProfileFormat::Jslb => "jslb.gz",
        };
        let path = args.output.join(format!("firefox-{basename}.{extension}"));
        write_firefox_profile(
            &path,
            &window.timeline,
            format,
            started,
            1000,
            source_name,
            None,
        )?;
        outputs.push(path.display().to_string());
    }
    let diagnostics_path = args.output.join(format!("diagnostics-{basename}.json"));
    outputs.push(diagnostics_path.display().to_string());
    write_json_atomic(
        &diagnostics_path,
        &ImportDiagnostics {
            schema_version: 3,
            source: args.input.display().to_string(),
            format: match args.format {
                crate::cli::ImportFormat::Auto => "auto",
                crate::cli::ImportFormat::PerfData => "perf_data",
                crate::cli::ImportFormat::Simpleperf => "simpleperf",
            }
            .to_owned(),
            started_unix_nanos: started,
            ended_unix_nanos: ended,
            samples: window.total_samples,
            aggregation_dropped_samples: window.aggregation_dropped_samples,
            timeline_dropped_samples: window.timeline_dropped_samples,
            outputs,
        },
    )?;
    Ok(())
}
