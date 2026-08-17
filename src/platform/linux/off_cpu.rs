use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use libbpf_rs::{
    Link, MapCore, MapFlags, Object, ObjectBuilder, RingBuffer, RingBufferBuilder,
    TracepointCategory,
};

use crate::{
    diagnostics::OffCpuWindowDiagnostics,
    process::read_threads,
    profile::{AttributedStack, Frame, OffCpuValues, Stack},
};

const EVENT_HEADER_SIZE: usize = 24;
const EVENT_STACK_SIZE: usize = EVENT_HEADER_SIZE + 127 * 8;
const EVENT_SWITCH_OUT: u32 = 1;
const EVENT_SWITCH_IN: u32 = 2;
const STAT_COUNT: u32 = 2;
const STAT_RINGBUF_DROPS: usize = 0;
const STAT_STACK_FAILURES: usize = 1;
const EVENT_QUEUE_CAPACITY: usize = 4096;
const EVENT_BUFFER_POOL_CAPACITY: usize = 256;

static OFF_CPU_BPF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/off_cpu.bpf.o"));

#[derive(Clone, Copy, Default)]
struct KernelStats([u64; STAT_COUNT as usize]);

struct PendingInterval {
    pid: u32,
    started: u64,
    stack: Stack,
}

pub struct OffCpuCollector {
    ring: RingBuffer<'static>,
    _link: Link,
    object: Object,
    receiver: Receiver<Vec<u8>>,
    recycle_sender: Sender<Vec<u8>>,
    userspace_drops: Arc<AtomicU64>,
    pid: i32,
    tracked_tids: HashMap<u32, u32>,
    cgroup_path: Option<std::path::PathBuf>,
    pending: HashMap<u32, PendingInterval>,
    samples: HashMap<AttributedStack, OffCpuValues>,
    max_stacks: usize,
    max_threads: usize,
    previous_stats: KernelStats,
    switch_out_events: u64,
    completed_intervals: u64,
    incomplete_intervals: u64,
    aggregation_dropped_events: u64,
    window_started: Instant,
}

impl OffCpuCollector {
    pub fn probe_load() -> Result<()> {
        let _object = ObjectBuilder::default()
            .open_memory(OFF_CPU_BPF)
            .context("failed to open off-CPU BPF object")?
            .load()
            .context("failed to load off-CPU BPF object")?;
        Ok(())
    }

    pub fn attach(
        pid: i32,
        cgroup_path: Option<std::path::PathBuf>,
        max_stacks: usize,
        max_threads: usize,
    ) -> Result<Self> {
        let object = ObjectBuilder::default()
            .open_memory(OFF_CPU_BPF)
            .context("failed to open off-CPU BPF object")?
            .load()
            .context("failed to load off-CPU BPF object")?;
        let link = object
            .progs_mut()
            .find(|program| program.name() == "sched_switch")
            .context("off-CPU sched_switch program is missing")?
            .attach_tracepoint(TracepointCategory::Sched, "sched_switch")
            .context("failed to attach off-CPU sched_switch tracepoint")?;
        let (sender, receiver) = bounded(EVENT_QUEUE_CAPACITY);
        let (recycle_sender, recycle_receiver) = bounded(EVENT_BUFFER_POOL_CAPACITY);
        for _ in 0..EVENT_BUFFER_POOL_CAPACITY {
            let _ = recycle_sender.try_send(Vec::with_capacity(EVENT_STACK_SIZE));
        }
        let userspace_drops = Arc::new(AtomicU64::new(0));
        let callback_drops = Arc::clone(&userspace_drops);
        let callback_recycle = recycle_sender.clone();
        let events = object
            .maps()
            .find(|map| map.name() == "off_cpu_events")
            .context("off-CPU events map is missing")?;
        let mut builder = RingBufferBuilder::new();
        builder.add(&events, move |bytes| {
            let mut buffer = recycle_receiver.try_recv().unwrap_or_default();
            buffer.clear();
            buffer.extend_from_slice(bytes);
            if let Err(error) = sender.try_send(buffer) {
                let mut buffer = match error {
                    TrySendError::Full(buffer) | TrySendError::Disconnected(buffer) => buffer,
                };
                buffer.clear();
                let _ = callback_recycle.try_send(buffer);
                callback_drops.fetch_add(1, Ordering::Relaxed);
            }
            0
        })?;
        let ring = builder.build()?;
        let mut collector = Self {
            ring,
            _link: link,
            object,
            receiver,
            recycle_sender,
            userspace_drops,
            pid,
            tracked_tids: HashMap::new(),
            cgroup_path,
            pending: HashMap::new(),
            samples: HashMap::new(),
            max_stacks,
            max_threads,
            previous_stats: KernelStats::default(),
            switch_out_events: 0,
            completed_intervals: 0,
            incomplete_intervals: 0,
            aggregation_dropped_events: 0,
            window_started: Instant::now(),
        };
        collector.reconcile_threads()?;
        collector.previous_stats = collector.read_stats()?;
        Ok(collector)
    }

