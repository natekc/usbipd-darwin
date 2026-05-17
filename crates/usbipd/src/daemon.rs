//! Tokio adapter (TCP or unix-domain socket) around the USB/IP wire
//! protocol, and the URB session loop.
//!
//! Connection lifecycle:
//! 1. Read an 8-byte op header.
//! 2. If `OP_REQ_DEVLIST`, write the device list and close.
//! 3. If `OP_REQ_IMPORT`, read the 32-byte busid, open the device, write the
//!    import reply, then enter the URB loop.
//! 4. URB loop: read 48-byte URB headers and dispatch `CMD_SUBMIT` /
//!    `CMD_UNLINK` until the client disconnects.
//!
//! Security knobs (loopback bind, allow-list, unix-socket transport) are
//! documented in the project README; this module just enforces them.

use anyhow::{Context, Result, anyhow};
use host_mac::{OpenedDevice, SetupPacket, UsbDevice};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};
use usbip_proto::{
    CmdSubmit, CmdUnlink, ExportedDevice, ExportedInterface, OP_REP_DEVLIST, OP_REQ_DEVLIST,
    OP_REQ_IMPORT, OpHeader, RetSubmit, URB_HEADER_SIZE, USBIP_CMD_SUBMIT, USBIP_CMD_UNLINK,
    USBIP_DIR_IN, USBIP_VERSION, UrbHeader, decode_req_import_busid, encode_rep_import_err,
    encode_rep_import_ok, write_ret_submit, write_ret_unlink,
};

/// Boxed trait object for the read half of a session.
type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
/// Boxed trait object for the write half of a session, shared across the
/// `urb_loop` task and every spawned per-URB task behind a tokio mutex.
type SharedWriter = Arc<AsyncMutex<Box<dyn AsyncWrite + Send + Unpin>>>;

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

    /// Keep only devices this policy permits. Used to build the
    /// `OP_REP_DEVLIST` body so the client never sees devices it isn't
    /// allowed to import.
    fn filter(&self, devices: Vec<UsbDevice>) -> Vec<UsbDevice> {
        devices
            .into_iter()
            .filter(|d| self.permits(d.vendor_id, d.product_id))
            .collect()
    }
}

/// Configuration for [`run`]. Constructed by the CLI.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub endpoint: Endpoint,
    pub policy: AccessPolicy,
    /// If `false` (the default), binding to any TCP address other than
    /// loopback returns an error. Set to `true` from the CLI flag
    /// `--allow-public` after the user has acknowledged the consequence.
    pub allow_public_bind: bool,
}

/// Where the daemon accepts connections from. TCP for the canonical
/// USB/IP wire format on the standard port; Unix-domain socket for
/// integrations (Lima, vsock-forwarders, …) that want filesystem-level
/// access control instead of network exposure.
#[derive(Debug, Clone)]
pub enum Endpoint {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(a) => write!(f, "tcp://{a}"),
            Self::Unix(p) => write!(f, "unix://{}", p.display()),
        }
    }
}

