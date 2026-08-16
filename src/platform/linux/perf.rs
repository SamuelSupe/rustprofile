use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs::File,
    os::fd::{AsRawFd, FromRawFd},
    sync::atomic::{Ordering, fence},
};

use anyhow::{Context, Result, bail};
use memmap2::{MmapMut, MmapOptions};
use perf_event_open_sys::{bindings as perf, ioctls, perf_event_open};

use super::lifecycle::LifecycleNotifier;
use crate::{
    config::{DEFAULT_MAX_FRAMES, DEFAULT_STACK_BYTES, UnwindMode},
    maps::ExecutableRanges,
    process::read_threads,
    profile::has_address_cycle,
    unwind::{RawRegisters, StackSnapshot},
};

const DATA_PAGES: usize = 16;
const STACK_BUFFER_POOL_CAPACITY: usize = 256;

#[derive(Debug)]
pub enum PerfSampleData {
    FramePointer(Vec<u64>),
    Dwarf(StackSnapshot),
}

#[derive(Debug)]
pub struct PerfSample {
    pub tid: u32,
    pub time: u64,
    pub period: u64,
    pub data: PerfSampleData,
}

#[derive(Debug, Default)]
pub struct PerfBatch {
    pub samples: Vec<PerfSample>,
    pub lost_samples: u64,
    pub malformed_records: u64,
    stack_buffers: Vec<Vec<u8>>,
}

