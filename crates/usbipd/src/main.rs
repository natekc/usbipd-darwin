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
    let devices = host_mac::list_devices()?;
    println!("Local USB devices");
    println!("=================");
    if devices.is_empty() {
        println!(" (none)");
        return Ok(());
    }
    for d in &devices {
        let vendor = d.manufacturer.as_deref().unwrap_or("(unknown vendor)");
        let product = d.product.as_deref().unwrap_or("(unknown product)");
        println!(
            " - busid {} ({:04x}:{:04x})",
            d.busid, d.vendor_id, d.product_id
        );
        println!("     {vendor} : {product}");
        println!(
            "     class={:02x}/{:02x}/{:02x}",
            d.class, d.subclass, d.protocol
        );
    }
    Ok(())
}
