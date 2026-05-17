//! Tokio TCP adapter around [`usbip_server`] and the URB session loop.
//!
//! Connection lifecycle:
//! 1. Read an 8-byte op header.
//! 2. If `OP_REQ_DEVLIST`, write the device list and close.
//! 3. If `OP_REQ_IMPORT`, read the 32-byte busid, open the device, write the
//!    import reply, then enter the URB loop.
//! 4. URB loop: read 48-byte URB headers and dispatch `CMD_SUBMIT` /
//!    `CMD_UNLINK` until the client disconnects.
//!
//! ## Security model
//!
//! USB/IP has **no authentication or transport encryption**. Anyone who can
//! reach the listener can enumerate and steal any allow-listed device. Two
//! defenses are exposed through [`DaemonConfig`]:
//!
//! * `listen` defaults to `127.0.0.1`. Binding to a routable address
//!   requires the caller to acknowledge that with `--allow-public` (in the
//!   CLI) or by setting [`DaemonConfig::allow_public_bind`].
//! * `policy` filters which devices the daemon will even mention. If a
//!   client requests a non-allow-listed busid via `OP_REQ_IMPORT` the
//!   request is rejected and the device is never opened, never captured.
//!
//! In addition, each successfully-imported device is locked to its
//! connecting client: a second concurrent `OP_REQ_IMPORT` for the same
//! busid is refused so two clients can't tear-and-share the same device.

use anyhow::{Context, Result, anyhow};
use host_mac::{OpenedDevice, SetupPacket, UsbDevice};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};
use usbip_proto::{
    CmdSubmit, CmdUnlink, OP_REQ_DEVLIST, OP_REQ_IMPORT, OpHeader, RetSubmit, URB_HEADER_SIZE,
    USBIP_CMD_SUBMIT, USBIP_CMD_UNLINK, USBIP_DIR_IN, USBIP_VERSION, UrbHeader,
    decode_req_import_busid, encode_rep_import_err, encode_rep_import_ok, write_ret_submit,
    write_ret_unlink,
};
use usbip_server::{Reply, encode_rep_devlist, handle_op, to_exported};

/// Per-URB transfer timeout. Long enough for slow bulk operations on large
/// mass-storage SCSI commands; short enough that a wedged device will free
/// the connection within a minute.
const URB_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-connection cap on in-flight `CMD_SUBMIT` URBs. Bounds the daemon's
/// blocking-thread footprint when a misbehaving client floods submits
/// without ever reading replies.
const MAX_INFLIGHT_URBS: usize = 256;

/// Linux errno values used in `RET_SUBMIT.status` on failure.
///
/// These are the values the *client* (always a Linux kernel usbip-vhci
/// driver) expects, so we hard-code them as Linux-x86 ABI numbers
/// regardless of the host we're compiled on. They are NOT the host's
/// errno; do not replace with `libc::EPIPE` etc.
const EPIPE: i32 = -32;
const EOVERFLOW: i32 = -75;

/// Access policy applied to every `OP_REQ_DEVLIST` and `OP_REQ_IMPORT`.
///
/// `AllowAll` is what the original Linux `usbipd` does: anything that is
/// `usbip bind`-ed is exportable. We default to an explicit allow-list
/// instead because there is no `usbip bind` step on macOS — every device
/// would otherwise be exportable the moment the daemon starts.
#[derive(Debug, Clone, Default)]
pub enum AccessPolicy {
    /// Reject every device.
    #[default]
    DenyAll,
    /// Allow every device. Equivalent to the upstream Linux behavior;
    /// only safe on a single-user, fully-trusted host.
    AllowAll,
    /// Allow only the listed `(vendor_id, product_id)` pairs.
    AllowList(HashSet<(u16, u16)>),
}

impl AccessPolicy {
    fn permits(&self, vendor_id: u16, product_id: u16) -> bool {
        match self {
            Self::DenyAll => false,
            Self::AllowAll => true,
            Self::AllowList(set) => set.contains(&(vendor_id, product_id)),
        }
    }

