//! `usbipd` — macOS USB/IP server daemon.

#![forbid(unsafe_code)]

mod daemon;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use tracing_subscriber::EnvFilter;

/// Default TCP port for USB/IP. Matches the IANA registration and the
/// hard-coded default in the Linux `usbipd` and `usbip` utilities.
const DEFAULT_PORT: u16 = 3240;

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
    /// Run the USB/IP server.
    ///
    /// At this stage the daemon only answers `OP_REQ_DEVLIST` (i.e. it makes
    /// `usbip list -r <host>` work). Attaching a device for actual transfers
    /// is not yet implemented.
    Daemon {
        /// Address and port to listen on.
        #[arg(long, default_value_t = SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)))]
        listen: SocketAddr,
    },
    /// Release a force-captured device back to macOS (root only).
    ///
    /// Manual escape hatch for the case where the daemon was killed
    /// ungracefully (e.g. `SIGKILL`) while holding a device with the
    /// `USBDeviceReEnumerate` capture flag set. macOS keeps the device
    /// detached from its kernel drivers until either a release
    /// re-enumerate or a physical unplug; this command does the former.
    #[cfg(target_os = "macos")]
    ReleaseCapture {
        /// USB/IP busid of the device to release (e.g. `01-1`).
        busid: String,
    },
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
        Cmd::Daemon { listen } => daemon::run(listen),
        #[cfg(target_os = "macos")]
        Cmd::ReleaseCapture { busid } => release_capture(&busid),
    }
}

#[cfg(target_os = "macos")]
fn release_capture(busid: &str) -> Result<()> {
    host_mac::release_capture(busid)?;
    println!("released capture for busid {busid}");
    Ok(())
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