/// Shared state held for the lifetime of the listener and threaded
/// through every spawned task.
struct DaemonState {
    policy: AccessPolicy,
    /// Devices currently held open by a client, keyed by busid. The
    /// value is a human-readable peer label (`"tcp:addr"` or
    /// `"unix:uid=N,pid=M"`) for diagnostics. Used to refuse a second
    /// concurrent import of the same device.
    attached: std::sync::Mutex<HashMap<String, String>>,
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
    fn try_attach(self: &Arc<Self>, busid: &str, peer: &str) -> Option<AttachGuard> {
        let mut map = self.attached.lock().expect("attached map poisoned");
        if map.contains_key(busid) {
            return None;
        }
        map.insert(busid.to_owned(), peer.to_owned());
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
    if let Endpoint::Tcp(addr) = &config.endpoint {
        if !is_loopback(addr) && !config.allow_public_bind {
            return Err(anyhow!(
                "refusing to bind {addr}: USB/IP has no authentication. \
                 Re-run with --allow-public if you really mean to expose USB devices to the network."
            ));
        }
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

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    info!(endpoint = %config.endpoint, "usbipd listening");

    match &config.endpoint {
        Endpoint::Tcp(addr) => {
            let listener = TcpListener::bind(addr)
                .await
                .with_context(|| format!("bind {addr}"))?;
            accept_loop_tcp(listener, &state, &mut shutdown).await
        }
        Endpoint::Unix(path) => {
            // Remove any leftover socket from a previous run. Hard-fail
            // if the path exists but is not a socket — we don't want to
            // overwrite a regular file.
            if let Ok(meta) = std::fs::symlink_metadata(path) {
                use std::os::unix::fs::FileTypeExt;
                if !meta.file_type().is_socket() {
                    return Err(anyhow!(
                        "{} exists and is not a socket; refusing to overwrite",
                        path.display()
                    ));
                }
                let _ = std::fs::remove_file(path);
            }
            let listener = UnixListener::bind(path)
                .with_context(|| format!("bind unix:{}", path.display()))?;
            // Tighten permissions: only the owning user should be able to
            // connect. Filesystem permissions are the only access control
            // a unix-socket transport offers.
            if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
                warn!(error = %e, "could not chmod 0600 on socket; access will fall back to umask");
            }
            accept_loop_unix(listener, path.clone(), &state, &mut shutdown).await
        }
    }
}

async fn accept_loop_tcp(
    listener: TcpListener,
    state: &Arc<DaemonState>,
    shutdown: &mut std::pin::Pin<&mut impl std::future::Future<Output = std::io::Result<()>>>,
) -> Result<()> {
    loop {
        tokio::select! {
            res = listener.accept() => match res {
                Ok((stream, addr)) => {
                    spawn_session(Arc::clone(state), format!("tcp:{addr}"), stream.into_split());
                }
                Err(e) => warn!(error = %e, "accept failed"),
            },
            _ = &mut *shutdown => {
                info!("ctrl-c received, shutting down");
                return Ok(());
            }
        }
    }
}

async fn accept_loop_unix(
    listener: UnixListener,
    path: PathBuf,
    state: &Arc<DaemonState>,
    shutdown: &mut std::pin::Pin<&mut impl std::future::Future<Output = std::io::Result<()>>>,
) -> Result<()> {
    let result = loop {
        tokio::select! {
            res = listener.accept() => match res {
                Ok((stream, _)) => {
                    spawn_session(Arc::clone(state), unix_peer_label(&stream), stream.into_split());
                }
                Err(e) => warn!(error = %e, "accept failed"),
            },
            _ = &mut *shutdown => {
                info!("ctrl-c received, shutting down");
                break Ok(());
            }
        }
    };
    let _ = std::fs::remove_file(&path);
    result
}

/// Wrap an accepted (read, write) pair in the trait-object types the
/// session handler needs and spawn it. Factored out to keep the two
/// accept loops a one-liner so the only thing that differs between
/// them is the peer label format.
fn spawn_session<R, W>(state: Arc<DaemonState>, peer: String, halves: (R, W))
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let (r, w) = halves;
    let reader: BoxRead = Box::new(r);
    let writer: SharedWriter = Arc::new(AsyncMutex::new(Box::new(w)));
    tokio::spawn(async move {
        let peer_for_log = peer.clone();
        if let Err(e) = handle_session(reader, writer, peer, state).await {
            warn!(peer = %peer_for_log, error = %e, "client session ended with error");
        }
    });
}

fn unix_peer_label(stream: &UnixStream) -> String {
    if let Ok(cred) = stream.peer_cred() {
        format!("unix:uid={},pid={}", cred.uid(), cred.pid().unwrap_or(0))
    } else {
        "unix:unknown".into()
    }
}

fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