    pub fn reconcile_threads(&mut self) -> Result<()> {
        let pids = if let Some(path) = self.cgroup_path.as_deref() {
            std::fs::read_to_string(path.join("cgroup.procs"))
                .with_context(|| format!("failed to read {}", path.join("cgroup.procs").display()))?
                .lines()
                .filter_map(|pid| pid.parse::<i32>().ok())
                .collect::<Vec<_>>()
        } else {
            vec![self.pid]
        };
        let mut current = HashMap::new();
        for pid in pids {
            for tid in read_threads(pid).unwrap_or_default() {
                if let (Ok(tid), Ok(pid)) = (u32::try_from(tid), u32::try_from(pid)) {
                    current.insert(tid, pid);
                }
            }
        }
        self.apply_threads(current)
    }

    pub fn reconcile_thread_snapshot(
        &mut self,
        threads_by_pid: &HashMap<i32, BTreeSet<i32>>,
    ) -> Result<()> {
        let mut current = HashMap::new();
        for (pid, threads) in threads_by_pid {
            for tid in threads {
                if let (Ok(tid), Ok(pid)) = (u32::try_from(*tid), u32::try_from(*pid)) {
                    current.insert(tid, pid);
                }
            }
        }
        self.apply_threads(current)
    }

    fn apply_threads(&mut self, current: HashMap<u32, u32>) -> Result<()> {
        if current.len() > self.max_threads {
            anyhow::bail!(
                "target has {} threads, exceeding --max-threads={}",
                current.len(),
                self.max_threads
            );
        }
        let map = self
            .object
            .maps_mut()
            .find(|map| map.name() == "tracked_tids")
            .context("off-CPU tracked_tids map is missing")?;
        for (tid, pid) in &current {
            if self.tracked_tids.get(tid) == Some(pid) {
                continue;
            }
            map.update(&tid.to_ne_bytes(), &pid.to_ne_bytes(), MapFlags::ANY)?;
        }
        for tid in self
            .tracked_tids
            .keys()
            .filter(|tid| !current.contains_key(tid))
        {
            let _ = map.delete(&tid.to_ne_bytes());
            if self.pending.remove(tid).is_some() {
                self.incomplete_intervals = self.incomplete_intervals.saturating_add(1);
            }
        }
        self.tracked_tids = current;
        Ok(())
    }

    pub fn drain(&mut self) -> Result<()> {
        self.ring
            .consume()
            .context("failed to consume off-CPU events")?;
        while let Ok(mut bytes) = self.receiver.try_recv() {
            self.apply(&bytes);
            bytes.clear();
            let _ = self.recycle_sender.try_send(bytes);
        }
        Ok(())
    }

    pub fn snapshot_window(
        &mut self,
    ) -> Result<(
        HashMap<AttributedStack, OffCpuValues>,
        OffCpuWindowDiagnostics,
    )> {
        self.drain()?;
        let boundary = monotonic_nanos();
        let pending = self
            .pending
            .iter_mut()
            .map(|(tid, interval)| {
                let elapsed = boundary.saturating_sub(interval.started);
                interval.started = boundary;
                (*tid, interval.pid, interval.stack.clone(), elapsed)
            })
            .collect::<Vec<_>>();
        for (tid, pid, stack, elapsed) in pending {
            self.aggregate(pid, tid, stack, elapsed, false);
        }
        let current = self.read_stats()?;
        let ring_buffer_drops = current.0[STAT_RINGBUF_DROPS]
            .saturating_sub(self.previous_stats.0[STAT_RINGBUF_DROPS])
            .saturating_add(self.userspace_drops.swap(0, Ordering::Relaxed));
        let stack_failures = current.0[STAT_STACK_FAILURES]
            .saturating_sub(self.previous_stats.0[STAT_STACK_FAILURES]);
        self.previous_stats = current;
        let samples = std::mem::take(&mut self.samples);
        let diagnostics = OffCpuWindowDiagnostics {
            requested: true,
            enabled: true,
            switch_out_events: std::mem::take(&mut self.switch_out_events),
            completed_intervals: std::mem::take(&mut self.completed_intervals),
            incomplete_intervals: std::mem::take(&mut self.incomplete_intervals)
                .saturating_add(stack_failures),
            nanoseconds: samples.values().fold(0_i64, |total, value| {
                total.saturating_add(value.nanoseconds)
            }),
            aggregation_dropped_events: std::mem::take(&mut self.aggregation_dropped_events),
            ring_buffer_drops,
            reason: None,
        };
        self.window_started = Instant::now();
        Ok((samples, diagnostics))
    }

