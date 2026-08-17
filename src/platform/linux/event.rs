use std::collections::{BTreeMap, VecDeque};

use super::perf::PerfSample;

pub struct PerfEventSorter {
    pending: BTreeMap<u64, VecDeque<PerfSample>>,
    pending_len: usize,
    max_pending: usize,
    reorder_nanos: u64,
    max_seen: u64,
    peak_pending: usize,
    forced_flushes: u64,
}

impl PerfEventSorter {
    pub fn new(reorder_nanos: u64, max_pending: usize) -> Self {
        Self {
            pending: BTreeMap::new(),
            pending_len: 0,
            max_pending,
            reorder_nanos,
            max_seen: 0,
            peak_pending: 0,
            forced_flushes: 0,
        }
    }

    pub fn push(&mut self, sample: PerfSample, ready: &mut Vec<PerfSample>) {
        self.max_seen = self.max_seen.max(sample.time);
        self.pending
            .entry(sample.time)
            .or_default()
            .push_back(sample);
        self.pending_len += 1;
        self.peak_pending = self.peak_pending.max(self.pending_len);
        self.drain_before(self.max_seen.saturating_sub(self.reorder_nanos), ready);
        while self.pending_len > self.max_pending {
            if let Some(sample) = self.pop_oldest() {
                ready.push(sample);
                self.forced_flushes = self.forced_flushes.saturating_add(1);
            } else {
                break;
            }
        }
    }

    pub fn flush(&mut self, ready: &mut Vec<PerfSample>) {
        while let Some(sample) = self.pop_oldest() {
            ready.push(sample);
        }
    }

    pub fn take_stats(&mut self) -> (usize, u64) {
        let peak = std::mem::take(&mut self.peak_pending);
        let forced = std::mem::take(&mut self.forced_flushes);
        (peak, forced)
    }

    fn drain_before(&mut self, watermark: u64, ready: &mut Vec<PerfSample>) {
        loop {
            let Some((&timestamp, _)) = self.pending.first_key_value() else {
                break;
            };
            if timestamp > watermark {
                break;
            }
            if let Some(sample) = self.pop_oldest() {
                ready.push(sample);
            }
        }
    }

    fn pop_oldest(&mut self) -> Option<PerfSample> {
        let timestamp = *self.pending.first_key_value()?.0;
        let queue = self.pending.get_mut(&timestamp)?;
        let sample = queue.pop_front();
        if queue.is_empty() {
            self.pending.remove(&timestamp);
        }
        if sample.is_some() {
            self.pending_len -= 1;
        }
        sample
    }
}

#[cfg(test)]
mod tests {
    use super::PerfEventSorter;
    use crate::platform::linux::perf::{PerfSample, PerfSampleData};

    fn sample(time: u64, marker: u64) -> PerfSample {
        PerfSample {
            pid: 1,
            tid: 1,
            time,
            period: 1,
            data: PerfSampleData::FramePointer(vec![marker]),
        }
    }

    fn marker(sample: &PerfSample) -> u64 {
        match &sample.data {
            PerfSampleData::FramePointer(frames) => frames[0],
            PerfSampleData::Dwarf(_) => panic!("test sample should use frame pointers"),
        }
    }

    #[test]
    fn equal_timestamps_preserve_insertion_order() {
        let mut sorter = PerfEventSorter::new(100, 16);
        let mut ready = Vec::new();
        sorter.push(sample(10, 1), &mut ready);
        sorter.push(sample(10, 2), &mut ready);
        sorter.flush(&mut ready);

        assert_eq!(ready.iter().map(marker).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn watermark_emits_timestamp_ordered_samples() {
        let mut sorter = PerfEventSorter::new(10, 16);
        let mut ready = Vec::new();
        sorter.push(sample(100, 100), &mut ready);
        sorter.push(sample(95, 95), &mut ready);
        sorter.push(sample(110, 110), &mut ready);

        assert_eq!(ready.iter().map(marker).collect::<Vec<_>>(), [95, 100]);
        sorter.flush(&mut ready);
        assert_eq!(ready.iter().map(marker).collect::<Vec<_>>(), [95, 100, 110]);
    }

    #[test]
    fn pending_limit_forces_oldest_sample_out_and_reports_flush() {
        let mut sorter = PerfEventSorter::new(1_000, 2);
        let mut ready = Vec::new();
        sorter.push(sample(30, 30), &mut ready);
        sorter.push(sample(10, 10), &mut ready);
        sorter.push(sample(20, 20), &mut ready);

        assert_eq!(ready.iter().map(marker).collect::<Vec<_>>(), [10]);
        assert_eq!(sorter.take_stats(), (3, 1));
        sorter.flush(&mut ready);
        assert_eq!(ready.iter().map(marker).collect::<Vec<_>>(), [10, 20, 30]);
    }
}
