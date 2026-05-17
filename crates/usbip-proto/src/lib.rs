//! USB/IP wire protocol types and codec.
//!
//! Reference: <https://docs.kernel.org/usb/usbip_protocol.html>
//! (mirrored from `tools/usb/usbip/USBIP-PROTOCOL.TXT` in the Linux kernel source).
//!
//! All multi-byte integers on the wire are big-endian ("network byte order").
//! All strings are fixed-width, NUL-padded, ASCII.

#![forbid(unsafe_code)]

pub mod error;
pub mod op;

pub use error::ProtoError;
pub use op::{
    ExportedDevice, ExportedInterface, OP_REP_DEVLIST, OP_REP_IMPORT, OP_REQ_DEVLIST,
    OP_REQ_IMPORT, OpHeader, USBIP_VERSION,
};
