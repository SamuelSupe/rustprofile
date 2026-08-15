use std::collections::HashSet;

use rustprofile::heap::{HeapEvent, HeapEventKind, HeapState};

fn allocation(pointer: u64, size: u64, weight: u64) -> HeapEvent {
    HeapEvent {
        kind: HeapEventKind::Alloc,
        pointer,
        size,
        weight,
        frames: vec![0x1000, 0x2000],
    }
}

fn free(pointer: u64) -> HeapEvent {
    HeapEvent {
        kind: HeapEventKind::Free,
        pointer,
        size: 0,
        weight: 0,
        frames: Vec::new(),
    }
}

fn allocation_on_stack(pointer: u64, size: u64, weight: u64, frames: &[u64]) -> HeapEvent {
    HeapEvent {
        kind: HeapEventKind::Alloc,
        pointer,
        size,
        weight,
        frames: frames.to_vec(),
    }
}

#[test]
fn weighted_allocations_and_frees_reconcile_inuse_values_per_window() {
    let mut state = HeapState::default();
    state.apply(allocation(1, 100, 2));
    state.apply(allocation(2, 50, 3));

    let first = state.snapshot_window();
    assert_eq!(first.len(), 1, "same stack should be aggregated");
    let values = first.values().next().expect("aggregated stack value");
    assert_eq!(values.alloc_objects, 5);
    assert_eq!(values.alloc_space, 350);
    assert_eq!(values.inuse_objects, 5);
    assert_eq!(values.inuse_space, 350);
    assert_eq!(state.live_sample_count(), 2);

    state.apply(free(1));
    let second = state.snapshot_window();
    let values = second.values().next().expect("remaining live allocation");
    assert_eq!(
        values.alloc_objects, 0,
        "allocation counters reset per window"
    );
    assert_eq!(values.alloc_space, 0);
    assert_eq!(values.inuse_objects, 3);
    assert_eq!(values.inuse_space, 150);
    assert_eq!(state.live_sample_count(), 1);

    state.apply(free(2));
    assert!(state.snapshot_window().is_empty());
    assert_eq!(state.live_sample_count(), 0);
}

#[test]
fn replacement_and_retain_live_pointers_follow_kernel_pointer_identity() {
    let mut state = HeapState::default();
    state.apply(allocation(7, 64, 1));
    state.apply(allocation(7, 128, 4));

    let replaced = state.snapshot_window();
    let values = replaced.values().next().expect("replacement allocation");
    assert_eq!(values.alloc_objects, 5);
    assert_eq!(values.alloc_space, 576);
    assert_eq!(values.inuse_objects, 4);
    assert_eq!(values.inuse_space, 512);

    state.retain_live_pointers(&HashSet::from([99_u64]));
    assert_eq!(state.live_sample_count(), 0);
    assert!(state.snapshot_window().is_empty());
}

#[test]
fn max_stacks_drops_only_output_aggregation_not_live_totals() {
    let mut state = HeapState::with_max_stacks(1);
    state.apply(allocation_on_stack(1, 10, 1, &[0x1000]));
    state.apply(allocation_on_stack(2, 20, 2, &[0x2000]));

    let (snapshot, drops) = state.snapshot_window_with_drops();
    assert_eq!(snapshot.len(), 1);
    let retained = snapshot.values().next().expect("retained stack value");
    assert_eq!(retained.alloc_objects, 1);
    assert_eq!(retained.alloc_space, 10);
    assert_eq!(retained.inuse_objects, 1);
    assert_eq!(retained.inuse_space, 10);
    assert_eq!(drops.alloc_objects, 2);
    assert_eq!(drops.alloc_space, 40);
    assert_eq!(drops.inuse_objects, 2);
    assert_eq!(drops.inuse_space, 40);
    assert_eq!(state.live_sample_count(), 2);

    state.apply(free(2));
    assert_eq!(state.live_sample_count(), 1);
}

#[test]
fn invalid_and_overflowing_events_are_safe() {
    let mut state = HeapState::default();
    state.apply(allocation(0, 10, 1));
    state.apply(allocation(1, 0, 1));
    state.apply(allocation(2, u64::MAX, 2));

    let snapshot = state.snapshot_window();
    let values = snapshot.values().next().expect("valid allocation");
    assert_eq!(values.alloc_objects, 2);
    assert_eq!(values.alloc_space, i64::MAX);
    assert_eq!(values.inuse_objects, 2);
    assert_eq!(values.inuse_space, i64::MAX);
}
