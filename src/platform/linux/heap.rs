use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use libbpf_rs::{Link, MapCore, MapFlags, Object, ObjectBuilder, RingBuffer, RingBufferBuilder};
use object::{Object as _, ObjectSegment, ObjectSymbol, SymbolKind};

use crate::{
    config::{DEFAULT_MAX_FRAMES, UnwindMode},
    diagnostics::{AllocatorReport, HeapWindowDiagnostics},
    heap::{HeapEvent, HeapEventKind, HeapState},
    maps::{ExecutableRanges, read_process_maps},
    process::mapped_module_path,
    profile::{HeapValues, Stack, has_address_cycle},
    unwind::{DwarfUnwinder, RawRegisters},
};

const HEADER_SIZE: usize = 88;
const STAT_COUNT: u32 = 8;
const STAT_RINGBUF_DROPS: usize = 0;
const STAT_PENDING_OVERWRITES: usize = 1;
const STAT_MAP_UPDATE_FAILURES: usize = 2;
const STAT_STACK_FAILURES: usize = 3;
const STAT_ALLOC_EVENTS: usize = 4;
const STAT_SAMPLED_ALLOCS: usize = 5;
const STAT_SAMPLED_FREES: usize = 6;
const STAT_MAP_EVICTIONS: usize = 7;

const EVENT_STACK: u32 = 1;
const EVENT_ALLOC: u32 = 2;
const EVENT_FREE: u32 = 3;
const EVENT_BUFFER_POOL_CAPACITY: usize = 256;

static HEAP_BPF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/heap.bpf.o"));

#[derive(Clone, Copy, Debug, Default)]
struct KernelStats([u64; STAT_COUNT as usize]);

impl KernelStats {
    fn delta(self, earlier: Self) -> Self {
        let mut values = [0; STAT_COUNT as usize];
        for (index, value) in values.iter_mut().enumerate() {
            *value = self.0[index].saturating_sub(earlier.0[index]);
        }
        Self(values)
    }
}

pub struct HeapCollector {
    ring: RingBuffer<'static>,
    links: Vec<Link>,
    object: Object,
    receiver: Receiver<Vec<u8>>,
    recycle_sender: Sender<Vec<u8>>,
    pending_stacks: HashMap<u64, Vec<u64>>,
    state: HeapState,
    dwarf: Option<DwarfUnwinder>,
    allow_partial: bool,
    allocator: String,
    previous_stats: KernelStats,
    local_stack_failures: u64,
    local_stack_samples: u64,
    local_usable_stacks: u64,
    local_depth_sum: u64,
    fatal_unwind_error: Option<String>,
    executable_ranges: ExecutableRanges,
    seen_addresses: HashSet<u64>,
}

impl HeapCollector {
    pub fn probe_load(pid: i32, report: &AllocatorReport) -> Result<()> {
        let family = report
            .detected
            .as_deref()
            .context("allocator family was not selected")?;
        let module = report
            .module
            .as_deref()
            .context("allocator module was not selected")?;
        let maps = read_process_maps(pid)?;
        let mapping = maps
            .iter()
            .find(|mapping| mapping.path.as_deref() == Some(module));
        let process_path = mapped_module_path(pid, module, mapping);
        let offsets = symbol_offsets(&process_path)?;
        for (_, symbol, _) in required_probes(family)? {
            if !offsets.contains_key(*symbol) {
                bail!("allocator symbol {symbol} has no attachable file offset");
            }
        }
        let _object = load_heap_object(512 * 1024, UnwindMode::Fp)?;
        Ok(())
    }