async fn handle_session(
    mut reader: BoxRead,
    writer: SharedWriter,
    peer: String,
    state: Arc<DaemonState>,
) -> Result<()> {
    debug!(%peer, "client connected");
    let mut header_buf = [0u8; OpHeader::SIZE];
    reader
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
        OP_REQ_DEVLIST => handle_devlist(&writer, &peer, &state).await,
        OP_REQ_IMPORT => handle_import(reader, writer, peer, &state).await,
        code => {
            warn!(%peer, code = format!("0x{code:04x}"), "unsupported op code; closing");
            Ok(())
        }
    }
}

async fn handle_devlist(writer: &SharedWriter, peer: &str, state: &Arc<DaemonState>) -> Result<()> {
    let devices = host_mac::list_devices().context("enumerate USB devices")?;
    let total = devices.len();
    let filtered = state.policy.filter(devices);
    let bytes = encode_rep_devlist(&filtered);
    {
        let mut w = writer.lock().await;
        w.write_all(&bytes)
            .await
            .with_context(|| format!("write devlist to {peer}"))?;
        w.flush().await.ok();
    }
    debug!(%peer, bytes = bytes.len(), exported = filtered.len(), total, "devlist sent");
    Ok(())
}

async fn handle_import(
    mut reader: BoxRead,
    writer: SharedWriter,
    peer: String,
    state: &Arc<DaemonState>,
) -> Result<()> {
    let mut busid_buf = [0u8; 32];
    reader
        .read_exact(&mut busid_buf)
        .await
        .with_context(|| format!("read import busid from {peer}"))?;
    let busid = decode_req_import_busid(&busid_buf)?;
    info!(%peer, %busid, "OP_REQ_IMPORT");

    let devices = host_mac::list_devices().context("enumerate USB devices")?;
    let Some(desc) = devices.into_iter().find(|d| d.busid == busid) else {
        warn!(%peer, %busid, "import: device not found");
        return send_import_err(&writer).await;
    };

    if !state.policy.permits(desc.vendor_id, desc.product_id) {
        warn!(
            %peer,
            %busid,
            vid = format!("{:04x}", desc.vendor_id),
            pid = format!("{:04x}", desc.product_id),
            "import: device not in allow-list, refusing"
        );
        return send_import_err(&writer).await;
    }

    let Some(_attach_guard) = state.try_attach(&busid, &peer) else {
        let holder = state
            .attached
            .lock()
            .expect("attached map poisoned")
            .get(&busid)
            .cloned();
        warn!(
            %peer,
            %busid,
            holder = ?holder,
            "import: device is already attached to another client, refusing"
        );
        return send_import_err(&writer).await;
    };

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
            return send_import_err(&writer).await;
        }
    };
    info!(%peer, %busid, "device opened, entering URB loop");

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

    let mut out = Vec::with_capacity(8 + 312);
    encode_rep_import_ok(&mut out, &to_exported(&desc));
    writer.lock().await.write_all(&out).await?;

    // _attach_guard drops at end of scope, freeing the device for re-attachment.
    urb_loop(reader, writer, peer, desc, opened).await
}

async fn send_import_err(writer: &SharedWriter) -> Result<()> {
    let mut out = Vec::new();
    encode_rep_import_err(&mut out);
    writer.lock().await.write_all(&out).await.ok();
    Ok(())
}