    /// Filter a list of devices by this policy. Used to build the
    /// `OP_REP_DEVLIST` body so the client never sees devices it isn't
    /// allowed to import.
    fn filter<'a>(&self, devices: &'a [UsbDevice]) -> Vec<&'a UsbDevice> {
        devices
            .iter()
            .filter(|d| self.permits(d.vendor_id, d.product_id))
            .collect()
    }
}

/// Configuration for [`run`]. Constructed by the CLI.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub listen: SocketAddr,
    pub policy: AccessPolicy,
    /// If `false` (the default), binding to any address other than
    /// loopback returns an error. Set to `true` from the CLI flag
    /// `--allow-public` after the user has acknowledged the consequence.
    pub allow_public_bind: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 3240)),
            policy: AccessPolicy::default(),
            allow_public_bind: false,
        }
    }
}

/// Shared state held for the lifetime of the listener and threaded
/// through every spawned task.
struct DaemonState {
    policy: AccessPolicy,
    /// Devices currently held open by a client, keyed by busid. The
    /// value is the peer address for diagnostics. Used to refuse a
    /// second concurrent import of the same device.
    attached: std::sync::Mutex<HashMap<String, SocketAddr>>,
}

impl DaemonState {
    fn new(policy: AccessPolicy) -> Self {
        Self {
            policy,
            attached: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Try to claim `busid` for `peer`. Returns `Some(guard)` on success,
    /// `None` if another client already has it open. The guard releases
    /// the slot on drop.
    fn try_attach(self: &Arc<Self>, busid: &str, peer: SocketAddr) -> Option<AttachGuard> {
        let mut map = self.attached.lock().expect("attached map poisoned");
        if map.contains_key(busid) {
            return None;
        }
        map.insert(busid.to_owned(), peer);
        Some(AttachGuard {
            state: Arc::clone(self),
            busid: busid.to_owned(),
        })
    }
}

struct AttachGuard {
    state: Arc<DaemonState>,
    busid: String,
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        self.state
            .attached
            .lock()
            .expect("attached map poisoned")
            .remove(&self.busid);
    }
}

pub fn run(config: DaemonConfig) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(serve(config))
}

async fn serve(config: DaemonConfig) -> Result<()> {
    if !is_loopback(&config.listen) && !config.allow_public_bind {
        return Err(anyhow!(
            "refusing to bind {}: USB/IP has no authentication. \
             Re-run with --allow-public if you really mean to expose USB devices to the network.",
            config.listen
        ));
    }
    if matches!(config.policy, AccessPolicy::AllowAll) {
        warn!(
            "policy = AllowAll: every USB device on this host is exportable. \
             Use --allow vid:pid (repeatable) to restrict."
        );
    }
    if matches!(config.policy, AccessPolicy::DenyAll) {
        warn!(
            "policy = DenyAll: no devices are exportable. \
             Use --allow vid:pid (repeatable) or --allow-all to expose devices."
        );
    }
    let state = Arc::new(DaemonState::new(config.policy.clone()));

    let listener = TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("bind {}", config.listen))?;
    info!(listen = %config.listen, "usbipd listening");

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            res = listener.accept() => {
                match res {
                    Ok((stream, peer)) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(stream, peer, state).await {
                                warn!(%peer, error = %e, "client session ended with error");
                            }
                        });
                    }
                    Err(e) => warn!(error = %e, "accept failed"),
                }
            }
            _ = &mut shutdown => {
                info!("ctrl-c received, shutting down");
                return Ok(());
            }
        }
    }
}

fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

