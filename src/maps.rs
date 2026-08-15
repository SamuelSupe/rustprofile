use std::{collections::BTreeSet, fs, ops::Range, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MapEntry {
    pub start: u64,
    pub end: u64,
    pub offset: u64,
    pub permissions: String,
    pub device: String,
    pub inode: u64,
    pub path: Option<PathBuf>,
}

impl MapEntry {
    pub fn is_executable(&self) -> bool {
        self.permissions.as_bytes().get(2) == Some(&b'x')
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExecutableRanges(Vec<Range<u64>>);

impl ExecutableRanges {
    pub fn from_maps(maps: &[MapEntry]) -> Self {
        let mut ranges = maps
            .iter()
            .filter(|mapping| mapping.is_executable())
            .map(|mapping| mapping.start..mapping.end)
            .collect::<Vec<_>>();
        ranges.sort_unstable_by_key(|range| range.start);

        let mut merged = Vec::<Range<u64>>::with_capacity(ranges.len());
        for range in ranges {
            if let Some(previous) = merged.last_mut()
                && range.start <= previous.end
            {
                previous.end = previous.end.max(range.end);
                continue;
            }
            merged.push(range);
        }
        Self(merged)
    }

    pub fn contains(&self, address: u64) -> bool {
        let index = self.0.partition_point(|range| range.start <= address);
        index != 0 && address < self.0[index - 1].end
    }

    #[cfg(target_arch = "aarch64")]
    pub fn max_address(&self) -> u64 {
        self.0
            .last()
            .map(|range| range.end.saturating_sub(1))
            .unwrap_or_default()
    }
}

pub fn read_process_maps(pid: i32) -> Result<Vec<MapEntry>> {
    let path = format!("/proc/{pid}/maps");
    let contents = fs::read_to_string(&path).with_context(|| format!("failed to read {path}"))?;
    contents.lines().map(parse_map_line).collect()
}

pub fn parse_map_line(line: &str) -> Result<MapEntry> {
    let mut fields = line.split_whitespace();
    let range = fields.next().context("mapping is missing address range")?;
    let permissions = fields
        .next()
        .context("mapping is missing permissions")?
        .to_owned();
    let offset = parse_hex(fields.next(), "mapping offset")?;
    let device = fields
        .next()
        .context("mapping is missing device")?
        .to_owned();
    let inode = fields
        .next()
        .context("mapping is missing inode")?
        .parse::<u64>()
        .context("invalid mapping inode")?;
    let path = fields.collect::<Vec<_>>().join(" ");
    let path = if path.is_empty() || path.starts_with('[') {
        None
    } else {
        Some(PathBuf::from(
            path.strip_suffix(" (deleted)").unwrap_or(&path),
        ))
    };

    let (start, end) = range
        .split_once('-')
        .context("invalid mapping address range")?;
    let start = u64::from_str_radix(start, 16).context("invalid mapping start")?;
    let end = u64::from_str_radix(end, 16).context("invalid mapping end")?;
    if start >= end {
        bail!("mapping start must be below end");
    }

    Ok(MapEntry {
        start,
        end,
        offset,
        permissions,
        device,
        inode,
        path,
    })
}

fn parse_hex(value: Option<&str>, field: &str) -> Result<u64> {
    u64::from_str_radix(value.with_context(|| format!("missing {field}"))?, 16)
        .with_context(|| format!("invalid {field}"))
}

pub fn mapped_files(entries: &[MapEntry]) -> Vec<PathBuf> {
    entries
        .iter()
        .filter(|entry| entry.is_executable())
        .filter_map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