    pub fn attach(
        pid: i32,
        report: &AllocatorReport,
        allocation_interval: u64,
        mode: UnwindMode,
        allow_partial: bool,
        max_stacks: usize,
    ) -> Result<Self> {
        let family = report
            .detected
            .as_deref()
            .context("allocator family was not selected")?;
        let module = report
            .module
            .as_deref()
            .context("allocator module was not selected")?;
        let maps = read_process_maps(pid)?;
        let mapping = maps
            .iter()
            .find(|mapping| mapping.path.as_deref() == Some(module));
        let process_path = mapped_module_path(pid, module, mapping);
        let offsets = symbol_offsets(&process_path)?;

        let mut object = load_heap_object(allocation_interval, mode)?;
        let links = attach_allocator_programs(&mut object, pid, &process_path, family, &offsets)?;

        let (sender, receiver) = unbounded();
        let (recycle_sender, recycle_receiver) = bounded::<Vec<u8>>(EVENT_BUFFER_POOL_CAPACITY);
        let mut ring_builder = RingBufferBuilder::new();
        let events = object
            .maps()
            .find(|map| map.name() == "events")
            .context("heap BPF events map is missing")?;
        ring_builder.add(&events, move |data| {
            let mut bytes = recycle_receiver.try_recv().unwrap_or_default();
            bytes.clear();
            bytes.extend_from_slice(data);
            let _ = sender.send(bytes);
            0
        })?;
        let ring = ring_builder.build()?;
        let dwarf = if mode == UnwindMode::Dwarf {
            Some(DwarfUnwinder::for_process(pid)?)
        } else {
            None
        };

        let mut collector = Self {
            ring,
            links,
            object,
            receiver,
            recycle_sender,
            pending_stacks: HashMap::new(),
            state: HeapState::with_max_stacks(max_stacks),
            dwarf,
            allow_partial,
            allocator: family.to_owned(),
            previous_stats: KernelStats::default(),
            local_stack_failures: 0,
            local_stack_samples: 0,
            local_usable_stacks: 0,
            local_depth_sum: 0,
            fatal_unwind_error: None,
            executable_ranges: ExecutableRanges::from_maps(&maps),
            seen_addresses: HashSet::new(),
        };
        collector.previous_stats = collector.read_kernel_stats()?;
        Ok(collector)
    }

    pub fn drain(&mut self) -> Result<()> {
        self.ring
            .consume()
            .context("failed to consume heap BPF events")?;
        while let Ok(mut bytes) = self.receiver.try_recv() {
            self.apply_raw_event(&bytes);
            bytes.clear();
            let _ = self.recycle_sender.try_send(bytes);
        }
        if let Some(error) = self.fatal_unwind_error.take() {
            bail!("heap stack address normalization failed: {error}");
        }
        Ok(())
    }

    pub fn snapshot_window(
        &mut self,
    ) -> Result<(HashMap<Stack, HeapValues>, HeapWindowDiagnostics)> {
        self.drain()?;
        let current = self.read_kernel_stats()?;
        let delta = current.delta(self.previous_stats);
        self.previous_stats = current;
        if delta.0[STAT_MAP_EVICTIONS] != 0 || delta.0[STAT_RINGBUF_DROPS] != 0 {
            let live_pointers = self.live_kernel_pointers();
            self.state.retain_live_pointers(&live_pointers);
        }
        let unfinished_returns = self.pending_kernel_count();
        let stack_samples = std::mem::take(&mut self.local_stack_samples);
        let usable_stacks = std::mem::take(&mut self.local_usable_stacks);
        let depth_sum = std::mem::take(&mut self.local_depth_sum);
        let local_failures = std::mem::take(&mut self.local_stack_failures);
        let (profile, aggregation_drops) = self.state.snapshot_window_with_drops();
        let diagnostics = HeapWindowDiagnostics {
            allocator: Some(self.allocator.clone()),
            allocation_events: delta.0[STAT_ALLOC_EVENTS],
            sampled_allocations: delta.0[STAT_SAMPLED_ALLOCS],
            sampled_frees: delta.0[STAT_SAMPLED_FREES],
            alloc_objects: 0,
            alloc_space: 0,
            inuse_objects: 0,
            inuse_space: 0,
            aggregation_dropped_alloc_objects: aggregation_drops.alloc_objects,
            aggregation_dropped_alloc_space: aggregation_drops.alloc_space,
            aggregation_dropped_inuse_objects: aggregation_drops.inuse_objects,
            aggregation_dropped_inuse_space: aggregation_drops.inuse_space,
            live_samples: self.state.live_sample_count() as u64,
            ring_buffer_drops: delta.0[STAT_RINGBUF_DROPS],
            map_evictions: delta.0[STAT_MAP_EVICTIONS],
            map_update_failures: delta.0[STAT_MAP_UPDATE_FAILURES],
            pending_overwrites: delta.0[STAT_PENDING_OVERWRITES],
            unfinished_returns,
            stack_samples,
            usable_stacks,
            stack_failures: delta.0[STAT_STACK_FAILURES].max(local_failures),
            average_depth: if usable_stacks == 0 {
                0.0
            } else {
                depth_sum as f64 / usable_stacks as f64
            },
            symbolized_locations: 0,
            total_locations: 0,
            since_attach: true,
        };
        Ok((profile, diagnostics))
    }

