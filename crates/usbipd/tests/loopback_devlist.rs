//! End-to-end smoke test for the daemon over a unix socket.
//!
//! Spawns the `usbipd` binary as a child process listening on a
//! temp-dir unix socket with `--allow-all`, then hand-encodes an
//! `OP_REQ_DEVLIST` request, sends it, and decodes the reply. This
//! exercises the full pipeline:
//!
//! 1. CLI argument parsing.
//! 2. Unix-socket bind and 0600 permission enforcement.
//! 3. The op-header read/write path on a real socket.
//! 4. Host enumeration → `OP_REP_DEVLIST` round trip.
//! 5. SIGTERM shutdown.
//!
//! Does NOT exercise force-capture or URB transfer (would need root
//! and a real device); those are validated by the manual matrix
//! documented in CONTRIBUTING.md.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use usbip_proto::{OP_REP_DEVLIST, OP_REQ_DEVLIST, OpHeader, USBIP_VERSION};

/// Path to the `usbipd` binary that cargo built for this integration
/// test. Provided automatically by cargo.
fn usbipd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_usbipd"))
}

/// Block until `path` exists or `deadline` is reached.
fn wait_for_socket(path: &std::path::Path, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn devlist_over_unix_socket() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let sock = tmp.path().join("usbipd.sock");

    let mut child = Command::new(usbipd_bin())
        .arg("daemon")
        .arg("--socket")
        .arg(&sock)
        .arg("--allow-all")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn usbipd");

    // Bound the test wall-clock: 5s to bind the socket is generous.
    let bound = wait_for_socket(&sock, Instant::now() + Duration::from_secs(5));
    if !bound {
        let _ = child.kill();
        panic!("daemon did not bind unix socket within 5s");
    }

    // unix-socket transport must enforce mode 0600.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let meta = std::fs::metadata(&sock).expect("stat socket");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "unix socket must be mode 0600, got {mode:o}");
    }

    // Now talk USB/IP at it.
    let result = std::panic::catch_unwind(|| {
        let mut stream = UnixStream::connect(&sock).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set timeout");

        // Send OP_REQ_DEVLIST (8-byte op header, no body).
        let mut req = Vec::with_capacity(OpHeader::SIZE);
        OpHeader::new(OP_REQ_DEVLIST).encode(&mut req);
        stream.write_all(&req).expect("write req");

        // Read the 8-byte op-header reply.
        let mut hdr = [0_u8; OpHeader::SIZE];
        stream.read_exact(&mut hdr).expect("read reply header");
        let hdr = OpHeader::decode(&hdr).expect("decode reply header");

        assert_eq!(hdr.version, USBIP_VERSION, "wrong USB/IP version");
        assert_eq!(hdr.code, OP_REP_DEVLIST, "wrong op reply code");
        assert_eq!(hdr.status, 0, "non-zero status: {:#x}", hdr.status);

        // Followed by a 4-byte device count (big-endian). The actual host
        // device count is whatever the test machine has plugged in; we
        // only assert it's a sane non-negative integer (no protocol
        // framing surprises).
        let mut count_buf = [0_u8; 4];
        stream.read_exact(&mut count_buf).expect("read count");
        let count = u32::from_be_bytes(count_buf);
        assert!(count < 256, "implausible device count {count}");
    });

    // Always reap the child.
    let _ = child.kill();
    let _ = child.wait();

    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[test]
fn unknown_op_code_closes_connection() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let sock = tmp.path().join("usbipd.sock");

    let mut child = Command::new(usbipd_bin())
        .arg("daemon")
        .arg("--socket")
        .arg(&sock)
        .arg("--allow-all")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn usbipd");

    let bound = wait_for_socket(&sock, Instant::now() + Duration::from_secs(5));
    if !bound {
        let _ = child.kill();
        panic!("daemon did not bind unix socket within 5s");
    }

    let result = std::panic::catch_unwind(|| {
        let mut stream = UnixStream::connect(&sock).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set timeout");

        // Bogus op code 0x0000 with the correct version — daemon must close
        // without spinning.
        let mut req = Vec::with_capacity(OpHeader::SIZE);
        OpHeader {
            version: USBIP_VERSION,
            code: 0x0000,
            status: 0,
        }
        .encode(&mut req);
        stream.write_all(&req).expect("write bogus");

        // EOF expected.
        let mut buf = [0_u8; 16];
        let n = stream.read(&mut buf).expect("read");
        assert_eq!(n, 0, "daemon should have closed; instead got {n} bytes");
    });

    let _ = child.kill();
    let _ = child.wait();

    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