async fn handle_client(
    mut stream: TcpStream,
    peer: SocketAddr,
    state: Arc<DaemonState>,
) -> Result<()> {
    debug!(%peer, "client connected");
    let mut header_buf = [0u8; OpHeader::SIZE];
    stream
        .read_exact(&mut header_buf)
        .await
        .with_context(|| format!("read op header from {peer}"))?;
    let header = OpHeader::decode(&header_buf).context("decode op header")?;
    debug!(%peer, version = format!("0x{:04x}", header.version), code = format!("0x{:04x}", header.code), "op header");

    if header.version != USBIP_VERSION {
        warn!(
            %peer,
            got = format!("0x{:04x}", header.version),
            expected = format!("0x{USBIP_VERSION:04x}"),
            "rejecting op with unsupported USB/IP version"
        );
        return Ok(());
    }

    match header.code {
        OP_REQ_DEVLIST => handle_devlist(stream, peer, &state).await,
        OP_REQ_IMPORT => handle_import(stream, peer, &state).await,
        _ => {
            // Reuse the legacy handler for symmetry/logging.
            let devices = host_mac::list_devices().context("enumerate USB devices")?;
            let filtered = state.policy.filter(&devices);
            let owned: Vec<UsbDevice> = filtered.into_iter().cloned().collect();
            match handle_op(header, &owned)? {
                Reply::Bytes(bytes) => {
                    stream.write_all(&bytes).await?;
                }
                Reply::Unsupported(code) => {
                    warn!(%peer, code = format!("0x{code:04x}"), "unsupported op code; closing");
                }
            }
            Ok(())
        }
    }
}

async fn handle_devlist(
    mut stream: TcpStream,
    peer: SocketAddr,
    state: &Arc<DaemonState>,
) -> Result<()> {
    let devices = host_mac::list_devices().context("enumerate USB devices")?;
    let filtered: Vec<UsbDevice> = state.policy.filter(&devices).into_iter().cloned().collect();
    let bytes = encode_rep_devlist(&filtered);
    stream
        .write_all(&bytes)
        .await
        .with_context(|| format!("write devlist to {peer}"))?;
    stream.flush().await.ok();
    debug!(%peer, bytes = bytes.len(), exported = filtered.len(), total = devices.len(), "devlist sent");
    Ok(())
}

async fn handle_import(
    mut stream: TcpStream,
    peer: SocketAddr,
    state: &Arc<DaemonState>,
) -> Result<()> {
    let mut busid_buf = [0u8; 32];
    stream
        .read_exact(&mut busid_buf)
        .await
        .with_context(|| format!("read import busid from {peer}"))?;
    let busid = decode_req_import_busid(&busid_buf)?;
    info!(%peer, %busid, "OP_REQ_IMPORT");

    // Find the device in the current list so we can echo its descriptor
    // fields back in the import reply.
    let devices = host_mac::list_devices().context("enumerate USB devices")?;
    let Some(desc) = devices.into_iter().find(|d| d.busid == busid) else {
        warn!(%peer, %busid, "import: device not found");
        return send_import_err(&mut stream).await;
    };

    // Policy gate: refuse to even open a non-allow-listed device.
    if !state.policy.permits(desc.vendor_id, desc.product_id) {
        warn!(
            %peer,
            %busid,
            vid = format!("{:04x}", desc.vendor_id),
            pid = format!("{:04x}", desc.product_id),
            "import: device not in allow-list, refusing"
        );
        return send_import_err(&mut stream).await;
    }

    // Mutex gate: a device can only be opened by one client at a time
    // (force-capture is process-exclusive on macOS anyway).
    let Some(_attach_guard) = state.try_attach(&busid, peer) else {
        let holder = state
            .attached
            .lock()
            .expect("attached map poisoned")
            .get(&busid)
            .copied();
        warn!(
            %peer,
            %busid,
            holder = ?holder,
            "import: device is already attached to another client, refusing"
        );
        return send_import_err(&mut stream).await;
    };

    // Open the device on a blocking thread (nusb is sync).
    let opened = match tokio::task::spawn_blocking({
        let busid = busid.clone();
        move || OpenedDevice::open(&busid)
    })
    .await
    .context("join open task")?
    {
        Ok(d) => Arc::new(d),
        Err(e) => {
            warn!(%peer, %busid, error = %e, "import: open failed");
            return send_import_err(&mut stream).await;
        }
    };
    info!(%peer, %busid, "device opened, entering URB loop");

    // Fold the snapshot (real bConfigurationValue / bNumConfigurations
    // from the opened device, and the deterministic busnum/devnum from
    // OpenedDevice) into the pre-import enumeration entry, which has
    // the descriptor strings.
    let snap = opened.descriptor_snapshot();
    let desc = UsbDevice {
        busid: snap.busid.clone(),
        busnum: snap.busnum,
        devnum: snap.devnum,
        configuration_value: snap.configuration_value,
        num_configurations: snap.num_configurations,
        interfaces: if snap.interfaces.is_empty() {
            desc.interfaces.clone()
        } else {
            snap.interfaces
        },
        ..desc
    };

    // Send success import reply.
    let mut out = Vec::with_capacity(8 + 312);
    encode_rep_import_ok(&mut out, &to_exported(&desc));
    stream.write_all(&out).await?;

    // _attach_guard drops at end of scope, freeing the device for re-attachment.
    urb_loop(stream, peer, desc, opened).await
}

