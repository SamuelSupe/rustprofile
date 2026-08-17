use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::profile::{Frame, HeapValues, Stack};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeapEventKind {
    Alloc,
    Free,
}

#[derive(Clone, Debug)]
pub struct HeapEvent {
    pub kind: HeapEventKind,
    pub pointer: u64,
    pub size: u64,
    pub weight: u64,
    pub frames: Vec<u64>,
}

#[derive(Clone, Debug)]
struct LiveAllocation {
    weight: u64,
    weighted_space: u64,
    stack: Arc<Stack>,
}

#[derive(Clone, Copy, Debug, Default)]
struct LiveTotals {
    objects: u128,
    space: u128,
}

pub struct HeapState {
    live: HashMap<u64, LiveAllocation>,
    window_allocations: HashMap<Arc<Stack>, HeapValues>,
    live_totals: HashMap<Arc<Stack>, LiveTotals>,
    max_stacks: usize,
    aggregation_drops: HeapAggregationDrops,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HeapAggregationDrops {
    pub alloc_objects: i64,
    pub alloc_space: i64,
    pub inuse_objects: i64,
    pub inuse_space: i64,
}

impl HeapState {
    pub fn with_max_stacks(max_stacks: usize) -> Self {
        Self {
            max_stacks,
            ..Default::default()
        }
    }

    pub fn apply(&mut self, event: HeapEvent) {
        match event.kind {
            HeapEventKind::Alloc => self.allocate(event),
            HeapEventKind::Free => {
                if let Some(allocation) = self.live.remove(&event.pointer) {
                    self.remove_live_totals(&allocation);
                }
            }
        }
    }

    pub fn snapshot_window(&mut self) -> HashMap<Stack, HeapValues> {
        self.snapshot_window_with_drops().0
    }

    pub fn snapshot_window_with_drops(
        &mut self,
    ) -> (HashMap<Stack, HeapValues>, HeapAggregationDrops) {
        let mut values = std::mem::take(&mut self.window_allocations);
        let mut drops = std::mem::take(&mut self.aggregation_drops);
        for (stack, totals) in &self.live_totals {
            let inuse_objects = saturating_u128_i64(totals.objects);
            let inuse_space = saturating_u128_i64(totals.space);
            if let Some(entry) = values.get_mut(stack) {
                entry.inuse_objects = inuse_objects;
                entry.inuse_space = inuse_space;
            } else if values.len() < self.max_stacks {
                values.insert(
                    Arc::clone(stack),
                    HeapValues {
                        inuse_objects,
                        inuse_space,
                        ..Default::default()
                    },
                );
            } else {
                drops.inuse_objects = drops.inuse_objects.saturating_add(inuse_objects);
                drops.inuse_space = drops.inuse_space.saturating_add(inuse_space);
            }
        }
        (
            values
                .into_iter()
                .map(|(stack, values)| ((*stack).clone(), values))
                .collect(),
            drops,
        )
    }

    pub fn clear_live(&mut self) {
        self.live.clear();
        self.live_totals.clear();
    }

    pub fn live_sample_count(&self) -> usize {
        self.live.len()
    }

    pub fn retain_live_pointers(&mut self, pointers: &HashSet<u64>) {
        self.live.retain(|pointer, _| pointers.contains(pointer));
        self.rebuild_live_totals();
    }

    fn allocate(&mut self, event: HeapEvent) {
        if event.pointer == 0 || event.size == 0 || event.weight == 0 {
            return;
        }
        let stack = self.intern_stack(Stack::from(
            event
                .frames
                .into_iter()
                .map(|address| Frame { address })
                .collect::<Vec<_>>(),
        ));
        let alloc_objects = saturating_i64(event.weight);
        let weighted_space = event.size.saturating_mul(event.weight);
        let alloc_space = saturating_i64(weighted_space);
        if let Some(values) = self.window_allocations.get_mut(&stack) {
            values.alloc_objects = values.alloc_objects.saturating_add(alloc_objects);
            values.alloc_space = values.alloc_space.saturating_add(alloc_space);
        } else if self.window_allocations.len() < self.max_stacks {
            self.window_allocations.insert(
                Arc::clone(&stack),
                HeapValues {
                    alloc_objects,
                    alloc_space,
                    ..Default::default()
                },
            );
        } else {
            self.aggregation_drops.alloc_objects = self
                .aggregation_drops
                .alloc_objects
                .saturating_add(alloc_objects);
            self.aggregation_drops.alloc_space = self
                .aggregation_drops
                .alloc_space
                .saturating_add(alloc_space);
        }
        let previous = self.live.insert(
            event.pointer,
            LiveAllocation {
                weight: event.weight,
                weighted_space,
                stack: Arc::clone(&stack),
            },
        );
        if let Some(previous) = previous {
            self.remove_live_totals(&previous);
        }
        let totals = self.live_totals.entry(stack).or_default();
        totals.objects = totals.objects.saturating_add(u128::from(event.weight));
        totals.space = totals.space.saturating_add(u128::from(weighted_space));
    }

    fn intern_stack(&self, stack: Stack) -> Arc<Stack> {
        if let Some((existing, _)) = self.live_totals.get_key_value(&stack) {
            return Arc::clone(existing);
        }
        if let Some((existing, _)) = self.window_allocations.get_key_value(&stack) {
            return Arc::clone(existing);
        }
        Arc::new(stack)
    }

    fn remove_live_totals(&mut self, allocation: &LiveAllocation) {
        let remove = if let Some(totals) = self.live_totals.get_mut(allocation.stack.as_ref()) {
            totals.objects = totals.objects.saturating_sub(u128::from(allocation.weight));
            totals.space = totals
                .space
                .saturating_sub(u128::from(allocation.weighted_space));
            totals.objects == 0 && totals.space == 0
        } else {
            false
        };
        if remove {
            self.live_totals.remove(allocation.stack.as_ref());
        }
    }

    fn rebuild_live_totals(&mut self) {
        self.live_totals.clear();
        for allocation in self.live.values() {
            let totals = self
                .live_totals
                .entry(Arc::clone(&allocation.stack))
                .or_default();
            totals.objects = totals.objects.saturating_add(u128::from(allocation.weight));
            totals.space = totals
                .space
                .saturating_add(u128::from(allocation.weighted_space));
        }
    }
}

impl Default for HeapState {
    fn default() -> Self {
        Self {
            live: HashMap::new(),
            window_allocations: HashMap::new(),
            live_totals: HashMap::new(),
            max_stacks: usize::MAX,
            aggregation_drops: HeapAggregationDrops::default(),
        }
    }
}

fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn saturating_u128_i64(value: u128) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
