//! End-to-end smoke test using the host-mac wrapper (which does
//! `OpenSeize+ReEnumerate(Capture)` before opening).
use host_mac::{OpenedDevice, SetupPacket};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("debug")
        .with_target(true)
        .init();

    let busid = std::env::args().nth(1).unwrap_or_else(|| "00-1".to_string());
    eprintln!("opening busid={busid}");
    let dev = OpenedDevice::open(&busid)?;
    eprintln!("opened, intercepting SET_CONFIGURATION(1)");

    // Force SET_CONFIGURATION(1) via our control path so the cache-invalidation
    // logic runs exactly the same way as the daemon.
    let _ = dev.control_transfer(
        SetupPacket {
            bm_request_type: 0x00,
            b_request: 0x09,
            w_value: 1,
            w_index: 0,
            w_length: 0,
        },
        &[],
        Duration::from_secs(2),
    )?;
    eprintln!("submitting INQUIRY CBW (31 bytes) on bulk-OUT 0x02");

    let mut cbw = [0u8; 31];
    cbw[0..4].copy_from_slice(b"USBC");
    cbw[4..8].copy_from_slice(&0xdead_beef_u32.to_le_bytes());
    cbw[8..12].copy_from_slice(&36_u32.to_le_bytes());
    cbw[12] = 0x80;
    cbw[13] = 0;
    cbw[14] = 6;
    cbw[15] = 0x12;
    cbw[19] = 36;

    let start = std::time::Instant::now();
    let res = dev.data_transfer(0x02, 31, &cbw, Duration::from_secs(5));
    eprintln!("CBW result after {:?}: {:?}", start.elapsed(), res.map(|v| v.data.len()));
    Ok(())
}
