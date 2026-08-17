use anyhow::Result;

use crate::cli::Cli;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub fn run(cli: Cli) -> Result<()> {
    linux::run(cli)
}

#[cfg(not(target_os = "linux"))]
pub fn run(_cli: Cli) -> Result<()> {
    anyhow::bail!("rustprofile only supports Linux 5.4 or newer")
}