impl PerfBatch {
    pub fn recycle_stack_buffer(&mut self, mut buffer: Vec<u8>) {
        if self.stack_buffers.len() < STACK_BUFFER_POOL_CAPACITY {
            buffer.clear();
            self.stack_buffers.push(buffer);
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FpQuality {
    pub samples: u64,
    pub usable_samples: u64,
    pub valid_addresses: u64,
    pub total_addresses: u64,
    pub deep_samples: u64,
    pub cyclic_samples: u64,
    seen_addresses: HashSet<u64>,
}

impl FpQuality {
    pub fn observe(&mut self, frames: &[u64], executable_ranges: &ExecutableRanges) {
        self.samples += 1;
        self.total_addresses += frames.len() as u64;
        let valid = frames
            .iter()
            .filter(|address| executable_ranges.contains(**address))
            .count() as u64;
        self.valid_addresses += valid;
        if frames.len() >= 3 {
            self.deep_samples += 1;
        }
        let cyclic = has_address_cycle(frames, &mut self.seen_addresses);
        if cyclic {
            self.cyclic_samples += 1;
        }
        if !frames.is_empty() && valid == frames.len() as u64 && !cyclic {
            self.usable_samples += 1;
        }
    }

    pub fn calibration_passes(&self) -> bool {
        self.samples >= 64
            && ratio(self.valid_addresses, self.total_addresses) >= 0.90
            && ratio(self.deep_samples, self.samples) >= 0.70
            && self.cyclic_samples == 0
    }

    pub fn rejection_reason(&self) -> String {
        if self.samples < 64 {
            return format!(
                "only {} samples were collected during FP calibration",
                self.samples
            );
        }
        if ratio(self.valid_addresses, self.total_addresses) < 0.90 {
            return format!(
                "only {:.1}% of FP addresses were executable",
                ratio(self.valid_addresses, self.total_addresses) * 100.0
            );
        }
        if ratio(self.deep_samples, self.samples) < 0.70 {
            return format!(
                "only {:.1}% of FP samples reached three frames",
                ratio(self.deep_samples, self.samples) * 100.0
            );
        }
        if self.cyclic_samples != 0 {
            return format!(
                "{} FP samples contained an address cycle",
                self.cyclic_samples
            );
        }
        "frame-pointer calibration failed".to_owned()
    }
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

pub struct PerfCollector {
    pid: i32,
    mode: UnwindMode,
    frequency: u32,
    max_threads: usize,
    events: HashMap<i32, PerfEvent>,
    retired_events: Vec<PerfEvent>,
    poll_fds: Vec<libc::pollfd>,
    poll_tids: Vec<i32>,
}

pub fn probe_access(pid: i32) -> Result<()> {
    let tid = read_threads(pid)?
        .into_iter()
        .next()
        .context("target has no threads")?;
    LifecycleNotifier::probe_load(pid)?;
    let mut attribute = perf::perf_event_attr {
        size: std::mem::size_of::<perf::perf_event_attr>() as u32,
        type_: perf::PERF_TYPE_SOFTWARE,
        config: perf::PERF_COUNT_SW_CPU_CLOCK as u64,
        ..Default::default()
    };
    attribute.set_disabled(1);
    attribute.set_exclude_kernel(1);
    attribute.set_exclude_hv(1);
    // SAFETY: attribute is zero-initialized and requests a disabled per-thread event.
    let fd = unsafe {
        perf_event_open(
            &mut attribute,
            tid,
            -1,
            -1,
            perf::PERF_FLAG_FD_CLOEXEC as libc::c_ulong,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("perf access probe failed");
    }
    // SAFETY: perf_event_open returned a new owned descriptor.
    let _event = unsafe { File::from_raw_fd(fd) };
    Ok(())
}

impl PerfCollector {
    pub fn new(pid: i32, mode: UnwindMode, frequency: u32, max_threads: usize) -> Result<Self> {
        if mode == UnwindMode::Auto {
            bail!("perf collector requires a concrete FP or DWARF mode");
        }
        let mut collector = Self {
            pid,
            mode,
            frequency,
            max_threads,
            events: HashMap::new(),
            retired_events: Vec::new(),
            poll_fds: Vec::new(),
            poll_tids: Vec::new(),
        };
        collector.reconcile_threads()?;
        Ok(collector)
    }

    pub fn reconcile_threads(&mut self) -> Result<()> {
        let threads = read_threads(self.pid)?;
        if threads.len() > self.max_threads {
            bail!(
                "target has {} threads, exceeding the configured maximum of {}",
                threads.len(),
                self.max_threads
            );
        }
        let active = threads.iter().copied().collect::<HashSet<_>>();
        let retired = self
            .events
            .keys()
            .filter(|tid| !active.contains(tid))
            .copied()
            .collect::<Vec<_>>();
        self.retired_events.reserve(retired.len());
        for tid in retired {
            if let Some(event) = self.events.remove(&tid) {
                self.retired_events.push(event);
            }
        }
        for tid in threads {
            if !self.events.contains_key(&tid) {
                let event = match PerfEvent::open(tid, self.mode, self.frequency) {
                    Ok(event) => event,
                    Err(_)
                        if !std::path::Path::new(&format!("/proc/{}/task/{tid}", self.pid))
                            .exists() =>
                    {
                        continue;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to open CPU sampler for thread {tid}")
                        });
                    }
                };
                self.events.insert(tid, event);
            }
        }
        self.rebuild_poll_fds();
        Ok(())
    }

    pub fn drain_into(&mut self, batch: &mut PerfBatch) {
        reset_batch(batch);
        self.drain_retired_into(batch);
        for event in self.events.values_mut() {
            event.drain(batch);
        }
    }

    pub fn wait_and_drain_into(
        &mut self,
        batch: &mut PerfBatch,
        timeout: std::time::Duration,
    ) -> Result<()> {
        reset_batch(batch);
        self.drain_retired_into(batch);
        if self.poll_fds.is_empty() {
            return Ok(());
        }
        let timeout_millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: poll_fds points to initialized pollfd values and remains valid for the call.
        let ready = unsafe {
            libc::poll(
                self.poll_fds.as_mut_ptr(),
                self.poll_fds.len() as libc::nfds_t,
                timeout_millis,
            )
        };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(error).context("failed to wait for perf samples");
        }
        if ready == 0 {
            return Ok(());
        }
        for (poll_fd, tid) in self.poll_fds.iter().zip(&self.poll_tids) {
            if poll_fd.revents != 0
                && let Some(event) = self.events.get_mut(tid)
            {
                event.drain(batch);
            }
        }
        Ok(())
    }

    fn drain_retired_into(&mut self, batch: &mut PerfBatch) {
        for mut event in self.retired_events.drain(..) {
            event.drain(batch);
        }
    }

    fn rebuild_poll_fds(&mut self) {
        self.poll_fds.clear();
        self.poll_tids.clear();
        self.poll_fds.reserve(self.events.len());
        self.poll_tids.reserve(self.events.len());
        for (tid, event) in &self.events {
            self.poll_fds.push(libc::pollfd {
                fd: event.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            self.poll_tids.push(*tid);
        }
    }
}

fn reset_batch(batch: &mut PerfBatch) {
    batch.samples.clear();
    batch.lost_samples = 0;
    batch.malformed_records = 0;
}

struct PerfEvent {
    file: File,
    ring: MmapMut,
    mode: UnwindMode,
    register_mask: u64,
}

impl PerfEvent {
    fn open(tid: i32, mode: UnwindMode, frequency: u32) -> Result<Self> {
        let (sample_type, register_mask) = sample_configuration(mode);
        let mut attribute = perf::perf_event_attr {
            size: std::mem::size_of::<perf::perf_event_attr>() as u32,
            type_: perf::PERF_TYPE_SOFTWARE,
            config: perf::PERF_COUNT_SW_CPU_CLOCK as u64,
            ..Default::default()
        };
        attribute.__bindgen_anon_1.sample_freq = u64::from(frequency);
        attribute.sample_type = sample_type;
        attribute.sample_regs_user = register_mask;
        attribute.sample_stack_user = if mode == UnwindMode::Dwarf {
            DEFAULT_STACK_BYTES as u32
        } else {
            0
        };
        attribute.sample_max_stack = DEFAULT_MAX_FRAMES as u16;
        attribute.__bindgen_anon_2.wakeup_events = 1;
        attribute.set_disabled(1);
        attribute.set_freq(1);
        attribute.set_exclude_kernel(1);
        attribute.set_exclude_hv(1);
        attribute.set_exclude_callchain_kernel(1);

        // SAFETY: attribute is zero-initialized and populated according to perf_event_open(2).
        let fd = unsafe {
            perf_event_open(
                &mut attribute,
                tid,
                -1,
                -1,
                perf::PERF_FLAG_FD_CLOEXEC as libc::c_ulong,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("perf_event_open failed");
        }
        // SAFETY: perf_event_open returned a new owned file descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let page_size = page_size()?;
        // SAFETY: perf event ring buffers are shared mappings whose size is one metadata
        // page followed by a power-of-two number of data pages.
        let ring = unsafe {
            MmapOptions::new()
                .len(page_size * (DATA_PAGES + 1))
                .map_mut(&file)
        }
        .context("failed to mmap perf ring buffer")?;
        // SAFETY: file is an open perf event descriptor and the ioctl argument is unused.
        let enabled = unsafe { ioctls::ENABLE(file.as_raw_fd(), 0) };
        if enabled != 0 {
            return Err(std::io::Error::last_os_error()).context("failed to enable perf event");
        }
        Ok(Self {
            file,
            ring,
            mode,
            register_mask,
        })
    }

    fn drain(&mut self, batch: &mut PerfBatch) {
        let metadata = self.ring.as_mut_ptr().cast::<perf::perf_event_mmap_page>();
        // SAFETY: the first mmap page is a kernel-maintained perf_event_mmap_page.
        let (head, mut tail, data_offset, data_size) = unsafe {
            let head = std::ptr::read_volatile(std::ptr::addr_of!((*metadata).data_head));
            fence(Ordering::Acquire);
            let tail = std::ptr::read_volatile(std::ptr::addr_of!((*metadata).data_tail));
            let offset = std::ptr::read_volatile(std::ptr::addr_of!((*metadata).data_offset));
            let size = std::ptr::read_volatile(std::ptr::addr_of!((*metadata).data_size));
            (head, tail, offset, size)
        };
        if data_size == 0 || !data_size.is_power_of_two() {
            batch.malformed_records += 1;
            return;
        }

        while tail < head {
            let mut header = [0_u8; 8];
            if !self.copy_from_ring_into(tail, &mut header, data_offset, data_size) {
                batch.malformed_records += 1;
                tail = head;
                break;
            }
            let kind = u32::from_ne_bytes(header[0..4].try_into().expect("four bytes"));
            let size = u16::from_ne_bytes(header[6..8].try_into().expect("two bytes")) as usize;
            if size < 8 || size as u64 > data_size || tail + size as u64 > head {
                batch.malformed_records += 1;
                tail = head;
                break;
            }
            let Some(record) = self.record_from_ring(tail, size, data_offset, data_size) else {
                batch.malformed_records += 1;
                tail = head;
                break;
            };
            match kind {
                kind if kind == perf::PERF_RECORD_SAMPLE => {
                    let mut stack_buffer = if self.mode == UnwindMode::Dwarf {
                        batch.stack_buffers.pop().unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    match parse_sample(
                        &record[8..],
                        self.mode,
                        self.register_mask,
                        &mut stack_buffer,
                    ) {
                        Some(sample) => batch.samples.push(sample),
                        None => batch.malformed_records += 1,
                    }
                    if stack_buffer.capacity() != 0 {
                        batch.recycle_stack_buffer(stack_buffer);
                    }
                }
                kind if kind == perf::PERF_RECORD_LOST => {
                    if let Some(lost) = read_u64(record.as_ref(), 16) {
                        batch.lost_samples = batch.lost_samples.saturating_add(lost);
                    } else {
                        batch.malformed_records += 1;
                    }
                }
                kind if kind == perf::PERF_RECORD_LOST_SAMPLES => {
                    if let Some(lost) = read_u64(record.as_ref(), 8) {
                        batch.lost_samples = batch.lost_samples.saturating_add(lost);
                    } else {
                        batch.malformed_records += 1;
                    }
                }
                _ => {}
            }
            tail += size as u64;
        }

        fence(Ordering::Release);
        // SAFETY: metadata points to the writable perf metadata page for this mapping.
        unsafe {
            std::ptr::write_volatile(std::ptr::addr_of_mut!((*metadata).data_tail), tail);
        }
    }

    fn copy_from_ring_into(
        &self,
        absolute_offset: u64,
        output: &mut [u8],
        data_offset: u64,
        data_size: u64,
    ) -> bool {
        let Some(ring_start) = usize::try_from(data_offset).ok() else {
            return false;
        };
        let Some(ring_size) = usize::try_from(data_size).ok() else {
            return false;
        };
        let Some(start) = usize::try_from(absolute_offset & (data_size - 1)).ok() else {
            return false;
        };
        let Some(ring_end) = ring_start.checked_add(ring_size) else {
            return false;
        };
        let Some(ring) = self.ring.get(ring_start..ring_end) else {
            return false;
        };
        if output.len() > ring_size {
            return false;
        }
        let output_len = output.len();
        let first = output_len.min(ring_size - start);
        output[..first].copy_from_slice(&ring[start..start + first]);
        if first != output_len {
            output[first..].copy_from_slice(&ring[..output_len - first]);
        }
        true
    }

    fn record_from_ring(
        &self,
        absolute_offset: u64,
        length: usize,
        data_offset: u64,
        data_size: u64,
    ) -> Option<Cow<'_, [u8]>> {
        let ring_start = usize::try_from(data_offset).ok()?;
        let ring_size = usize::try_from(data_size).ok()?;
        let start = usize::try_from(absolute_offset & (data_size - 1)).ok()?;
        let ring_end = ring_start.checked_add(ring_size)?;
        let ring = self.ring.get(ring_start..ring_end)?;
        if length > ring_size {
            return None;
        }
        if length <= ring_size - start {
            return Some(Cow::Borrowed(&ring[start..start + length]));
        }
        let first = ring_size - start;
        let mut bytes = Vec::with_capacity(length);
        bytes.extend_from_slice(&ring[start..]);
        bytes.extend_from_slice(&ring[..length - first]);
        Some(Cow::Owned(bytes))
    }
}

impl Drop for PerfEvent {
    fn drop(&mut self) {
        // SAFETY: file remains an open perf event descriptor for the lifetime of PerfEvent.
        unsafe {
            ioctls::DISABLE(self.file.as_raw_fd(), 0);
        }
    }
}

fn sample_configuration(mode: UnwindMode) -> (u64, u64) {
    let common = u64::from(
        perf::PERF_SAMPLE_IP
            | perf::PERF_SAMPLE_TID
            | perf::PERF_SAMPLE_TIME
            | perf::PERF_SAMPLE_PERIOD,
    );
    match mode {
        UnwindMode::Fp => (common | u64::from(perf::PERF_SAMPLE_CALLCHAIN), 0),
        UnwindMode::Dwarf => (
            common
                | u64::from(perf::PERF_SAMPLE_REGS_USER)
                | u64::from(perf::PERF_SAMPLE_STACK_USER),
            native_register_mask(),
        ),
        UnwindMode::Auto => unreachable!("auto mode is resolved before opening perf events"),
    }
}

#[cfg(target_arch = "x86_64")]
fn native_register_mask() -> u64 {
    (1 << 6) | (1 << 7) | (1 << 8)
}

#[cfg(target_arch = "aarch64")]
fn native_register_mask() -> u64 {
    (1 << 29) | (1 << 30) | (1 << 31) | (1 << 32)
}

fn parse_sample(
    bytes: &[u8],
    mode: UnwindMode,
    register_mask: u64,
    stack_buffer: &mut Vec<u8>,
) -> Option<PerfSample> {
    let mut cursor = Cursor::new(bytes);
    let ip = cursor.u64()?;
    let _pid = cursor.u32()?;
    let tid = cursor.u32()?;
    let time = cursor.u64()?;
    let period = cursor.u64()?;
    let data = match mode {
        UnwindMode::Fp => {
            let count = usize::try_from(cursor.u64()?).ok()?.min(DEFAULT_MAX_FRAMES);
            let mut frames = Vec::with_capacity(count);
            for _ in 0..count {
                let address = cursor.u64()?;
                if address < perf::PERF_CONTEXT_MAX {
                    frames.push(address);
                }
            }
            if frames.first().copied() != Some(ip) {
                frames.insert(0, ip);
            }
            frames.truncate(DEFAULT_MAX_FRAMES);
            PerfSampleData::FramePointer(frames)
        }
        UnwindMode::Dwarf => {
            let abi = cursor.u64()?;
            if abi == perf::PERF_SAMPLE_REGS_ABI_NONE as u64 {
                return None;
            }
            let registers = decode_registers(&mut cursor, ip, register_mask)?;
            let requested_size = usize::try_from(cursor.u64()?).ok()?;
            let stack = cursor.bytes(requested_size)?;
            let dynamic_size = usize::try_from(cursor.u64()?).ok()?.min(stack.len());
            stack_buffer.clear();
            stack_buffer.extend_from_slice(&stack[..dynamic_size]);
            PerfSampleData::Dwarf(StackSnapshot {
                registers,
                bytes: std::mem::take(stack_buffer),
            })
        }
        UnwindMode::Auto => return None,
    };
    Some(PerfSample {
        tid,
        time,
        period,
        data,
    })
}

#[cfg(target_arch = "x86_64")]
fn decode_registers(cursor: &mut Cursor<'_>, ip: u64, register_mask: u64) -> Option<RawRegisters> {
    let mut sampled_ip = ip;
    let mut sp = None;
    let mut fp = None;
    for bit in 0..64 {
        if register_mask & (1_u64 << bit) == 0 {
            continue;
        }
        let value = cursor.u64()?;
        match bit {
            6 => fp = Some(value),
            7 => sp = Some(value),
            8 => sampled_ip = value,
            _ => {}
        }
    }
    Some(RawRegisters {
        ip: sampled_ip,
        sp: sp?,
        fp: fp?,
        lr: 0,
    })
}

#[cfg(target_arch = "aarch64")]
fn decode_registers(cursor: &mut Cursor<'_>, ip: u64, register_mask: u64) -> Option<RawRegisters> {
    let mut sampled_ip = ip;
    let mut sp = None;
    let mut fp = None;
    let mut lr = None;
    for bit in 0..64 {
        if register_mask & (1_u64 << bit) == 0 {
            continue;
        }
        let value = cursor.u64()?;
        match bit {
            29 => fp = Some(value),
            30 => lr = Some(value),
            31 => sp = Some(value),
            32 => sampled_ip = value,
            _ => {}
        }
    }
    Some(RawRegisters {
        ip: sampled_ip,
        sp: sp?,
        fp: fp?,
        lr: lr?,
    })
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn u32(&mut self) -> Option<u32> {
        let value = u32::from_ne_bytes(
            self.bytes(self.offset.checked_add(4)? - self.offset)?
                .try_into()
                .ok()?,
        );
        Some(value)
    }

    fn u64(&mut self) -> Option<u64> {
        let value = u64::from_ne_bytes(
            self.bytes(self.offset.checked_add(8)? - self.offset)?
                .try_into()
                .ok()?,
        );
        Some(value)
    }

    fn bytes(&mut self, length: usize) -> Option<&'a [u8]> {
        let end = self.offset.checked_add(length)?;
        let value = self.bytes.get(self.offset..end)?;
        self.offset = end;
        Some(value)
    }
}

fn page_size() -> Result<usize> {
    // SAFETY: sysconf with _SC_PAGESIZE has no pointer arguments.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size <= 0 {
        bail!("failed to determine system page size");
    }
    usize::try_from(size).context("page size does not fit usize")
}

#[cfg(test)]
mod tests {
    use std::{ptr, time::Duration};

    use super::*;

    #[test]
    fn drains_lost_record_from_retired_event_without_active_poll_fds() {
        let page_size = page_size().unwrap();
        let backing = tempfile::tempfile().unwrap();
        backing.set_len((page_size * 2) as u64).unwrap();
        // SAFETY: `backing` is a valid two-page file, and the requested mapping is within it.
        let mut ring = unsafe {
            MmapOptions::new()
                .len(page_size * 2)
                .map_mut(&backing)
                .unwrap()
        };
        // The retired event is dropped after draining, so keep a second mapping to verify its
        // metadata update after `wait_and_drain_into` returns.
        // SAFETY: `backing` remains a valid two-page file for this read-only mapping.
        let observer = unsafe { MmapOptions::new().len(page_size * 2).map(&backing).unwrap() };
        let metadata = ring.as_mut_ptr().cast::<perf::perf_event_mmap_page>();
        let observer_metadata = observer.as_ptr().cast::<perf::perf_event_mmap_page>();
        let record_size = 24_u16;

        // SAFETY: `ring` is page-aligned and its first page is large enough for the metadata
        // structure; the pointer remains valid while the mapping is owned by the event.
        unsafe {
            ptr::write(
                metadata,
                perf::perf_event_mmap_page {
                    data_head: u64::from(record_size),
                    data_offset: page_size as u64,
                    data_size: page_size as u64,
                    ..Default::default()
                },
            );
        }
        let record = &mut ring[page_size..page_size + usize::from(record_size)];
        record[0..4].copy_from_slice(&(perf::PERF_RECORD_LOST as u32).to_ne_bytes());
        record[6..8].copy_from_slice(&record_size.to_ne_bytes());
        record[8..16].copy_from_slice(&17_u64.to_ne_bytes());
        record[16..24].copy_from_slice(&9_u64.to_ne_bytes());

        let mut collector = PerfCollector {
            pid: 0,
            mode: UnwindMode::Fp,
            frequency: 0,
            max_threads: 0,
            events: HashMap::new(),
            retired_events: vec![PerfEvent {
                file: tempfile::tempfile().unwrap(),
                ring,
                mode: UnwindMode::Fp,
                register_mask: 0,
            }],
            poll_fds: Vec::new(),
            poll_tids: Vec::new(),
        };
        let mut batch = PerfBatch::default();

        collector
            .wait_and_drain_into(&mut batch, Duration::ZERO)
            .unwrap();

        assert_eq!(batch.lost_samples, 9);
        assert!(collector.retired_events.is_empty());
        // SAFETY: the observer mapping is live, page-aligned, and contains the metadata page.
        let data_tail =
            unsafe { ptr::read_volatile(ptr::addr_of!((*observer_metadata).data_tail)) };
        assert_eq!(data_tail, u64::from(record_size));
    }
}
