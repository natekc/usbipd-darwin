#![no_main]
//! Fuzz the 20-byte basic URB header decoder.

use libfuzzer_sys::fuzz_target;
use usbip_proto::UrbHeader;

fuzz_target!(|data: &[u8]| {
    let _ = UrbHeader::decode(data);
});
