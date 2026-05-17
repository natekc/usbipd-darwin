//! USB/IP wire protocol types and codec.
//!
//! Reference: <https://docs.kernel.org/usb/usbip_protocol.html>
//! (mirrored from `tools/usb/usbip/USBIP-PROTOCOL.TXT` in the Linux kernel source).
//!
//! All multi-byte integers on the wire are big-endian ("network byte order").
//! All strings are fixed-width, NUL-padded, ASCII.

#![forbid(unsafe_code)]

pub mod error;

pub use error::ProtoError;
