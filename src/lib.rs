mod cli;
mod config;
mod diagnostics;
mod maps;
mod otlp;
mod platform;
mod pprof;
mod process;
mod profile;
mod svg;
mod symbol;
mod target;
mod unwind;

pub mod heap;

use anyhow::Result;
use clap::Parser;

pub use config::{AllocatorChoice, ProfileKind, UnwindMode};
pub use diagnostics::{
    CheckReport, OtlpExportDiagnostics, OtlpExportStatus, TargetKind, TargetMetadata,
    WindowDiagnostics,
};

pub fn run() -> Result<()> {
    let command = cli::Cli::parse();
    platform::run(command)
}
