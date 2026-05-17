//! `usbipd` — macOS USB/IP server daemon.

#![forbid(unsafe_code)]

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "usbipd", version, about = "macOS USB/IP server daemon")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// List local USB devices available for sharing.
    List,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::List => list(),
    }
}

fn list() -> Result<()> {
    println!("usbipd list: not implemented yet (MVP-1)");
    Ok(())
}
