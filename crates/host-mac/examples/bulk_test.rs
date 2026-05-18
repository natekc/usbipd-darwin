//! Standalone bulk-OUT smoke test for a Cruzer-style mass-storage device.
//!
//! Usage (run as root, after the daemon has captured the device once):
//!     `sudo target/release/examples/bulk_test`
use nusb::MaybeFuture;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let info = nusb::list_devices()
        .wait()?
        .find(|d| d.vendor_id() == 0x0781 && d.product_id() == 0x5530)
        .ok_or("Cruzer 0781:5530 not found")?;
    println!("Found device, opening…");
    let dev = info.open().wait()?;
    let cfg = dev.active_configuration().map(|c| c.configuration_value()).unwrap_or(0);
    println!("Opened. Active cfg = {cfg}");

    // Make sure we are in config 1 (mass-storage class).
    if cfg != 1 {
        println!("Setting configuration 1");
        dev.set_configuration(1).wait()?;
    }

    println!("Claiming interface 0");
    let iface = dev.claim_interface(0).wait()?;
    println!("Setting alt 0");
    iface.set_alt_setting(0).wait()?;

    let mut ep_out = iface.endpoint::<nusb::transfer::Bulk, nusb::transfer::Out>(0x02)?;
    println!("Clearing halt on 0x02");
    ep_out.clear_halt().wait()?;

    // 31-byte CBW: INQUIRY (0x12, length 36).
    let mut cbw = [0u8; 31];
    cbw[0..4].copy_from_slice(b"USBC"); // signature
    cbw[4..8].copy_from_slice(&0xdead_beef_u32.to_le_bytes()); // tag
    cbw[8..12].copy_from_slice(&36_u32.to_le_bytes()); // data length
    cbw[12] = 0x80; // direction = IN
    cbw[13] = 0; // LUN
    cbw[14] = 6; // CB length
    cbw[15] = 0x12; // INQUIRY
    cbw[16] = 0;
    cbw[17] = 0;
    cbw[18] = 0;
    cbw[19] = 36;
    cbw[20] = 0;

    let mut buf = nusb::transfer::Buffer::new(31);
    buf.extend_from_slice(&cbw);
    println!("Submitting CBW (31 bytes) on 0x02 with 5s timeout…");
    let start = std::time::Instant::now();
    let completion = ep_out.transfer_blocking(buf, Duration::from_secs(5));
    println!(
        "CBW result after {:?}: status={:?}, transferred={}",
        start.elapsed(),
        completion.status,
        completion.buffer.len()
    );

    Ok(())
}
