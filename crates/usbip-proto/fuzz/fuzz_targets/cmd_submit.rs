#![no_main]
//! Fuzz the 40-byte `CmdSubmit` trailer decoder.

use libfuzzer_sys::fuzz_target;
use usbip_proto::CmdSubmit;

fuzz_target!(|data: &[u8]| {
    let _ = CmdSubmit::decode(data);
});