/// Top-level URB session loop.
///
/// After [`handle_import`] has written the import reply, the connection
/// switches to URB mode permanently. This loop reads 48-byte URB headers
/// and dispatches each one to the appropriate helper. The helpers are
/// split out (rather than inlined) only to keep this function readable
/// — the dispatch table here is the contract a reader needs to
/// understand the protocol.
async fn urb_loop(
    mut reader: BoxRead,
    writer: SharedWriter,
    peer: String,
    desc: UsbDevice,
    opened: Arc<OpenedDevice>,
) -> Result<()> {
    let devid = (desc.busnum << 16) | desc.devnum;
    let inflight: Arc<std::sync::Mutex<HashMap<u32, Inflight>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));
    let permits = Arc::new(Semaphore::new(MAX_INFLIGHT_URBS));

    let mut header_buf = [0u8; URB_HEADER_SIZE];
    let result: Result<()> = loop {
        // EOF here means clean client disconnect. A genuine unplug
        // surfaces here too via the next transfer error on either
        // side; we deliberately don't run a separate hotplug poller.
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
                if let Err(e) = handle_cmd_submit(
                    &mut reader,
                    &writer,
                    &opened,
                    &inflight,
                    &permits,
                    basic,
                    &header_buf,
                )
                .await
                {
                    break Err(e);
                }
            }
            USBIP_CMD_UNLINK => {
                if let Err(e) =
                    handle_cmd_unlink(&writer, &opened, &inflight, &peer, basic, &header_buf).await
                {
                    break Err(e);
                }
            }
            other => {
                break Err(anyhow!(
                    "unknown URB command 0x{other:08x}; closing connection"
                ));
            }
        }
    };

    drain_inflight(&inflight);
    result
}

