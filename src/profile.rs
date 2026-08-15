use std::collections::HashSet;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Frame {
    pub address: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Stack(pub Vec<Frame>);

#[derive(Clone, Copy, Debug, Default)]
pub struct CpuValues {
    pub samples: i64,
    pub nanoseconds: i64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HeapValues {
    pub alloc_objects: i64,
    pub alloc_space: i64,
    pub inuse_objects: i64,
    pub inuse_space: i64,
}

pub fn has_address_cycle(frames: &[u64], seen: &mut HashSet<u64>) -> bool {
    seen.clear();
    !frames.iter().all(|address| seen.insert(*address))
}
