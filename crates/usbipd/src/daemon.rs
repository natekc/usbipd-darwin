//! Tokio TCP adapter around [`usbip_server`].

use anyhow::{Context, Result};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};
use usbip_proto::OpHeader;
use usbip_server::{Reply, handle_op};

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

    // Refresh the device list on every request rather than caching, so that
    // hot-plug works transparently from the client's perspective.
    let devices = host_mac::list_devices().context("enumerate USB devices")?;
    match handle_op(header, &devices)? {
        Reply::Bytes(bytes) => {
            stream
                .write_all(&bytes)
                .await
                .with_context(|| format!("write reply to {peer}"))?;
            stream.flush().await.ok();
            debug!(%peer, bytes = bytes.len(), "reply sent");
        }
        Reply::Unsupported(code) => {
            warn!(%peer, code = format!("0x{code:04x}"), "unsupported op code; closing");
        }
    }
    Ok(())
}