async fn send_import_err(stream: &mut TcpStream) -> Result<()> {
    let mut out = Vec::new();
    encode_rep_import_err(&mut out);
    stream.write_all(&out).await.ok();
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn urb_loop(
    stream: TcpStream,
    peer: SocketAddr,
    desc: UsbDevice,
    opened: Arc<OpenedDevice>,
) -> Result<()> {
    let devid = (desc.busnum << 16) | desc.devnum;
    let (mut reader, writer) = stream.into_split();
    let writer: Arc<AsyncMutex<OwnedWriteHalf>> = Arc::new(AsyncMutex::new(writer));
    let inflight: Arc<std::sync::Mutex<HashMap<u32, Inflight>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));
    let permits = Arc::new(Semaphore::new(MAX_INFLIGHT_URBS));

    let mut header_buf = [0u8; URB_HEADER_SIZE];
    let result: Result<()> = loop {
        // EOF here means clean client disconnect.
        if let Err(e) = reader.read_exact(&mut header_buf).await {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                debug!(%peer, "client disconnected");
                break Ok(());
            }
            break Err(anyhow::Error::from(e).context("read URB header"));
        }
        let basic = match UrbHeader::decode(&header_buf[0..UrbHeader::SIZE]) {
            Ok(b) => b,
            Err(e) => break Err(anyhow::Error::from(e).context("decode URB header")),
        };
        if basic.devid != devid {
            warn!(
                %peer,
                got = format!("0x{:08x}", basic.devid),
                expected = format!("0x{devid:08x}"),
                "URB devid mismatch; closing connection"
            );
            break Err(anyhow!("URB devid mismatch"));
        }

        match basic.command {
            USBIP_CMD_SUBMIT => {
                let cmd = match CmdSubmit::decode(&header_buf[UrbHeader::SIZE..URB_HEADER_SIZE]) {
                    Ok(c) => c,
                    Err(e) => break Err(anyhow::Error::from(e).context("decode CMD_SUBMIT")),
                };
                // For OUT transfers the client sends the payload right after
                // the header — must consume it serially before spawning the
                // transfer task (we have only one reader).
                let dir_in = basic.direction == USBIP_DIR_IN;
                let tbl = usize::try_from(cmd.transfer_buffer_length).unwrap_or(0);
                let mut out_payload = Vec::new();
                if !dir_in && tbl > 0 {
                    out_payload.resize(tbl, 0);
                    if let Err(e) = reader.read_exact(&mut out_payload).await {
                        break Err(anyhow::Error::from(e).context("read OUT payload"));
                    }
                }

                let ep_addr = if basic.ep == 0 {
                    0
                } else {
                    u8::try_from(basic.ep & 0xF).unwrap_or(0)
                        | if dir_in { 0x80 } else { 0x00 }
                };
                let cancelled = Arc::new(AtomicBool::new(false));
                // Acquire a permit before spawning so a flooding client
                // back-pressures the read loop.
                let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
                    break Err(anyhow!("semaphore closed"));
                };
                let opened2 = Arc::clone(&opened);
                let writer2 = Arc::clone(&writer);
                let inflight2 = Arc::clone(&inflight);
                let cancelled2 = Arc::clone(&cancelled);
                let seqnum = basic.seqnum;
                let handle = tokio::spawn(async move {
                    let _permit = permit;
                    run_submit(opened2, writer2, basic, cmd, out_payload, cancelled2).await;
                    inflight2
                        .lock()
                        .expect("inflight map poisoned")
                        .remove(&seqnum);
                });
                inflight
                    .lock()
                    .expect("inflight map poisoned")
                    .insert(seqnum, Inflight {
                        abort: handle.abort_handle(),
                        ep_addr,
                        cancelled,
                    });
            }
            USBIP_CMD_UNLINK => {
                let unlink = match CmdUnlink::decode(
                    &header_buf[UrbHeader::SIZE..URB_HEADER_SIZE],
                ) {
                    Ok(u) => u,
                    Err(e) => break Err(anyhow::Error::from(e).context("decode CMD_UNLINK")),
                };
                // Look up the target SUBMIT, mark it cancelled, force its
                // in-flight nusb call to return early via cancel_endpoint,
                // and abort the task so it never writes a RET_SUBMIT.
                let status = {
                    let mut map = inflight.lock().expect("inflight map poisoned");
                    if let Some(info) = map.remove(&unlink.unlink_seqnum) {
                        info.cancelled.store(true, Ordering::SeqCst);
                        if info.ep_addr != 0 {
                            opened.cancel_endpoint(info.ep_addr);
                        }
                        info.abort.abort();
                        0
                    } else {
                        // The URB has already completed (or never existed).
                        // Linux returns -ENOENT but most clients ignore the
                        // status; stay quiet and report success.
                        0
                    }
                };
                debug!(
                    %peer,
                    seqnum = basic.seqnum,
                    unlink_seqnum = unlink.unlink_seqnum,
                    "CMD_UNLINK"
                );
                let mut out = Vec::with_capacity(URB_HEADER_SIZE);
                write_ret_unlink(&mut out, basic.seqnum, status);
                if let Err(e) = writer.lock().await.write_all(&out).await {
                    break Err(anyhow::Error::from(e).context("write RET_UNLINK"));
                }
            }
            other => {
                break Err(anyhow!(
                    "unknown URB command 0x{other:08x}; closing connection"
                ));
            }
        }
    };

    // Drain in-flight tasks on disconnect so they don't keep firing into
    // a dead socket. Aborting is fire-and-forget; tasks observe the
    // cancelled flag and skip writing.
    let pending: Vec<Inflight> = inflight
        .lock()
        .expect("inflight map poisoned")
        .drain()
        .map(|(_, v)| v)
        .collect();
    for info in pending {
        info.cancelled.store(true, Ordering::SeqCst);
        info.abort.abort();
    }
    result
}

