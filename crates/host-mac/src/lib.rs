//! macOS host-side USB backend.
//!
//! Wraps [`nusb`] to provide device enumeration and (eventually) device claim
//! / URB submission against `IOKit` / `IOUSBHost`.

#![forbid(unsafe_code)]

use nusb::MaybeFuture;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HostError {
    #[error("nusb error: {0}")]
    Nusb(#[from] nusb::Error),
}

/// A USB device visible to the host, in a form suitable for advertising
/// over USB/IP and for display to the user.
#[derive(Debug, Clone)]
pub struct UsbDevice {
    /// USB/IP-style busid, e.g. `"1-2"` or `"1-2.3"`. Derived from the host
    /// bus identifier plus the port chain.
    pub busid: String,

    pub vendor_id: u16,
    pub product_id: u16,

    /// USB device class / subclass / protocol from the device descriptor.
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,

    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
}

/// Enumerate USB devices visible to this process.
///
/// Does not require elevated privileges — descriptor reads go through `IOKit`
/// without claiming the device.
pub fn list_devices() -> Result<Vec<UsbDevice>, HostError> {
    let mut out = Vec::new();
    for info in nusb::list_devices().wait()? {
        out.push(UsbDevice::from(&info));
    }
    Ok(out)
}

impl From<&nusb::DeviceInfo> for UsbDevice {
    fn from(info: &nusb::DeviceInfo) -> Self {
        Self {
            busid: format_busid(info),
            vendor_id: info.vendor_id(),
            product_id: info.product_id(),
            class: info.class(),
            subclass: info.subclass(),
            protocol: info.protocol(),
            manufacturer: info.manufacturer_string().map(str::to_owned),
            product: info.product_string().map(str::to_owned),
            serial: info.serial_number().map(str::to_owned),
        }
    }
}

/// Format a USB/IP-style busid from a nusb `DeviceInfo`.
///
/// USB/IP uses `<bus>-<port>[.<port>...]`. We reuse the host bus id as the
/// bus token and join the port chain with `.`.
fn format_busid(info: &nusb::DeviceInfo) -> String {
    let ports: Vec<String> = info.port_chain().iter().map(ToString::to_string).collect();
    let bus = info.bus_id();
    if ports.is_empty() {
        // Some devices (root hubs) may have no port chain; fall back to the
        // device address so the busid is still unique within the bus.
        format!("{bus}-0.{}", info.device_address())
    } else {
        format!("{bus}-{}", ports.join("."))
    }
}
