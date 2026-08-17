use std::{collections::HashSet, sync::Arc};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Frame {
    pub address: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Stack(pub Arc<[Frame]>);

impl From<Vec<Frame>> for Stack {
    fn from(frames: Vec<Frame>) -> Self {
        Self(frames.into())
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AttributedStack {
    pub stack: Stack,
    pub pid: u32,
    pub tid: u32,
    pub thread_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TimedStackSample {
    pub stack: Stack,
    pub pid: u32,
    pub tid: u32,
    pub thread_name: Option<String>,
    pub timestamp: u64,
    pub cpu_delta: u64,
}

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

#[derive(Clone, Copy, Debug, Default)]
pub struct OffCpuValues {
    pub events: i64,
    pub nanoseconds: i64,
}

pub fn has_address_cycle(frames: &[u64], seen: &mut HashSet<u64>) -> bool {
    seen.clear();
    !frames.iter().all(|address| seen.insert(*address))
}
