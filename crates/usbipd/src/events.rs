//! `usbipd events` — push-based hotplug stream over a unix-domain socket.
//!
//! # Wire format
//!
//! One JSON object per line (newline-delimited JSON / NDJSON). Two event
//! kinds, distinguished by the `event` discriminator:
//!
//! ```json
//! {"event":"added","busid":"01-1","vendor_id":"1050","product_id":"0407",
//!  "manufacturer":"Yubico","product":"YubiKey OTP+FIDO+CCID","serial":"123",
//!  "class":0,"subclass":0,"protocol":0,"speed":2}
//! {"event":"removed","busid":"01-1"}
//! ```
//!
//! # Subscription model
//!
//! On accept, each new subscriber first receives the current device set
//! as `added` events (so a Lima hostagent starting up after devices were
//! already plugged in still sees them), then live hotplug events.
//!
//! `added` events are **idempotent** on the consumer side: a duplicate
//! for the same busid means "still present, possibly with updated
//! metadata" — not a brand-new arrival. The implementation can emit a
//! duplicate when a connection races with a hotplug event because the
//! snapshot-then-subscribe ordering is reversed (subscribe first,
//! snapshot second) to guarantee that no event is *missed*.
//!
//! # Why a unix socket and not stdout / a TCP port?
//!
//! - Multiple consumers (e.g. multiple Lima instances) can subscribe
//!   independently without needing a fan-out helper.
//! - Filesystem permissions (0600) are the only access control needed.
//! - Stdout would conflate logging with the data stream; a long-running
//!   `usbipd events` is meant to run as a daemon.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, broadcast};
use tracing::{debug, info, warn};

/// One line on the wire. Serde-tagged so consumers can dispatch on `event`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventLine {
    Added(AddedDevice),
    Removed { busid: String },
}

/// Projection of a [`host_mac::UsbDevice`] suitable for the wire.
///
/// Field set is deliberately small and stable: any consumer (e.g. the
/// Lima hostagent USB watcher) should be able to rely on these field
/// names and types not changing without a major version bump. The
/// shape mirrors `JsonDevice` in `main.rs` so that `usbipd list --json`
/// and `usbipd events` agree on every field they have in common.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AddedDevice {
    pub busid: String,
    pub vendor_id: String,
    pub product_id: String,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub speed: u32,
}

impl AddedDevice {
    pub fn from_host(d: &host_mac::UsbDevice) -> Self {
        Self {
            busid: d.busid.clone(),
            vendor_id: format!("{:04x}", d.vendor_id),
            product_id: format!("{:04x}", d.product_id),
            manufacturer: d.manufacturer.clone(),
            product: d.product.clone(),
            serial: d.serial.clone(),
            class: d.class,
            subclass: d.subclass,
            protocol: d.protocol,
            speed: d.speed,
        }
    }
}

/// Encode one event as a single newline-terminated JSON line.
///
/// Public so tests can assert on the on-wire bytes without having to
/// open a socket.
#[must_use]
pub fn encode_line(ev: &EventLine) -> String {
    let mut s = serde_json::to_string(ev).expect("EventLine is always serializable");
    s.push('\n');
    s
}

/// Maps an opaque hotplug `Id` (`nusb::DeviceId` in production) to the
/// busid we previously emitted for it.
///
/// `nusb::HotplugEvent::Disconnected` carries only a `DeviceId`, so we
/// must remember the busid we assigned at `Connected` time to be able
/// to emit a meaningful removal event. Kept purely synchronous and
/// generic over the id type so it can be unit-tested with primitive
/// integers, without spinning up a real USB stack.
#[derive(Debug)]
pub struct IdMap<Id: Eq + Hash> {
    by_id: HashMap<Id, String>,
}

impl<Id: Eq + Hash> Default for IdMap<Id> {
    fn default() -> Self {
        Self {
            by_id: HashMap::new(),
        }
    }
}

impl<Id: Eq + Hash> IdMap<Id> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn note_added(&mut self, id: Id, busid: String) {
        self.by_id.insert(id, busid);
    }

    /// Look up and forget the busid associated with this id.
    ///
    /// Returns `None` for unknown ids (e.g. a `Disconnected` for a
    /// device the daemon never saw `Connected`, which can happen if
    /// the device was unplugged in the window between the initial
    /// `list_devices` snapshot and `watch_devices` taking effect).
    /// Callers must treat unknown removals as a no-op, not an error.
    pub fn note_removed(&mut self, id: &Id) -> Option<String> {
        self.by_id.remove(id)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }
}