    pub fn clear_for_exec(&mut self) {
        self.pending_stacks.clear();
        self.state.clear_live();
    }

    pub fn refresh_unwinder(&mut self, pid: i32) -> Result<()> {
        let maps = read_process_maps(pid)?;
        self.executable_ranges = ExecutableRanges::from_maps(&maps);
        if self.dwarf.is_some() {
            self.dwarf = Some(DwarfUnwinder::for_process(pid)?);
        }
        Ok(())
    }

    pub fn link_count(&self) -> usize {
        self.links.len()
    }

    fn apply_raw_event(&mut self, bytes: &[u8]) {
        let Some(header) = EventHeader::parse(bytes) else {
            self.local_stack_failures += 1;
            return;
        };
        match header.kind {
            EVENT_STACK => {
                self.local_stack_samples += 1;
                let frames = if header.unwind_mode == 1 {
                    parse_fp_stack(bytes, &header)
                } else {
                    self.unwind_stack(bytes, &header)
                };
                let cyclic = has_address_cycle(&frames, &mut self.seen_addresses);
                let invalid = frames
                    .iter()
                    .any(|address| !self.executable_ranges.contains(*address));
                if frames.is_empty() || cyclic || invalid {
                    self.local_stack_failures += 1;
                } else {
                    self.local_usable_stacks += 1;
                    self.local_depth_sum = self.local_depth_sum.saturating_add(frames.len() as u64);
                    self.pending_stacks.insert(header.token, frames);
                    if self.pending_stacks.len() > 65_536 {
                        let mut tokens = self.pending_stacks.keys().copied().collect::<Vec<_>>();
                        let middle = tokens.len() / 2;
                        tokens.select_nth_unstable(middle);
                        let cutoff = tokens[middle];
                        let before = self.pending_stacks.len();
                        self.pending_stacks.retain(|token, _| *token >= cutoff);
                        self.local_stack_failures = self
                            .local_stack_failures
                            .saturating_add((before - self.pending_stacks.len()) as u64);
                    }
                }
            }
            EVENT_ALLOC => {
                let Some(frames) = self.pending_stacks.remove(&header.token) else {
                    self.local_stack_failures += 1;
                    return;
                };
                self.state.apply(HeapEvent {
                    kind: HeapEventKind::Alloc,
                    pointer: header.pointer,
                    size: header.size,
                    weight: header.weight,
                    frames,
                });
            }
            EVENT_FREE => self.state.apply(HeapEvent {
                kind: HeapEventKind::Free,
                pointer: header.pointer,
                size: 0,
                weight: 0,
                frames: Vec::new(),
            }),
            _ => {}
        }
    }

    fn unwind_stack(&mut self, bytes: &[u8], header: &EventHeader) -> Vec<u64> {
        let stack_length = usize::try_from(header.stack_length.max(0))
            .unwrap_or_default()
            .min(bytes.len().saturating_sub(HEADER_SIZE));
        let registers = RawRegisters {
            ip: header.ip,
            sp: header.sp,
            fp: header.fp,
            lr: header.lr,
        };
        let Some(unwinder) = self.dwarf.as_mut() else {
            return self
                .allow_partial
                .then_some(vec![header.ip])
                .unwrap_or_default();
        };
        let outcome =
            unwinder.unwind_bytes(registers, &bytes[HEADER_SIZE..HEADER_SIZE + stack_length]);
        if outcome.fatal {
            self.fatal_unwind_error = outcome.error.clone();
            return Vec::new();
        }
        let leaf_only_failure = outcome.frames.len() == 1 && outcome.error.is_some();
        if (outcome.frames.is_empty() || leaf_only_failure) && self.allow_partial && header.ip != 0
        {
            vec![header.ip]
        } else if leaf_only_failure {
            Vec::new()
        } else {
            outcome.frames
        }
    }

