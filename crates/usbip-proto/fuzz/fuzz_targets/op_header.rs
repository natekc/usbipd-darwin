#![no_main]
//! Fuzz the 8-byte op-header decoder.
//!
//! Property: `OpHeader::decode` must never panic on arbitrary input.
//! It is allowed to return an error.

use libfuzzer_sys::fuzz_target;
use usbip_proto::OpHeader;

fuzz_target!(|data: &[u8]| {
    let _ = OpHeader::decode(data);
});
