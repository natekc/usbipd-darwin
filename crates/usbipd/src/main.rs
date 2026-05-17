//! `usbipd` — macOS USB/IP server daemon.

#![forbid(unsafe_code)]

mod daemon;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use std::collections::HashSet;
use std::net::SocketAddr;
use tracing_subscriber::EnvFilter;

use daemon::{AccessPolicy, DaemonConfig};

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
    /// By default the daemon binds to 127.0.0.1 and refuses to export
    /// any device. Specify one or more `--allow VID:PID` flags (or
    /// `--allow-all` to skip filtering) to actually export devices, and
    /// `--allow-public` to accept binding to a non-loopback address.
    Daemon {
        /// Address and port to listen on.
        ///
        /// Defaults to 127.0.0.1. Any non-loopback address requires
        /// `--allow-public` as a separate acknowledgement.
        #[arg(long, default_value_t = SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)))]
        listen: SocketAddr,

        /// Allow a specific (VendorID:ProductID) pair. Hex, no `0x` prefix.
        /// Example: `--allow 1050:0407` (`YubiKey` 5).
        /// Repeatable.
        #[arg(long = "allow", value_name = "VID:PID", value_parser = parse_vid_pid)]
        allow: Vec<(u16, u16)>,

        /// Allow every USB device on this host. Only safe on a fully-
        /// trusted single-user machine. Mutually exclusive with `--allow`.
        #[arg(long, conflicts_with = "allow")]
        allow_all: bool,

        /// Permit binding `--listen` to a non-loopback address. The
        /// USB/IP protocol is unauthenticated; the only legitimate
        /// reason to set this is when fronting the daemon with a
        /// firewall or VPN that supplies its own access control.
        #[arg(long)]
        allow_public: bool,
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

fn parse_vid_pid(s: &str) -> Result<(u16, u16), String> {
    let (v, p) = s
        .split_once(':')
        .ok_or_else(|| format!("expected VID:PID, got {s:?}"))?;
    let vid = u16::from_str_radix(v, 16).map_err(|e| format!("bad vid {v:?}: {e}"))?;
    let pid = u16::from_str_radix(p, 16).map_err(|e| format!("bad pid {p:?}: {e}"))?;
    Ok((vid, pid))
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
        Cmd::Daemon {
            listen,
            allow,
            allow_all,
            allow_public,
        } => {
            let policy = if allow_all {
                AccessPolicy::AllowAll
            } else if allow.is_empty() {
                AccessPolicy::DenyAll
            } else {
                AccessPolicy::AllowList(allow.into_iter().collect::<HashSet<_>>())
            };
            daemon::run(DaemonConfig {
                listen,
                policy,
                allow_public_bind: allow_public,
            })
            .context("daemon")
        }
        #[cfg(target_os = "macos")]
        Cmd::ReleaseCapture { busid } => release_capture(&busid),
    }
}

#[cfg(target_os = "macos")]
fn release_capture(busid: &str) -> Result<()> {
    host_mac::release_capture(busid).map_err(|e| anyhow!("{e}"))?;
    println!("released capture for busid {busid}");
    Ok(())
}

fn list() -> Result<()> {
    let devices = host_mac::list_devices().map_err(|e| anyhow!("{e}"))?;
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
        println!("     allow with: --allow {:04x}:{:04x}", d.vendor_id, d.product_id);
    }
    Ok(())
}