/// Fan-out hub shared between the publisher task and the accept loop.
///
/// - The publisher updates the snapshot and `send`s on the broadcast
///   channel.
/// - Each accepted subscriber subscribes to the broadcast channel and
///   reads the snapshot, in that order, then forwards events to its
///   socket until the peer disconnects or the channel is closed.
#[derive(Debug)]
pub struct Hub {
    snapshot: Mutex<HashMap<String, AddedDevice>>,
    tx: broadcast::Sender<EventLine>,
}

impl Hub {
    /// `buffer` is the broadcast channel capacity. Slow subscribers
    /// past this many backlogged events get a `Lagged` notification
    /// and skip ahead; they remain connected and will catch up on the
    /// next event.
    #[must_use]
    pub fn new(buffer: usize) -> Arc<Self> {
        let (tx, _) = broadcast::channel(buffer);
        Arc::new(Self {
            snapshot: Mutex::new(HashMap::new()),
            tx,
        })
    }

    pub async fn publish_added(&self, dev: AddedDevice) {
        self.snapshot
            .lock()
            .await
            .insert(dev.busid.clone(), dev.clone());
        // send() only errors when there are no subscribers, which is a
        // normal steady state (no Lima instance attached).
        let _ = self.tx.send(EventLine::Added(dev));
    }

    pub async fn publish_removed(&self, busid: String) {
        self.snapshot.lock().await.remove(&busid);
        let _ = self.tx.send(EventLine::Removed { busid });
    }

    /// Current device set as `added` events. Order is unspecified.
    pub async fn snapshot_added(&self) -> Vec<EventLine> {
        self.snapshot
            .lock()
            .await
            .values()
            .cloned()
            .map(EventLine::Added)
            .collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EventLine> {
        self.tx.subscribe()
    }
}

/// Bind a unix-domain listener at `path`, replacing any pre-existing
/// socket file there. Sets the mode to 0600 so only the owning user
/// can connect.
///
/// Refuses to overwrite a non-socket file at `path` — that almost
/// certainly indicates a typo and would silently destroy user data.
pub fn bind_listener(path: &Path) -> Result<UnixListener> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            anyhow::bail!(
                "{} exists and is not a socket; refusing to overwrite",
                path.display()
            );
        }
        let _ = std::fs::remove_file(path);
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("bind unix:{}", path.display()))?;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        warn!(error = %e, "could not chmod 0600 on events socket; access will fall back to umask");
    }
    Ok(listener)
}

/// Accept connections until `shutdown` resolves, spawning a per-client
/// task for each accepted stream.
///
/// Generic over the shutdown future so tests can drive it with a
/// `oneshot` channel rather than waiting on a real signal.
pub async fn accept_loop<F>(listener: UnixListener, hub: Arc<Hub>, shutdown: F)
where
    F: std::future::Future<Output = ()> + Unpin,
{
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => {
                debug!("events accept loop shutting down");
                return;
            }
            res = listener.accept() => match res {
                Ok((stream, _addr)) => {
                    // Subscribe first, snapshot second. The reverse
                    // ordering would create a window in which a hotplug
                    // event arrives after the snapshot but before the
                    // subscription, and the subscriber would miss it.
                    // This ordering can instead produce a duplicate
                    // `added` for the same device, which consumers
                    // already handle as idempotent.
                    let rx = hub.subscribe();
                    let initial = hub.snapshot_added().await;
                    tokio::spawn(serve_subscriber(stream, initial, rx));
                }
                Err(e) => warn!(error = %e, "events accept failed"),
            }
        }
    }
}

async fn serve_subscriber(
    mut stream: UnixStream,
    initial: Vec<EventLine>,
    mut rx: broadcast::Receiver<EventLine>,
) {
    for ev in initial {
        if write_line(&mut stream, &ev).await.is_err() {
            return;
        }
    }
    loop {
        match rx.recv().await {
            Ok(ev) => {
                if write_line(&mut stream, &ev).await.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(skipped = n, "events subscriber lagged");
                // Keep the connection open. The subscriber will resume
                // with whatever event lands next; consumers that need
                // perfect state should reconnect on `Lagged` (the
                // snapshot replay then re-establishes truth).
            }
        }
    }
}

async fn write_line(stream: &mut UnixStream, ev: &EventLine) -> std::io::Result<()> {
    stream.write_all(encode_line(ev).as_bytes()).await
}

/// Synchronous entry point invoked from `main`. Builds a tokio runtime,
/// seeds the hub from currently-attached devices, then drives the hub
/// from `nusb::watch_devices()` until SIGINT / SIGTERM.
pub fn run(socket: PathBuf) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(run_async(socket))
}