struct Inflight {
    abort: AbortHandle,
    ep_addr: u8,
    cancelled: Arc<AtomicBool>,
}

async fn run_submit(
    opened: Arc<OpenedDevice>,
    writer: Arc<AsyncMutex<OwnedWriteHalf>>,
    basic: UrbHeader,
    cmd: CmdSubmit,
    out_payload: Vec<u8>,
    cancelled: Arc<AtomicBool>,
) {
    let dir_in = basic.direction == USBIP_DIR_IN;
    let tbl = usize::try_from(cmd.transfer_buffer_length).unwrap_or(0);
    let ep = basic.ep;
    let seqnum = basic.seqnum;

    // Run the actual nusb call on a blocking thread.
    let result = match tokio::task::spawn_blocking(
        move || -> Result<(usize, Vec<u8>), host_mac::HostError> {
            if ep == 0 {
                let setup = SetupPacket::from_bytes(cmd.setup);
                let data = opened.control_transfer(setup, &out_payload, URB_TIMEOUT)?;
                let len = if dir_in { data.len() } else { out_payload.len() };
                Ok((len, data))
            } else {
                let ep_addr =
                    u8::try_from(ep & 0xF).unwrap_or(0) | if dir_in { 0x80 } else { 0x00 };
                let r = opened.data_transfer(ep_addr, tbl, &out_payload, URB_TIMEOUT)?;
                Ok((r.actual_length, r.data))
            }
        },
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(seqnum, ep, error = %e, "transfer task join failed");
            return;
        }
    };

    // Cancelled by CMD_UNLINK: skip the reply entirely.
    if cancelled.load(Ordering::SeqCst) {
        debug!(seqnum, ep, "URB cancelled; no RET_SUBMIT");
        return;
    }

    let (status, actual_length, payload) = match result {
        Ok((actual, data)) => {
            if dir_in {
                if data.len() > tbl {
                    (EOVERFLOW, 0, Vec::new())
                } else {
                    let actual_i32 = i32::try_from(actual).unwrap_or(i32::MAX);
                    (0, actual_i32, data)
                }
            } else {
                let actual_i32 = i32::try_from(actual).unwrap_or(i32::MAX);
                (0, actual_i32, Vec::new())
            }
        }
        Err(e) => {
            warn!(seqnum, ep, error = %e, "transfer failed");
            (EPIPE, 0, Vec::new())
        }
    };

    let ret = RetSubmit {
        status,
        actual_length,
        start_frame: 0,
        number_of_packets: 0,
        error_count: 0,
        padding: [0; 8],
    };
    let mut out = Vec::with_capacity(URB_HEADER_SIZE + payload.len());
    write_ret_submit(&mut out, seqnum, &ret, &payload);
    if let Err(e) = writer.lock().await.write_all(&out).await {
        debug!(seqnum, error = %e, "client write failed; closing");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(vid: u16, pid: u16) -> UsbDevice {
        UsbDevice {
            busid: format!("01-{vid:x}"),
            busnum: 1,
            devnum: 1,
            speed: 3,
            vendor_id: vid,
            product_id: pid,
            bcd_device: 0,
            class: 0,
            subclass: 0,
            protocol: 0,
            configuration_value: 1,
            num_configurations: 1,
            manufacturer: None,
            product: None,
            serial: None,
            interfaces: Vec::new(),
        }
    }

    #[test]
    fn policy_deny_all_rejects_everything() {
        let p = AccessPolicy::DenyAll;
        assert!(!p.permits(0x1050, 0x0407));
        assert!(p.filter(&[dev(0x1050, 0x0407)]).is_empty());
    }

    #[test]
    fn policy_allow_all_admits_everything() {
        let p = AccessPolicy::AllowAll;
        assert!(p.permits(0x1050, 0x0407));
        assert_eq!(p.filter(&[dev(0x1050, 0x0407), dev(1, 2)]).len(), 2);
    }

    #[test]
    fn policy_allow_list_is_exact_match() {
        let p = AccessPolicy::AllowList([(0x1050, 0x0407)].into_iter().collect());
        assert!(p.permits(0x1050, 0x0407));
        assert!(!p.permits(0x1050, 0x0408));
        assert!(!p.permits(0x1051, 0x0407));
        let devs = [dev(0x1050, 0x0407), dev(1, 2)];
        let filtered = p.filter(&devs);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].vendor_id, 0x1050);
    }

    #[test]
    fn try_attach_is_mutually_exclusive_per_busid() {
        let state = Arc::new(DaemonState::new(AccessPolicy::AllowAll));
        let peer1: SocketAddr = "127.0.0.1:1111".parse().unwrap();
        let peer2: SocketAddr = "127.0.0.1:2222".parse().unwrap();
        let g1 = state.try_attach("01-1", peer1);
        assert!(g1.is_some(), "first client should win");
        let g2 = state.try_attach("01-1", peer2);
        assert!(g2.is_none(), "second concurrent client should be refused");
        // A *different* busid should still succeed.
        let g3 = state.try_attach("01-2", peer2);
        assert!(g3.is_some());
        drop(g1);
        let g4 = state.try_attach("01-1", peer2);
        assert!(g4.is_some(), "should be re-attachable after first guard dropped");
    }

    #[test]
    fn is_loopback_classifies_correctly() {
        assert!(is_loopback(&"127.0.0.1:3240".parse().unwrap()));
        assert!(is_loopback(&"[::1]:3240".parse().unwrap()));
        assert!(!is_loopback(&"0.0.0.0:3240".parse().unwrap()));
        assert!(!is_loopback(&"192.168.1.10:3240".parse().unwrap()));
    }
}
