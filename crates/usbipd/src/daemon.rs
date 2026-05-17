//! Tokio TCP adapter around [`usbip_server`] and the URB session loop.
//!
//! Connection lifecycle:
//! 1. Read an 8-byte op header.
//! 2. If `OP_REQ_DEVLIST`, write the device list and close.
//! 3. If `OP_REQ_IMPORT`, read the 32-byte busid, open the device, write the
//!    import reply, then enter the URB loop.
//! 4. URB loop: read 48-byte URB headers and dispatch `CMD_SUBMIT` /
//!    `CMD_UNLINK` until the client disconnects.

use anyhow::{Context, Result, anyhow};
use host_mac::{OpenedDevice, SetupPacket, UsbDevice};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};
use usbip_proto::{
    CmdSubmit, CmdUnlink, OP_REQ_DEVLIST, OP_REQ_IMPORT, OpHeader, RetSubmit, URB_HEADER_SIZE,
    USBIP_CMD_SUBMIT, USBIP_CMD_UNLINK, USBIP_DIR_IN, UrbHeader, decode_req_import_busid,
    encode_rep_import_err, encode_rep_import_ok, write_ret_submit, write_ret_unlink,
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
const EPIPE: i32 = -32;
const EOVERFLOW: i32 = -75;

pub fn run(listen: SocketAddr) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(serve(listen))
}

async fn serve(listen: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    info!(%listen, "usbipd listening");

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            res = listener.accept() => {
                match res {
                    Ok((stream, peer)) => {
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(stream, peer).await {
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

async fn handle_client(mut stream: TcpStream, peer: SocketAddr) -> Result<()> {
    debug!(%peer, "client connected");
    let mut header_buf = [0u8; OpHeader::SIZE];
    stream
        .read_exact(&mut header_buf)
        .await
        .with_context(|| format!("read op header from {peer}"))?;
    let header = OpHeader::decode(&header_buf).context("decode op header")?;
    debug!(%peer, version = format!("0x{:04x}", header.version), code = format!("0x{:04x}", header.code), "op header");

    match header.code {
        OP_REQ_DEVLIST => handle_devlist(stream, peer).await,
        OP_REQ_IMPORT => handle_import(stream, peer).await,
        _ => {
            // Reuse the legacy handler for symmetry/logging.
            let devices = host_mac::list_devices().context("enumerate USB devices")?;
            match handle_op(header, &devices)? {
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

async fn handle_devlist(mut stream: TcpStream, peer: SocketAddr) -> Result<()> {
    let devices = host_mac::list_devices().context("enumerate USB devices")?;
    let bytes = encode_rep_devlist(&devices);
    stream
        .write_all(&bytes)
        .await
        .with_context(|| format!("write devlist to {peer}"))?;
    stream.flush().await.ok();
    debug!(%peer, bytes = bytes.len(), "devlist sent");
    Ok(())
}

async fn handle_import(mut stream: TcpStream, peer: SocketAddr) -> Result<()> {
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
        let mut out = Vec::new();
        encode_rep_import_err(&mut out);
        stream.write_all(&out).await.ok();
        return Ok(());
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
            let mut out = Vec::new();
            encode_rep_import_err(&mut out);
            stream.write_all(&out).await.ok();
            return Ok(());
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

    urb_loop(stream, peer, desc, opened).await
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
