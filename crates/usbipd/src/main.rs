//! `usbipd` — macOS USB/IP server daemon.

#![forbid(unsafe_code)]

mod daemon;
mod events;
mod sudoers;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

use daemon::{AccessPolicy, DaemonConfig, Endpoint};

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
    List {
        /// Emit machine-readable JSON instead of the human-readable
        /// table. Stable output, suitable for `jq` and for Lima's
        /// device-picker UI.
        #[arg(long)]
        json: bool,
    },
    /// Run the USB/IP server.
    ///
    /// By default the daemon binds to 127.0.0.1 and refuses to export
    /// any device. Specify one or more `--allow VID:PID` flags (or
    /// `--allow-all` to skip filtering) to actually export devices, and
    /// `--allow-public` to accept binding to a non-loopback address.
    Daemon {
        /// TCP address and port to listen on.
        ///
        /// Defaults to 127.0.0.1. Any non-loopback address requires
        /// `--allow-public` as a separate acknowledgement. Mutually
        /// exclusive with `--socket`.
        #[arg(long, default_value_t = SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)), conflicts_with = "socket")]
        listen: SocketAddr,

        /// Unix-domain socket path to listen on instead of TCP. The
        /// socket is created with mode 0600 (owner-only) so the only
        /// access control needed is filesystem permissions on the
        /// socket path. Intended for integrations like Lima that
        /// forward USB/IP over an already-authenticated transport.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,

        /// Allow a specific (VendorID:ProductID) pair. Hex, no `0x` prefix.
        /// Example: `--allow 1050:0407` (`YubiKey` 5).
        /// Repeatable.
        #[arg(long = "allow", value_name = "VID:PID", value_parser = parse_vid_pid)]
        allow: Vec<(u16, u16)>,

        /// Allow every USB device on this host. Only safe on a fully-
        /// trusted single-user machine. Mutually exclusive with `--allow`.
        #[arg(long, conflicts_with = "allow")]
        allow_all: bool,

        /// Permit binding `--listen` to a non-loopback TCP address.
        /// Ignored when `--socket` is used. The USB/IP protocol is
        /// unauthenticated; the only legitimate reason to set this is
        /// when fronting the daemon with a firewall or VPN that
        /// supplies its own access control.
        #[arg(long)]
        allow_public: bool,
    },
    /// Stream USB hotplug events as NDJSON over a unix socket.
    ///
    /// Intended for integrations like Lima that want to auto-attach
    /// devices as they appear. The socket is created with mode 0600;
    /// multiple consumers may subscribe simultaneously, and each new
    /// subscriber first receives the current device set as `added`
    /// events. See the `events` module docs for the on-wire schema.
    Events {
        /// Unix-domain socket path to bind. Any existing socket file
        /// at this path is replaced; any existing non-socket file is
        /// preserved (the command fails instead).
        #[arg(long, value_name = "PATH")]
        socket: PathBuf,
    },

    /// Print a `NOPASSWD:NOSETENV` sudoers fragment for an exact
    /// `usbipd daemon ...` invocation.
    ///
    /// Modeled on `limactl sudoers` / `socket_vmnet` in lima-vm/lima:
    /// the generated rule whitelists the absolute path of this binary
    /// plus the exact daemon argument list you pass here, scoped to a
    /// chosen Unix group. After installing the fragment, members of
    /// that group can run `sudo usbipd daemon ...` (with the same
    /// args) without a password.
    ///
    /// Why root? macOS denies non-root processes exclusive access to
    /// USB interfaces that are already bound to a kernel driver
    /// (`kIOReturnExclusiveAccess`, 0xe00002c5), which kills bulk
    /// transfers to e.g. mass-storage devices. Run as root to break
    /// the kernel driver's claim.
    ///
    /// Typical use:
    ///
    ///     usbipd sudoers --listen 127.0.0.1:3240 --allow 0781:5530 \
    ///         | sudo tee /etc/sudoers.d/usbipd
    ///     sudo usbipd daemon --listen 127.0.0.1:3240 --allow 0781:5530
    Sudoers {
        /// Unix group whose members get the NOPASSWD rule. The
        /// default `admin` matches the local-admin group on macOS
        /// and mirrors the convention used by `limactl sudoers`.
        #[arg(long, default_value = "admin")]
        group: String,

        /// Override the binary path baked into the rule. Defaults to
        /// the absolute path of the currently-running `usbipd`.
        /// Useful when generating the fragment on one host for
        /// deployment on another, or when the binary is reached via
        /// a symlink you'd rather pin.
        #[arg(long, value_name = "PATH")]
        binary: Option<PathBuf>,

        /// Daemon arguments to whitelist, exactly as you would pass
        /// them to `usbipd daemon`. Separate from `usbipd sudoers`'s
        /// own flags with `--`, e.g.
        /// `usbipd sudoers --group admin -- --listen 127.0.0.1:3240`.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "DAEMON_ARGS"
        )]
        daemon_args: Vec<String>,
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
        Cmd::List { json } => list(json),
        Cmd::Daemon {
            listen,
            socket,
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
            let endpoint = match socket {
                Some(path) => Endpoint::Unix(path),
                None => Endpoint::Tcp(listen),
            };
            daemon::run(DaemonConfig {
                endpoint,
                policy,
                allow_public_bind: allow_public,
            })
            .context("daemon")
        }
        Cmd::Events { socket } => events::run(socket).context("events"),
        Cmd::Sudoers {
            group,
            binary,
            daemon_args,
        } => emit_sudoers(&group, binary, &daemon_args),
        #[cfg(target_os = "macos")]
        Cmd::ReleaseCapture { busid } => release_capture(&busid),
    }
}

