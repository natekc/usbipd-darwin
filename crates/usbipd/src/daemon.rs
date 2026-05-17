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
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
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

    // Send success import reply.
    let mut out = Vec::with_capacity(8 + 312);
    encode_rep_import_ok(&mut out, &to_exported(&desc));
    stream.write_all(&out).await?;

    urb_loop(stream, peer, desc, opened).await
}

async fn urb_loop(
    mut stream: TcpStream,
    peer: SocketAddr,
    desc: UsbDevice,
    opened: Arc<OpenedDevice>,
) -> Result<()> {
    let devid = (desc.busnum << 16) | desc.devnum;
    let mut header_buf = [0u8; URB_HEADER_SIZE];
    loop {
        // EOF here means clean client disconnect.
        if let Err(e) = stream.read_exact(&mut header_buf).await {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                debug!(%peer, "client disconnected");
                return Ok(());
            }
            return Err(e).context("read URB header");
        }
        let basic = UrbHeader::decode(&header_buf[0..UrbHeader::SIZE])?;
        if basic.devid != devid {
            warn!(
                %peer,
                got = format!("0x{:08x}", basic.devid),
                expected = format!("0x{devid:08x}"),
                "URB devid mismatch; ignoring"
            );
        }

        match basic.command {
            USBIP_CMD_SUBMIT => {
                let cmd = CmdSubmit::decode(&header_buf[UrbHeader::SIZE..URB_HEADER_SIZE])?;
                handle_submit(&mut stream, &opened, basic, cmd).await?;
            }
            USBIP_CMD_UNLINK => {
                let unlink = CmdUnlink::decode(&header_buf[UrbHeader::SIZE..URB_HEADER_SIZE])?;
                debug!(
                    %peer,
                    seqnum = basic.seqnum,
                    unlink_seqnum = unlink.unlink_seqnum,
                    "CMD_UNLINK (no-op, transfers are synchronous)"
                );
                let mut out = Vec::with_capacity(URB_HEADER_SIZE);
                write_ret_unlink(&mut out, basic.seqnum, 0);
                stream.write_all(&out).await?;
            }
            other => {
                return Err(anyhow!(
                    "unknown URB command 0x{other:08x}; closing connection"
                ));
            }
        }
    }
}

async fn handle_submit(
    stream: &mut TcpStream,
    opened: &Arc<OpenedDevice>,
    basic: UrbHeader,
    cmd: CmdSubmit,
) -> Result<()> {
    let dir_in = basic.direction == USBIP_DIR_IN;
    let tbl = usize::try_from(cmd.transfer_buffer_length).unwrap_or(0);

    // For OUT transfers the client sends the payload right after the header.
    let mut out_payload = Vec::new();
    if !dir_in && tbl > 0 {
        out_payload.resize(tbl, 0);
        stream.read_exact(&mut out_payload).await?;
    }

    let opened = opened.clone();
    let ep = basic.ep;
    let seqnum = basic.seqnum;

    // Run the actual nusb call on a blocking thread.
    let result = tokio::task::spawn_blocking(move || {
        if ep == 0 {
            let setup = SetupPacket::from_bytes(cmd.setup);
            opened.control_transfer(setup, &out_payload, URB_TIMEOUT)
        } else {
            let ep_addr = u8::try_from(ep & 0xF).unwrap_or(0) | if dir_in { 0x80 } else { 0x00 };
            opened.data_transfer(ep_addr, tbl, &out_payload, URB_TIMEOUT)
        }
    })
    .await
    .context("join transfer task")?;

    let (status, payload) = match result {
        Ok(data) => {
            if dir_in {
                // Mass-storage and most other classes treat a short read as
                // success — actual_length reflects the real byte count.
                if data.len() > tbl {
                    // Defensive: refuse to ship more than the client asked.
                    (EOVERFLOW, Vec::new())
                } else {
                    (0, data)
                }
            } else {
                (0, Vec::new())
            }
        }
        Err(e) => {
            warn!(seqnum, ep, error = %e, "transfer failed");
            (EPIPE, Vec::new())
        }
    };

    let actual_length = i32::try_from(payload.len()).unwrap_or(i32::MAX);
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
    stream.write_all(&out).await?;
    Ok(())
}
