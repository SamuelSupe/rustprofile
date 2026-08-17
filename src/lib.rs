mod cli;
mod config;
mod diagnostics;
mod firefox;
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

pub use config::{AllocatorChoice, FirefoxProfileFormat, ProfileKind, UnwindMode};
pub use diagnostics::{
    CapabilityReport, CheckReport, OtlpExportDiagnostics, OtlpExportStatus, TargetKind,
    TargetMetadata, WindowDiagnostics,
};

pub fn run() -> Result<()> {
    let command = cli::Cli::parse();
    platform::run(command)
}
