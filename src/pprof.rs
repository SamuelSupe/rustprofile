use std::{
    collections::HashMap,
    fs,
    io::{self, Write},
    path::Path,
};

use anyhow::{Context, Result};
use flate2::{Compression, write::GzEncoder};
use prost::Message;
use serde::Serialize;

use crate::{
    profile::{CpuValues, HeapValues, Stack},
    symbol::{MappingInfo, ResolvedLocation, SymbolizedLine, Symbolizer},
};

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Profile {
    #[prost(message, repeated, tag = "1")]
    pub sample_type: Vec<ValueType>,
    #[prost(message, repeated, tag = "2")]
    pub sample: Vec<Sample>,
    #[prost(message, repeated, tag = "3")]
    pub mapping: Vec<Mapping>,
    #[prost(message, repeated, tag = "4")]
    pub location: Vec<Location>,
    #[prost(message, repeated, tag = "5")]
    pub function: Vec<Function>,
    #[prost(string, repeated, tag = "6")]
    pub string_table: Vec<String>,
    #[prost(int64, tag = "7")]
    pub drop_frames: i64,
    #[prost(int64, tag = "8")]
    pub keep_frames: i64,
    #[prost(int64, tag = "9")]
    pub time_nanos: i64,
    #[prost(int64, tag = "10")]
    pub duration_nanos: i64,
    #[prost(message, optional, tag = "11")]
    pub period_type: Option<ValueType>,
    #[prost(int64, tag = "12")]
    pub period: i64,
    #[prost(int64, repeated, tag = "13")]
    pub comment: Vec<i64>,
    #[prost(int64, tag = "14")]
    pub default_sample_type: i64,
    #[prost(int64, tag = "15")]
    pub doc_url: i64,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ValueType {
    #[prost(int64, tag = "1")]
    pub r#type: i64,
    #[prost(int64, tag = "2")]
    pub unit: i64,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Sample {
    #[prost(uint64, repeated, packed = "true", tag = "1")]
    pub location_id: Vec<u64>,
    #[prost(int64, repeated, packed = "true", tag = "2")]
    pub value: Vec<i64>,
    #[prost(message, repeated, tag = "3")]
    pub label: Vec<Label>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Label {
    #[prost(int64, tag = "1")]
    pub key: i64,
    #[prost(int64, tag = "2")]
    pub str: i64,
    #[prost(int64, tag = "3")]
    pub num: i64,
    #[prost(int64, tag = "4")]
    pub num_unit: i64,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Mapping {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(uint64, tag = "2")]
    pub memory_start: u64,
    #[prost(uint64, tag = "3")]
    pub memory_limit: u64,
    #[prost(uint64, tag = "4")]
    pub file_offset: u64,
    #[prost(int64, tag = "5")]
    pub filename: i64,
    #[prost(int64, tag = "6")]
    pub build_id: i64,
    #[prost(bool, tag = "7")]
    pub has_functions: bool,
    #[prost(bool, tag = "8")]
    pub has_filenames: bool,
    #[prost(bool, tag = "9")]
    pub has_line_numbers: bool,
    #[prost(bool, tag = "10")]
    pub has_inline_frames: bool,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Location {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(uint64, tag = "2")]
    pub mapping_id: u64,
    #[prost(uint64, tag = "3")]
    pub address: u64,
    #[prost(message, repeated, tag = "4")]
    pub line: Vec<Line>,
    #[prost(bool, tag = "5")]
    pub is_folded: bool,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Line {
    #[prost(uint64, tag = "1")]
    pub function_id: u64,
    #[prost(int64, tag = "2")]
    pub line: i64,
    #[prost(int64, tag = "3")]
    pub column: i64,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Function {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(int64, tag = "2")]
    pub name: i64,
    #[prost(int64, tag = "3")]
    pub system_name: i64,
    #[prost(int64, tag = "4")]
    pub filename: i64,
    #[prost(int64, tag = "5")]
    pub start_line: i64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ExportStats {
    pub symbolized_locations: u64,
    pub total_locations: u64,
}

pub fn write_cpu_profile(
    path: &Path,
    samples: &HashMap<Stack, CpuValues>,
    symbolizer: &mut Symbolizer,
    time_nanos: i64,
    duration_nanos: i64,
    frequency: u32,
    labels: &[(String, String)],
) -> Result<(ExportStats, Profile)> {
    let mut builder = ProfileBuilder::new(symbolizer, time_nanos, duration_nanos);
    let samples_type = builder.value_type("samples", "count");
    let cpu_type = builder.value_type("cpu", "nanoseconds");
    builder.profile.sample_type = vec![samples_type, cpu_type.clone()];
    builder.profile.period_type = Some(cpu_type);
    builder.profile.period = i64::from(1_000_000_000_u32 / frequency.max(1));
    builder.profile.default_sample_type = builder.intern("cpu");
    let labels = builder.sample_labels(labels);

    for (stack, values) in samples {
        builder.add_sample(stack, vec![values.samples, values.nanoseconds], &labels);
    }
    let stats = builder.stats();
    let profile = builder.finish();
    write_profile_atomic(path, &profile)?;
    Ok((stats, profile))
}

pub fn write_heap_profile(
    path: &Path,
    samples: &HashMap<Stack, HeapValues>,
    symbolizer: &mut Symbolizer,
    time_nanos: i64,
    duration_nanos: i64,
    allocation_interval: u64,
    labels: &[(String, String)],
) -> Result<(ExportStats, Profile)> {
    let mut builder = ProfileBuilder::new(symbolizer, time_nanos, duration_nanos);
    builder.profile.sample_type = [
        ("alloc_objects", "count"),
        ("alloc_space", "bytes"),
        ("inuse_objects", "count"),
        ("inuse_space", "bytes"),
    ]
    .into_iter()
    .map(|(kind, unit)| builder.value_type(kind, unit))
    .collect();
    builder.profile.period_type = Some(builder.value_type("space", "bytes"));
    builder.profile.period = i64::try_from(allocation_interval).unwrap_or(i64::MAX);
    builder.profile.default_sample_type = builder.intern("inuse_space");
    let comment = builder.intern(
        "inuse values include only sampled allocations observed since rustprofile attached",
    );
    builder.profile.comment.push(comment);
    let labels = builder.sample_labels(labels);

    for (stack, values) in samples {
        builder.add_sample(
            stack,
            vec![
                values.alloc_objects,
                values.alloc_space,
                values.inuse_objects,
                values.inuse_space,
            ],
            &labels,
        );
    }
    let stats = builder.stats();
    let profile = builder.finish();
    write_profile_atomic(path, &profile)?;
    Ok((stats, profile))
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    atomic_write(path, |file| {
        serde_json::to_writer_pretty(&mut *file, value)?;
        file.write_all(b"\n")?;
        Ok(())
    })
}

fn write_profile_atomic(path: &Path, profile: &Profile) -> Result<()> {
    atomic_write(path, |file| {
        let bytes = profile.encode_to_vec();
        let mut encoder = GzEncoder::new(file, Compression::default());
        encoder.write_all(&bytes)?;
        encoder.finish()?;
        Ok(())
    })
}

pub(crate) fn atomic_write<F>(path: &Path, write: F) -> Result<()>
where
    F: FnOnce(&mut fs::File) -> Result<()>,
{
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create output directory {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    write(temporary.as_file_mut())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to publish {}", path.display()))?;
    match sync_directory(parent) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
            ) => {}
        Err(error) => return Err(error).context("failed to sync output directory"),
    }
    Ok(())
}

pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

struct ProfileBuilder<'a> {
    profile: Profile,
    symbolizer: &'a mut Symbolizer,
    strings: HashMap<String, i64>,
    mappings: HashMap<MappingInfo, u64>,
    functions: HashMap<(String, String, Option<String>), u64>,
    locations: HashMap<u64, u64>,
    stats: ExportStats,
}

impl<'a> ProfileBuilder<'a> {
    fn new(symbolizer: &'a mut Symbolizer, time_nanos: i64, duration_nanos: i64) -> Self {
        let profile = Profile {
            sample_type: Vec::new(),
            sample: Vec::new(),
            mapping: Vec::new(),
            location: Vec::new(),
            function: Vec::new(),
            string_table: vec![String::new()],
            drop_frames: 0,
            keep_frames: 0,
            time_nanos,
            duration_nanos,
            period_type: None,
            period: 0,
            comment: Vec::new(),
            default_sample_type: 0,
            doc_url: 0,
        };
        let mut strings = HashMap::new();
        strings.insert(String::new(), 0);
        Self {
            profile,
            symbolizer,
            strings,
            mappings: HashMap::new(),
            functions: HashMap::new(),
            locations: HashMap::new(),
            stats: ExportStats::default(),
        }
    }

    fn intern(&mut self, value: impl AsRef<str>) -> i64 {
        let value = value.as_ref();
        if let Some(index) = self.strings.get(value) {
            return *index;
        }
        let index = self.profile.string_table.len() as i64;
        let value = value.to_owned();
        self.profile.string_table.push(value.clone());
        self.strings.insert(value, index);
        index
    }

    fn value_type(&mut self, kind: &str, unit: &str) -> ValueType {
        ValueType {
            r#type: self.intern(kind),
            unit: self.intern(unit),
        }
    }

    fn sample_labels(&mut self, labels: &[(String, String)]) -> Vec<Label> {
        labels
            .iter()
            .map(|(key, value)| Label {
                key: self.intern(key),
                str: self.intern(value),
                num: 0,
                num_unit: 0,
            })
            .collect()
    }

    fn add_sample(&mut self, stack: &Stack, values: Vec<i64>, labels: &[Label]) {
        let location_id = stack
            .0
            .iter()
            .map(|frame| self.location(frame.address))
            .collect();
        self.profile.sample.push(Sample {
            location_id,
            value: values,
            label: labels.to_vec(),
        });
    }

    fn location(&mut self, address: u64) -> u64 {
        if let Some(id) = self.locations.get(&address) {
            return *id;
        }
        let resolved = self.symbolizer.resolve(address);
        self.stats.total_locations += 1;
        if resolved.is_symbolized() {
            self.stats.symbolized_locations += 1;
        }
        let mapping_id = resolved
            .mapping
            .as_ref()
            .map(|mapping| self.mapping(mapping, &resolved))
            .unwrap_or_default();
        let line = resolved
            .lines
            .iter()
            .map(|line| Line {
                function_id: self.function(line),
                line: line.line,
                column: 0,
            })
            .collect();
        let id = self.profile.location.len() as u64 + 1;
        self.profile.location.push(Location {
            id,
            mapping_id,
            address,
            line,
            is_folded: false,
        });
        self.locations.insert(address, id);
        id
    }

    fn mapping(&mut self, info: &MappingInfo, resolved: &ResolvedLocation) -> u64 {
        let has_filenames = resolved.lines.iter().any(|line| line.filename.is_some());
        let has_line_numbers = resolved.lines.iter().any(|line| line.line != 0);
        if let Some(id) = self.mappings.get(info).copied() {
            if let Some(mapping) = self.profile.mapping.get_mut(id as usize - 1) {
                mapping.has_functions |= !resolved.lines.is_empty();
                mapping.has_filenames |= has_filenames;
                mapping.has_line_numbers |= has_line_numbers;
                mapping.has_inline_frames |= resolved.lines.len() > 1;
            }
            return id;
        }
        let id = self.profile.mapping.len() as u64 + 1;
        let filename = self.intern(info.filename.to_string_lossy());
        let build_id = self.intern(info.build_id.as_deref().unwrap_or_default());
        self.profile.mapping.push(Mapping {
            id,
            memory_start: info.start,
            memory_limit: info.limit,
            file_offset: info.offset,
            filename,
            build_id,
            has_functions: !resolved.lines.is_empty(),
            has_filenames,
            has_line_numbers,
            has_inline_frames: resolved.lines.len() > 1,
        });
        self.mappings.insert(info.clone(), id);
        id
    }

    fn function(&mut self, line: &SymbolizedLine) -> u64 {
        let key = (
            line.function.clone(),
            line.system_name.clone(),
            line.filename.clone(),
        );
        if let Some(id) = self.functions.get(&key) {
            return *id;
        }
        let id = self.profile.function.len() as u64 + 1;
        let name = self.intern(&line.function);
        let system_name = self.intern(&line.system_name);
        let filename = self.intern(line.filename.as_deref().unwrap_or_default());
        self.profile.function.push(Function {
            id,
            name,
            system_name,
            filename,
            start_line: 0,
        });
        self.functions.insert(key, id);
        id
    }

    fn stats(&self) -> ExportStats {
        self.stats
    }

    fn finish(self) -> Profile {
        self.profile
    }
}