async fn run_async(socket: PathBuf) -> Result<()> {
    use nusb::MaybeFuture;

    let listener = bind_listener(&socket)?;
    info!(path = %socket.display(), "events socket ready");

    let hub = Hub::new(256);

    // Start the watch *before* the snapshot so any device unplugged in
    // between the two calls is delivered as a `Disconnected` event, not
    // silently lost.
    let mut watch = nusb::watch_devices().context("nusb::watch_devices")?;

    let mut idmap: IdMap<nusb::DeviceId> = IdMap::new();
    for info in nusb::list_devices()
        .wait()
        .context("initial list_devices")?
    {
        let id = info.id();
        let dev = host_mac::UsbDevice::from_info(&info);
        let added = AddedDevice::from_host(&dev);
        idmap.note_added(id, added.busid.clone());
        hub.publish_added(added).await;
    }

    let hub_for_pub = Arc::clone(&hub);
    let publisher = tokio::spawn(async move {
        use futures_core::Stream;
        use std::future::poll_fn;
        use std::pin::Pin;
        loop {
            let next = poll_fn(|cx| Pin::new(&mut watch).poll_next(cx)).await;
            match next {
                Some(nusb::hotplug::HotplugEvent::Connected(info)) => {
                    let id = info.id();
                    let dev = host_mac::UsbDevice::from_info(&info);
                    let added = AddedDevice::from_host(&dev);
                    idmap.note_added(id, added.busid.clone());
                    hub_for_pub.publish_added(added).await;
                }
                Some(nusb::hotplug::HotplugEvent::Disconnected(id)) => {
                    if let Some(busid) = idmap.note_removed(&id) {
                        hub_for_pub.publish_removed(busid).await;
                    }
                }
                None => return,
            }
        }
    });

    let shutdown = Box::pin(async {
        let _ = tokio::signal::ctrl_c().await;
    });
    accept_loop(listener, Arc::clone(&hub), shutdown).await;

    publisher.abort();
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    fn sample_added(busid: &str) -> AddedDevice {
        AddedDevice {
            busid: busid.into(),
            vendor_id: "1050".into(),
            product_id: "0407".into(),
            manufacturer: Some("Yubico".into()),
            product: Some("YubiKey".into()),
            serial: Some("SN1".into()),
            class: 0,
            subclass: 0,
            protocol: 0,
            speed: 2,
        }
    }

    /// Asserts the documented wire schema. Specifically: the enum tag
    /// lives in `event`, the discriminator values are `added` and
    /// `removed`, lines end in `\n`, and `vendor_id` / `product_id`
    /// are lowercase 4-digit hex strings (consumers parse them as
    /// strings, not numbers — leading zeros matter).
    #[test]
    fn wire_format_matches_documented_schema() {
        let added_line = encode_line(&EventLine::Added(AddedDevice {
            vendor_id: "0001".into(),
            product_id: "00ff".into(),
            ..sample_added("01-1")
        }));
        assert!(
            added_line.ends_with('\n'),
            "lines must be newline-terminated"
        );
        let parsed: serde_json::Value = serde_json::from_str(added_line.trim_end()).unwrap();
        assert_eq!(parsed["event"], "added");
        assert_eq!(parsed["busid"], "01-1");
        assert_eq!(
            parsed["vendor_id"], "0001",
            "leading-zero hex must be preserved"
        );
        assert_eq!(parsed["product_id"], "00ff");
        assert!(
            parsed.get("manufacturer").is_some(),
            "manufacturer key present even when null"
        );

        let removed_line = encode_line(&EventLine::Removed {
            busid: "02-3".into(),
        });
        let parsed: serde_json::Value = serde_json::from_str(removed_line.trim_end()).unwrap();
        assert_eq!(parsed["event"], "removed");
        assert_eq!(parsed["busid"], "02-3");
        assert!(
            parsed.get("vendor_id").is_none(),
            "removed events must not carry vendor metadata"
        );
    }

    #[test]
    fn idmap_roundtrip_known_and_unknown() {
        let mut m: IdMap<u32> = IdMap::new();
        m.note_added(7, "01-1".into());
        m.note_added(9, "02-1".into());
        assert_eq!(m.len(), 2);
        assert_eq!(m.note_removed(&7).as_deref(), Some("01-1"));
        assert_eq!(m.len(), 1);
        // Same id again: drops to None (we forgot it on first removal).
        assert!(m.note_removed(&7).is_none());
        // Unknown id: None, not panic.
        assert!(m.note_removed(&12345).is_none());
        // Other entry still there.
        assert_eq!(m.note_removed(&9).as_deref(), Some("02-1"));
    }

    /// End-to-end exercise of `Hub` + `accept_loop` + `serve_subscriber`
    /// without any USB hardware: drive the hub directly and observe the
    /// bytes on the socket.
    #[tokio::test]
    async fn subscribers_get_snapshot_then_live_events() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("events.sock");
        let listener = bind_listener(&sock_path).unwrap();

        // Snapshot must be applied *before* the listener accepts, since
        // a new subscriber reads the snapshot at accept time. Pre-seed.
        let hub = Hub::new(16);
        hub.publish_added(sample_added("01-1")).await;

        let (sd_tx, sd_rx) = oneshot::channel::<()>();
        let hub_for_loop = Arc::clone(&hub);
        let loop_task = tokio::spawn(async move {
            accept_loop(
                listener,
                hub_for_loop,
                Box::pin(async move {
                    let _ = sd_rx.await;
                }),
            )
            .await;
        });

        // First subscriber: should see the pre-seeded device.
        let stream = UnixStream::connect(&sock_path).await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(v["event"], "added");
        assert_eq!(v["busid"], "01-1");

        // Live event after the subscriber is established.
        hub.publish_added(sample_added("02-1")).await;
        line.clear();
        timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(v["event"], "added");
        assert_eq!(v["busid"], "02-1");

        // Removal of the first device.
        hub.publish_removed("01-1".into()).await;
        line.clear();
        timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(v["event"], "removed");
        assert_eq!(v["busid"], "01-1");

        // Second subscriber should see the *current* snapshot (just 02-1),
        // not the already-removed 01-1.
        let stream2 = UnixStream::connect(&sock_path).await.unwrap();
        let mut reader2 = BufReader::new(stream2);
        let mut line2 = String::new();
        timeout(Duration::from_secs(2), reader2.read_line(&mut line2))
            .await
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(line2.trim_end()).unwrap();
        assert_eq!(v["event"], "added");
        assert_eq!(
            v["busid"], "02-1",
            "second subscriber's snapshot must reflect prior removal"
        );

        let _ = sd_tx.send(());
        let _ = timeout(Duration::from_secs(2), loop_task).await;
    }

    /// Disconnecting one subscriber must not kill the publisher or
    /// affect other subscribers — a real Lima instance crash should
    /// be transparent to other consumers.
    #[tokio::test]
    async fn subscriber_disconnect_does_not_affect_publisher() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("events.sock");
        let listener = bind_listener(&sock_path).unwrap();
        let hub = Hub::new(16);

        let (sd_tx, sd_rx) = oneshot::channel::<()>();
        let hub_for_loop = Arc::clone(&hub);
        let loop_task = tokio::spawn(async move {
            accept_loop(
                listener,
                hub_for_loop,
                Box::pin(async move {
                    let _ = sd_rx.await;
                }),
            )
            .await;
        });

        let a = UnixStream::connect(&sock_path).await.unwrap();
        let b = UnixStream::connect(&sock_path).await.unwrap();
        // Give the accept loop a moment to register both subscriptions.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Drop A.
        drop(a);

        // Publish — B must still receive.
        hub.publish_added(sample_added("03-1")).await;
        let mut reader = BufReader::new(b);
        let mut line = String::new();
        timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(v["busid"], "03-1");

        let _ = sd_tx.send(());
        let _ = timeout(Duration::from_secs(2), loop_task).await;
    }

    #[tokio::test]
    async fn bind_listener_sets_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("events.sock");
        let _listener = bind_listener(&sock_path).unwrap();
        let mode = std::fs::metadata(&sock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "events socket must be owner-only (got {mode:o})"
        );
    }

    #[tokio::test]
    async fn bind_listener_refuses_to_overwrite_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-socket");
        std::fs::write(&path, b"important user data").unwrap();
        let err = bind_listener(&path).unwrap_err();
        assert!(
            err.to_string().contains("not a socket"),
            "expected refusal message, got: {err}",
        );
        // File must still be intact.
        assert_eq!(std::fs::read(&path).unwrap(), b"important user data");
    }

    #[tokio::test]
    async fn bind_listener_replaces_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sock");
        // Simulate a stale socket left by a previous daemon crash.
        let stale = UnixListener::bind(&path).unwrap();
        drop(stale);
        // Should rebind without complaint.
        let _fresh = bind_listener(&path).unwrap();
    }
}
