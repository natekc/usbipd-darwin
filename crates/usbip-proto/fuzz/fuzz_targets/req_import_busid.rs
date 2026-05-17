#![no_main]
//! Fuzz the 32-byte busid string extractor from `OP_REQ_IMPORT`.

use libfuzzer_sys::fuzz_target;
use usbip_proto::decode_req_import_busid;

fuzz_target!(|data: &[u8]| {
    let _ = decode_req_import_busid(data);
});