/// Handle a single `CMD_SUBMIT` URB header.
///
/// 1. Decode the 40-byte `CmdSubmit` trailer.
/// 2. For OUT transfers, read the payload off `reader` synchronously
///    (we have a single read half, so payload reads must be serialised).
/// 3. Acquire a permit so a flooding client back-pressures the read loop.
/// 4. Spawn a task to actually issue the transfer; record it in
///    `inflight` keyed by seqnum so a later `CMD_UNLINK` can cancel it.
async fn handle_cmd_submit(
    reader: &mut BoxRead,
    writer: &SharedWriter,
    opened: &Arc<OpenedDevice>,
    inflight: &Arc<std::sync::Mutex<HashMap<u32, Inflight>>>,
    permits: &Arc<Semaphore>,
    basic: UrbHeader,
    header_buf: &[u8; URB_HEADER_SIZE],
) -> Result<()> {
    let cmd = CmdSubmit::decode(&header_buf[UrbHeader::SIZE..URB_HEADER_SIZE])
        .context("decode CMD_SUBMIT")?;
    let dir_in = basic.direction == USBIP_DIR_IN;
    let tbl = usize::try_from(cmd.transfer_buffer_length).unwrap_or(0);
    let mut out_payload = Vec::new();
    if !dir_in && tbl > 0 {
        out_payload.resize(tbl, 0);
        reader
            .read_exact(&mut out_payload)
            .await
            .context("read OUT payload")?;
    }

    let ep_addr = if basic.ep == 0 {
        0
    } else {
        u8::try_from(basic.ep & 0xF).unwrap_or(0) | if dir_in { 0x80 } else { 0x00 }
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let permit = Arc::clone(permits)
        .acquire_owned()
        .await
        .map_err(|_| anyhow!("semaphore closed"))?;
    let opened2 = Arc::clone(opened);
    let writer2 = Arc::clone(writer);
    let inflight2 = Arc::clone(inflight);
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
    inflight.lock().expect("inflight map poisoned").insert(
        seqnum,
        Inflight {
            abort: handle.abort_handle(),
            ep_addr,
            cancelled,
        },
    );
    Ok(())
}

/// Handle a single `CMD_UNLINK` URB header.
///
/// Look up the in-flight `CMD_SUBMIT` by `unlink_seqnum`, flag it
/// cancelled, ask nusb to cancel the endpoint so any blocked transfer
/// returns immediately, abort the spawned task so it never writes a
/// stale `RET_SUBMIT`, then send `RET_UNLINK`. If the SUBMIT has
/// already completed we silently report success — most Linux clients
/// ignore the status anyway.
async fn handle_cmd_unlink(
    writer: &SharedWriter,
    opened: &Arc<OpenedDevice>,
    inflight: &Arc<std::sync::Mutex<HashMap<u32, Inflight>>>,
    peer: &str,
    basic: UrbHeader,
    header_buf: &[u8; URB_HEADER_SIZE],
) -> Result<()> {
    let unlink = CmdUnlink::decode(&header_buf[UrbHeader::SIZE..URB_HEADER_SIZE])
        .context("decode CMD_UNLINK")?;
    {
        let mut map = inflight.lock().expect("inflight map poisoned");
        if let Some(info) = map.remove(&unlink.unlink_seqnum) {
            info.cancelled.store(true, Ordering::SeqCst);
            if info.ep_addr != 0 {
                opened.cancel_endpoint(info.ep_addr);
            }
            info.abort.abort();
        }
    }
    debug!(
        %peer,
        seqnum = basic.seqnum,
        unlink_seqnum = unlink.unlink_seqnum,
        "CMD_UNLINK"
    );
    let mut out = Vec::with_capacity(URB_HEADER_SIZE);
    write_ret_unlink(&mut out, basic.seqnum, 0);
    writer
        .lock()
        .await
        .write_all(&out)
        .await
        .context("write RET_UNLINK")?;
    Ok(())
}

/// Drain in-flight tasks on disconnect so they don't keep firing into
/// a dead socket. Aborting is fire-and-forget; tasks observe the
/// cancelled flag and skip writing.
fn drain_inflight(inflight: &Arc<std::sync::Mutex<HashMap<u32, Inflight>>>) {
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
}

struct Inflight {
    abort: AbortHandle,
    ep_addr: u8,
    cancelled: Arc<AtomicBool>,
}

async fn run_submit(
    opened: Arc<OpenedDevice>,
    writer: SharedWriter,
    basic: UrbHeader,
    cmd: CmdSubmit,
    out_payload: Vec<u8>,
    cancelled: Arc<AtomicBool>,
) {
    let dir_in = basic.direction == USBIP_DIR_IN;
    let tbl = usize::try_from(cmd.transfer_buffer_length).unwrap_or(0);
    let ep = basic.ep;
    let seqnum = basic.seqnum;

    // Long enough for slow bulk operations on large mass-storage SCSI
    // commands; short enough that a wedged device frees the connection
    // within a minute. Not user-configurable yet; revisit if real
    // workloads need it.
    let timeout = Duration::from_secs(60);

    // Run the actual nusb call on a blocking thread.
    let result = match tokio::task::spawn_blocking(
        move || -> Result<(usize, Vec<u8>), host_mac::HostError> {
            if ep == 0 {
                let setup = SetupPacket::from_bytes(cmd.setup);
                let data = opened.control_transfer(setup, &out_payload, timeout)?;
                let len = if dir_in {
                    data.len()
                } else {
                    out_payload.len()
                };
                Ok((len, data))
            } else {
                let ep_addr =
                    u8::try_from(ep & 0xF).unwrap_or(0) | if dir_in { 0x80 } else { 0x00 };
                let r = opened.data_transfer(ep_addr, tbl, &out_payload, timeout)?;
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

// ---------------------------------------------------------------------
// Wire-format encoders for the bits the daemon writes directly. These
// used to live in a separate `usbip-server` crate "for transport-
// agnosticism" but in practice only the daemon ever called them.

/// Encode an `OP_REP_DEVLIST` payload: op-header + u32 device count +
/// one [`ExportedDevice`] record per device.
///
/// Hubs (`bDeviceClass == 0x09`) are filtered out because the Linux
/// usbip client never wants to attach to a hub interface.
fn encode_rep_devlist(devices: &[UsbDevice]) -> Vec<u8> {
    let exported: Vec<_> = devices.iter().filter(|d| d.class != 0x09).collect();
    let mut out = Vec::with_capacity(8 + 4 + exported.len() * 320);
    OpHeader::new(OP_REP_DEVLIST).encode(&mut out);
    let n = u32::try_from(exported.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&n.to_be_bytes());
    for d in exported {
        to_exported(d).encode(&mut out);
    }
    out
}

/// Convert the host-side [`UsbDevice`] into the wire-format
/// [`ExportedDevice`]. The path field is synthesized as
/// `/usbipd-darwin/<busid>`.
fn to_exported(d: &UsbDevice) -> ExportedDevice {
    ExportedDevice {
        path: format!("/usbipd-darwin/{}", d.busid),
        busid: d.busid.clone(),
        busnum: d.busnum,
        devnum: d.devnum,
        speed: d.speed,
        id_vendor: d.vendor_id,
        id_product: d.product_id,
        bcd_device: d.bcd_device,
        b_device_class: d.class,
        b_device_subclass: d.subclass,
        b_device_protocol: d.protocol,
        b_configuration_value: d.configuration_value,
        b_num_configurations: d.num_configurations,
        interfaces: d
            .interfaces
            .iter()
            .map(|i| ExportedInterface {
                class: i.class,
                subclass: i.subclass,
                protocol: i.protocol,
            })
            .collect(),
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
        assert!(p.filter(vec![dev(0x1050, 0x0407)]).is_empty());
    }

    #[test]
    fn policy_allow_all_admits_everything() {
        let p = AccessPolicy::AllowAll;
        assert!(p.permits(0x1050, 0x0407));
        assert_eq!(p.filter(vec![dev(0x1050, 0x0407), dev(1, 2)]).len(), 2);
    }

    #[test]
    fn policy_allow_list_is_exact_match() {
        let p = AccessPolicy::AllowList([(0x1050, 0x0407)].into_iter().collect());
        assert!(p.permits(0x1050, 0x0407));
        assert!(!p.permits(0x1050, 0x0408));
        assert!(!p.permits(0x1051, 0x0407));
        let filtered = p.filter(vec![dev(0x1050, 0x0407), dev(1, 2)]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].vendor_id, 0x1050);
    }

    #[test]
    fn try_attach_is_mutually_exclusive_per_busid() {
        let state = Arc::new(DaemonState::new(AccessPolicy::AllowAll));
        let peer1 = "tcp:127.0.0.1:1111";
        let peer2 = "tcp:127.0.0.1:2222";
        let g1 = state.try_attach("01-1", peer1);
        assert!(g1.is_some(), "first client should win");
        let g2 = state.try_attach("01-1", peer2);
        assert!(g2.is_none(), "second concurrent client should be refused");
        // A *different* busid should still succeed.
        let g3 = state.try_attach("01-2", peer2);
        assert!(g3.is_some());
        drop(g1);
        let g4 = state.try_attach("01-1", peer2);
        assert!(
            g4.is_some(),
            "should be re-attachable after first guard dropped"
        );
    }

    #[test]
    fn is_loopback_classifies_correctly() {
        assert!(is_loopback(&"127.0.0.1:3240".parse().unwrap()));
        assert!(is_loopback(&"[::1]:3240".parse().unwrap()));
        assert!(!is_loopback(&"0.0.0.0:3240".parse().unwrap()));
        assert!(!is_loopback(&"192.168.1.10:3240".parse().unwrap()));
    }

    #[test]
    fn devlist_filters_hubs_and_encodes_record() {
        let mut hub = dev(0x1d6b, 0x0002);
        hub.class = 0x09; // hub
        let kbd = dev(0x1050, 0x0407);
        let bytes = encode_rep_devlist(&[hub, kbd]);
        // op-header (8) + count (4) = 12. Count must be 1 (hub filtered).
        assert_eq!(&bytes[8..12], &[0, 0, 0, 1]);
        let (decoded, _) = usbip_proto::ExportedDevice::decode(&bytes[12..]).unwrap();
        assert_eq!(decoded.id_vendor, 0x1050);
        assert_eq!(decoded.id_product, 0x0407);
        assert!(decoded.path.starts_with("/usbipd-darwin/"));
    }
}
