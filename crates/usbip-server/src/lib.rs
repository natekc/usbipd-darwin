//! USB/IP server state machine.
//!
//! Transport-agnostic: callers feed bytes in and pull bytes out. The TCP
//! adapter lives in the `usbipd` binary crate.

#![forbid(unsafe_code)]
