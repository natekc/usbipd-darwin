//! macOS host-side USB backend.
//!
//! Wraps [`nusb`] to provide device enumeration and (eventually) device claim
//! / URB submission against `IOKit` / `IOUSBHost`.

#![forbid(unsafe_code)]