fn emit_sudoers(group: &str, binary: Option<PathBuf>, daemon_args: &[String]) -> Result<()> {
    // Default to `daemon` with no extra args if the caller passed
    // nothing. Picking a sensible default keeps the smoke-test
    // invocation (`usbipd sudoers`) useful, while passing args
    // through verbatim lets the user pin a tight rule.
    let owned: Vec<String> = if daemon_args.is_empty() {
        vec!["daemon".to_string()]
    } else {
        let mut v = Vec::with_capacity(daemon_args.len() + 1);
        if daemon_args.first().map(String::as_str) != Some("daemon") {
            v.push("daemon".to_string());
        }
        v.extend(daemon_args.iter().cloned());
        v
    };
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    let bin = binary.unwrap_or_else(sudoers::self_path);
    let fragment = sudoers::render(&sudoers::Spec {
        group,
        binary: &bin,
        args: &args,
    })
    .map_err(|e| anyhow!("render sudoers fragment: {e}"))?;
    print!("{fragment}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn release_capture(busid: &str) -> Result<()> {
    host_mac::release_capture(busid).map_err(|e| anyhow!("{e}"))?;
    println!("released capture for busid {busid}");
    Ok(())
}

fn list(json: bool) -> Result<()> {
    let devices = host_mac::list_devices().map_err(|e| anyhow!("{e}"))?;
    if json {
        let rows: Vec<JsonDevice> = devices.iter().map(JsonDevice::from).collect();
        let stdout = std::io::stdout().lock();
        serde_json::to_writer_pretty(stdout, &rows).context("write json")?;
        println!();
        return Ok(());
    }
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
        println!(
            "     allow with: --allow {:04x}:{:04x}",
            d.vendor_id, d.product_id
        );
    }
    Ok(())
}

/// JSON projection of a device. Field set is deliberately small and
/// stable: any consumer (e.g. Lima's device-picker) should be able to
/// rely on these field names and types not changing without a major
/// version bump.
#[derive(serde::Serialize)]
struct JsonDevice {
    busid: String,
    vendor_id: String,
    product_id: String,
    manufacturer: Option<String>,
    product: Option<String>,
    class: u8,
    subclass: u8,
    protocol: u8,
    speed: u32,
}

impl From<&host_mac::UsbDevice> for JsonDevice {
    fn from(d: &host_mac::UsbDevice) -> Self {
        Self {
            busid: d.busid.clone(),
            vendor_id: format!("{:04x}", d.vendor_id),
            product_id: format!("{:04x}", d.product_id),
            manufacturer: d.manufacturer.clone(),
            product: d.product.clone(),
            class: d.class,
            subclass: d.subclass,
            protocol: d.protocol,
            speed: d.speed,
        }
    }
}
