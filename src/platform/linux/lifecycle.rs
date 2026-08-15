use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use anyhow::{Context, Result};
use libbpf_rs::{
    Link, MapCore, Object, ObjectBuilder, RingBuffer, RingBufferBuilder, TracepointCategory,
};

static LIFECYCLE_BPF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/lifecycle.bpf.o"));

pub struct LifecycleNotifier {
    ring: RingBuffer<'static>,
    _links: Vec<Link>,
    _object: Object,
    pending: Arc<AtomicU8>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LifecycleEvents {
    pub thread_change: bool,
    pub exec: bool,
}

impl LifecycleNotifier {
    pub fn probe_load(pid: i32) -> Result<()> {
        let _object = load_object(pid)?;
        Ok(())
    }

    pub fn attach(pid: i32) -> Result<Self> {
        let object = load_object(pid)?;

        let fork = object
            .progs_mut()
            .find(|program| program.name() == "process_fork")
            .context("lifecycle fork program is missing")?
            .attach_tracepoint(TracepointCategory::Sched, "sched_process_fork")?;
        let exit = object
            .progs_mut()
            .find(|program| program.name() == "process_exit")
            .context("lifecycle exit program is missing")?
            .attach_tracepoint(TracepointCategory::Sched, "sched_process_exit")?;
        let exec = object
            .progs_mut()
            .find(|program| program.name() == "process_exec")
            .context("lifecycle exec program is missing")?
            .attach_tracepoint(TracepointCategory::Sched, "sched_process_exec")?;

        let pending = Arc::new(AtomicU8::new(0));
        let callback_pending = Arc::clone(&pending);
        let events = object
            .maps()
            .find(|map| map.name() == "lifecycle_events")
            .context("lifecycle BPF ring map is missing")?;
        let mut ring_builder = RingBufferBuilder::new();
        ring_builder.add(&events, move |data| {
            let kind = data
                .get(0..4)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u32::from_ne_bytes)
                .unwrap_or_default();
            let flag = if kind == 3 { 2 } else { 1 };
            callback_pending.fetch_or(flag, Ordering::Relaxed);
            0
        })?;
        let ring = ring_builder.build()?;
        Ok(Self {
            ring,
            _links: vec![fork, exit, exec],
            _object: object,
            pending,
        })
    }

    pub fn consume(&self) -> Result<LifecycleEvents> {
        self.ring.consume()?;
        let flags = self.pending.swap(0, Ordering::Relaxed);
        Ok(LifecycleEvents {
            thread_change: flags & 1 != 0,
            exec: flags & 2 != 0,
        })
    }
}

fn load_object(pid: i32) -> Result<Object> {
    let mut builder = ObjectBuilder::default();
    let mut open = builder
        .open_memory(LIFECYCLE_BPF)
        .context("failed to open lifecycle BPF object")?;
    let mut rodata = open
        .maps_mut()
        .find(|map| map.name().to_string_lossy().ends_with(".rodata"))
        .context("lifecycle BPF rodata map is missing")?;
    let mut initial = rodata
        .initial_value()
        .context("lifecycle BPF rodata has no initial value")?
        .to_vec();
    initial
        .get_mut(0..4)
        .context("lifecycle BPF rodata is unexpectedly short")?
        .copy_from_slice(&(pid as u32).to_ne_bytes());
    rodata.set_initial_value(&initial)?;
    open.load().context("failed to load lifecycle BPF object")
}