    fn apply(&mut self, bytes: &[u8]) {
        if bytes.len() < EVENT_HEADER_SIZE {
            self.incomplete_intervals = self.incomplete_intervals.saturating_add(1);
            return;
        }
        let kind = read_u32(bytes, 0);
        let pid = read_u32(bytes, 4);
        let tid = read_u32(bytes, 8);
        let stack_len = read_i32(bytes, 12).max(0) as usize;
        let timestamp = read_u64(bytes, 16);
        if kind == EVENT_SWITCH_OUT {
            if bytes.len() < EVENT_STACK_SIZE {
                self.incomplete_intervals = self.incomplete_intervals.saturating_add(1);
                return;
            }
            self.switch_out_events = self.switch_out_events.saturating_add(1);
            let frame_count = (stack_len / 8).min(127);
            let frames = (0..frame_count)
                .map(|index| Frame {
                    address: read_u64(bytes, 24 + index * 8),
                })
                .filter(|frame| frame.address != 0)
                .collect::<Vec<_>>();
            if frames.is_empty() {
                self.incomplete_intervals = self.incomplete_intervals.saturating_add(1);
                return;
            }
            if self
                .pending
                .insert(
                    tid,
                    PendingInterval {
                        pid,
                        started: timestamp,
                        stack: Stack::from(frames),
                    },
                )
                .is_some()
            {
                self.incomplete_intervals = self.incomplete_intervals.saturating_add(1);
            }
        } else if kind == EVENT_SWITCH_IN
            && let Some(interval) = self.pending.remove(&tid)
        {
            self.completed_intervals = self.completed_intervals.saturating_add(1);
            self.aggregate(
                interval.pid,
                tid,
                interval.stack,
                timestamp.saturating_sub(interval.started),
                true,
            );
        }
    }

    fn aggregate(&mut self, pid: u32, tid: u32, stack: Stack, elapsed: u64, completed: bool) {
        let name = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/comm"))
            .ok()
            .map(|name| name.trim_end().to_owned());
        let key = AttributedStack {
            stack,
            pid,
            tid,
            thread_name: name,
        };
        let full = self.samples.len() >= self.max_stacks;
        let value = match self.samples.entry(key) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) if !full => {
                entry.insert(OffCpuValues::default())
            }
            std::collections::hash_map::Entry::Vacant(_) => {
                self.aggregation_dropped_events = self.aggregation_dropped_events.saturating_add(1);
                return;
            }
        };
        if completed {
            value.events = value.events.saturating_add(1);
        }
        value.nanoseconds = value
            .nanoseconds
            .saturating_add(i64::try_from(elapsed).unwrap_or(i64::MAX));
    }

    fn read_stats(&self) -> Result<KernelStats> {
        let map = self
            .object
            .maps()
            .find(|map| map.name() == "off_cpu_stats")
            .context("off-CPU stats map is missing")?;
        let mut stats = KernelStats::default();
        for index in 0..STAT_COUNT {
            if let Some(per_cpu) = map.lookup_percpu(&index.to_ne_bytes(), MapFlags::ANY)? {
                stats.0[index as usize] = per_cpu
                    .iter()
                    .filter_map(|value| value.get(..8)?.try_into().ok().map(u64::from_ne_bytes))
                    .sum();
            }
        }
        Ok(stats)
    }
}

fn monotonic_nanos() -> u64 {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: value is a valid writable timespec and CLOCK_MONOTONIC is supported on Linux.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) } != 0 {
        return 0;
    }
    u64::try_from(value.tv_sec)
        .unwrap_or_default()
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(value.tv_nsec).unwrap_or_default())
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    bytes
        .get(offset..offset + 4)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_ne_bytes)
        .unwrap_or_default()
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    bytes
        .get(offset..offset + 4)
        .and_then(|value| value.try_into().ok())
        .map(i32::from_ne_bytes)
        .unwrap_or_default()
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    bytes
        .get(offset..offset + 8)
        .and_then(|value| value.try_into().ok())
        .map(u64::from_ne_bytes)
        .unwrap_or_default()
}