    fn read_kernel_stats(&self) -> Result<KernelStats> {
        let map = self
            .object
            .maps()
            .find(|map| map.name() == "stats")
            .context("heap BPF stats map is missing")?;
        let mut totals = [0_u64; STAT_COUNT as usize];
        for index in 0..STAT_COUNT {
            if let Some(per_cpu) = map.lookup_percpu(&index.to_ne_bytes(), MapFlags::ANY)? {
                for value in per_cpu {
                    if value.len() >= 8 {
                        totals[index as usize] = totals[index as usize].saturating_add(
                            u64::from_ne_bytes(value[0..8].try_into().expect("eight bytes")),
                        );
                    }
                }
            }
        }
        Ok(KernelStats(totals))
    }

    fn live_kernel_pointers(&self) -> HashSet<u64> {
        self.object
            .maps()
            .find(|map| map.name() == "live_samples")
            .map(|map| {
                map.keys()
                    .filter_map(|key| key.get(0..8)?.try_into().ok().map(u64::from_ne_bytes))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn pending_kernel_count(&self) -> u64 {
        let Some(map) = self.object.maps().find(|map| map.name() == "pending") else {
            return 0;
        };
        map.keys().count() as u64
    }
}

fn parse_fp_stack(bytes: &[u8], header: &EventHeader) -> Vec<u64> {
    let stack_length = usize::try_from(header.stack_length.max(0))
        .unwrap_or_default()
        .min(DEFAULT_MAX_FRAMES * 8)
        .min(bytes.len().saturating_sub(HEADER_SIZE));
    let mut frames = bytes[HEADER_SIZE..HEADER_SIZE + stack_length]
        .chunks_exact(8)
        .map(|address| u64::from_ne_bytes(address.try_into().expect("eight-byte chunk")))
        .filter(|address| *address != 0 && *address < u64::MAX - 4095)
        .collect::<Vec<_>>();
    if header.ip != 0 && frames.first().copied() != Some(header.ip) {
        frames.insert(0, header.ip);
    }
    frames.truncate(DEFAULT_MAX_FRAMES);
    frames
}

#[derive(Debug)]
struct EventHeader {
    kind: u32,
    unwind_mode: u32,
    token: u64,
    pointer: u64,
    size: u64,
    weight: u64,
    ip: u64,
    sp: u64,
    fp: u64,
    lr: u64,
    stack_length: i32,
}

impl EventHeader {
    fn parse(bytes: &[u8]) -> Option<Self> {
        (bytes.len() >= HEADER_SIZE).then(|| Self {
            kind: read_u32(bytes, 0).expect("header was length checked"),
            unwind_mode: read_u32(bytes, 4).expect("header was length checked"),
            token: read_u64(bytes, 16).expect("header was length checked"),
            pointer: read_u64(bytes, 24).expect("header was length checked"),
            size: read_u64(bytes, 32).expect("header was length checked"),
            weight: read_u64(bytes, 40).expect("header was length checked"),
            ip: read_u64(bytes, 48).expect("header was length checked"),
            sp: read_u64(bytes, 56).expect("header was length checked"),
            fp: read_u64(bytes, 64).expect("header was length checked"),
            lr: read_u64(bytes, 72).expect("header was length checked"),
            stack_length: i32::from_ne_bytes(
                bytes[80..84].try_into().expect("header was length checked"),
            ),
        })
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn load_heap_object(allocation_interval: u64, mode: UnwindMode) -> Result<Object> {
    let mut builder = ObjectBuilder::default();
    let mut open = builder
        .open_memory(HEAP_BPF)
        .context("failed to open embedded heap BPF object")?;
    let mut rodata = open
        .maps_mut()
        .find(|map| map.name().to_string_lossy().ends_with(".rodata"))
        .context("heap BPF rodata map is missing")?;
    let mut initial = rodata
        .initial_value()
        .context("heap BPF rodata has no initial value")?
        .to_vec();
    if initial.len() < 12 {
        bail!("heap BPF rodata is unexpectedly short");
    }
    initial[0..8].copy_from_slice(&allocation_interval.to_ne_bytes());
    let bpf_mode = if mode == UnwindMode::Fp { 1_u32 } else { 2_u32 };
    initial[8..12].copy_from_slice(&bpf_mode.to_ne_bytes());
    rodata.set_initial_value(&initial)?;
    open.load().context("failed to load heap BPF object")
}

fn symbol_offsets(path: &Path) -> Result<HashMap<String, usize>> {
    let data = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let object = object::File::parse(data.as_slice()).context("allocator module is not ELF")?;
    let mut offsets = HashMap::new();
    for symbol in object.symbols().chain(object.dynamic_symbols()) {
        if symbol.kind() != SymbolKind::Text || symbol.address() == 0 {
            continue;
        }
        let Ok(name) = symbol.name() else {
            continue;
        };
        let Some(offset) = object.segments().find_map(|segment| {
            let range = segment.address()..segment.address().saturating_add(segment.size());
            range.contains(&symbol.address()).then(|| {
                let (file_offset, _) = segment.file_range();
                file_offset.saturating_add(symbol.address() - segment.address())
            })
        }) else {
            continue;
        };
        if let Ok(offset) = usize::try_from(offset) {
            offsets.entry(name.to_owned()).or_insert(offset);
        }
    }
    Ok(offsets)
}

fn attach_allocator_programs(
    object: &mut Object,
    pid: i32,
    path: &Path,
    family: &str,
    offsets: &HashMap<String, usize>,
) -> Result<Vec<Link>> {
    let required = required_probes(family)?;
    let mut links = Vec::new();
    for (program, symbol, retprobe) in required {
        links.push(attach_one(
            object, pid, path, program, symbol, *retprobe, offsets,
        )?);
    }
    if family == "system" {
        for (program, symbol, retprobe) in [
            ("system_aligned_alloc_enter", "aligned_alloc", false),
            ("system_aligned_alloc_exit", "aligned_alloc", true),
            ("system_posix_memalign_enter", "posix_memalign", false),
            ("system_posix_memalign_exit", "posix_memalign", true),
        ] {
            if offsets.contains_key(symbol) {
                links.push(attach_one(
                    object, pid, path, program, symbol, retprobe, offsets,
                )?);
            }
        }
    }
    Ok(links)
}

fn required_probes(family: &str) -> Result<&'static [(&'static str, &'static str, bool)]> {
    Ok(match family {
        "rust" => &[
            ("rust_alloc_enter", "__rust_alloc", false),
            ("rust_alloc_exit", "__rust_alloc", true),
            ("rust_alloc_zeroed_enter", "__rust_alloc_zeroed", false),
            ("rust_alloc_zeroed_exit", "__rust_alloc_zeroed", true),
            ("rust_realloc_enter", "__rust_realloc", false),
            ("rust_realloc_exit", "__rust_realloc", true),
            ("rust_dealloc_enter", "__rust_dealloc", false),
        ],
        "system" => &[
            ("system_malloc_enter", "malloc", false),
            ("system_malloc_exit", "malloc", true),
            ("system_calloc_enter", "calloc", false),
            ("system_calloc_exit", "calloc", true),
            ("system_realloc_enter", "realloc", false),
            ("system_realloc_exit", "realloc", true),
            ("system_free_enter", "free", false),
        ],
        other => bail!("unsupported allocator family {other}"),
    })
}

fn attach_one(
    object: &mut Object,
    pid: i32,
    path: &Path,
    program_name: &str,
    symbol_name: &str,
    retprobe: bool,
    offsets: &HashMap<String, usize>,
) -> Result<Link> {
    let offset = *offsets
        .get(symbol_name)
        .with_context(|| format!("allocator symbol {symbol_name} has no file offset"))?;
    let program = object
        .progs_mut()
        .find(|program| program.name() == program_name)
        .with_context(|| format!("heap BPF program {program_name} is missing"))?;
    program
        .attach_uprobe(retprobe, pid, path, offset)
        .with_context(|| format!("failed to attach {program_name} to {symbol_name}"))
}
