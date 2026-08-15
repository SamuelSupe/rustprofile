use std::{fmt, str::FromStr, time::Duration};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

pub const DEFAULT_CPU_FREQUENCY: u32 = 49;
pub const DEFAULT_ALLOC_INTERVAL: u64 = 512 * 1024;
pub const DEFAULT_MAX_FRAMES: usize = 127;
pub const DEFAULT_STACK_BYTES: usize = 16 * 1024;
pub const DEFAULT_MAX_THREADS: usize = 1024;
pub const DEFAULT_MAX_STACKS: usize = 65_536;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum UnwindMode {
    Auto,
    Fp,
    Dwarf,
}

impl fmt::Display for UnwindMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Auto => "auto",
            Self::Fp => "fp",
            Self::Dwarf => "dwarf",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    Cpu,
    Heap,
}

impl fmt::Display for ProfileKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Cpu => "cpu",
            Self::Heap => "heap",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum AllocatorChoice {
    Auto,
    Rust,
    System,
}

impl fmt::Display for AllocatorChoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Auto => "auto",
            Self::Rust => "rust",
            Self::System => "system",
        })
    }
}

pub fn parse_duration(value: &str) -> Result<Duration, String> {
    if value == "0" {
        return Ok(Duration::ZERO);
    }
    humantime::parse_duration(value).map_err(|error| error.to_string())
}

pub fn parse_byte_size(value: &str) -> Result<u64, String> {
    let size = parse_size::parse_size(value).map_err(|error| error.to_string())?;
    if size == 0 {
        return Err("size must be greater than zero".to_owned());
    }
    Ok(size)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl KernelVersion {
    pub const MINIMUM: Self = Self {
        major: 5,
        minor: 8,
        patch: 0,
    };

    pub fn is_supported(self) -> bool {
        self >= Self::MINIMUM
    }
}

impl Ord for KernelVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
    }
}

impl PartialOrd for KernelVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl FromStr for KernelVersion {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let version = value.split_once('-').map_or(value, |(version, _)| version);
        let mut parts = version.split('.');
        let major = parse_version_part(parts.next(), value)?;
        let minor = parse_version_part(parts.next(), value)?;
        let patch = parts
            .next()
            .map(|part| part.parse::<u32>().context("invalid kernel patch version"))
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            major,
            minor,
            patch,
        })
    }
}

fn parse_version_part(part: Option<&str>, original: &str) -> Result<u32> {
    let Some(part) = part else {
        bail!("invalid kernel release {original:?}");
    };
    part.parse::<u32>()
        .with_context(|| format!("invalid kernel release {original:?}"))
}
