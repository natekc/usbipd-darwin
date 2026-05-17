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

/// A single USB interface from the device's active configuration.
#[derive(Debug, Clone, Copy)]
pub struct UsbInterface {
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

/// A USB device visible to the host, in a form suitable for advertising
/// over USB/IP and for display to the user.
#[derive(Debug, Clone)]
pub struct UsbDevice {
    /// USB/IP-style busid, e.g. `"1-2"` or `"1-2.3"`. Derived from the host
    /// bus identifier plus the port chain.
    pub busid: String,

    /// Synthesized USB/IP bus number. Stable per-process for a given busid.
    pub busnum: u32,
    /// Synthesized USB/IP device number. Stable per-process for a given busid.
    pub devnum: u32,

    /// USB/IP speed enum value (1=low .. 6=super-plus, 0=unknown).
    pub speed: u32,

    pub vendor_id: u16,
    pub product_id: u16,
    pub bcd_device: u16,

    /// USB device class / subclass / protocol from the device descriptor.
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,

    /// `bConfigurationValue` of the currently active configuration, or 0
    /// if the device is not configured.
    pub configuration_value: u8,
    /// `bNumConfigurations`. nusb does not expose this directly without
    /// opening the device, so we report 1 as a sane default (the active
    /// config we can see).
    pub num_configurations: u8,

    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,

    pub interfaces: Vec<UsbInterface>,
}

/// Enumerate USB devices visible to this process.
///
/// Does not require elevated privileges — descriptor reads go through `IOKit`
/// without claiming the device.
pub fn list_devices() -> Result<Vec<UsbDevice>, HostError> {
    let mut out = Vec::new();
    for (idx, info) in nusb::list_devices().wait()?.enumerate() {
        // Synthesize stable per-process bus/dev numbers. The exact values
        // don't matter to USB/IP clients as long as they round-trip; we
        // pick (1, idx+1) so devnum starts at 1.
        let busnum = 1;
        let devnum = u32::try_from(idx).unwrap_or(0) + 1;
        out.push(UsbDevice::from_nusb(&info, busnum, devnum));
    }
    Ok(out)
}

impl UsbDevice {
    fn from_nusb(info: &nusb::DeviceInfo, busnum: u32, devnum: u32) -> Self {
        let interfaces = info
            .interfaces()
            .map(|i| UsbInterface {
                class: i.class(),
                subclass: i.subclass(),
                protocol: i.protocol(),
            })
            .collect();
        Self {
            busid: format_busid(info),
            busnum,
            devnum,
            speed: speed_to_usbip(info.speed()),
            vendor_id: info.vendor_id(),
            product_id: info.product_id(),
            bcd_device: info.device_version(),
            class: info.class(),
            subclass: info.subclass(),
            protocol: info.protocol(),
            // active_configuration is only available after opening the device.
            // For listing (MVP-3) we default to 1, which is the canonical value
            // for single-config devices and matches what the Linux usbipd does
            // for typical mass-storage / HID / etc. hardware.
            configuration_value: 1,
            num_configurations: 1,
            manufacturer: info.manufacturer_string().map(str::to_owned),
            product: info.product_string().map(str::to_owned),
            serial: info.serial_number().map(str::to_owned),
            interfaces,
        }
    }
}

/// Map a [`nusb::Speed`] to the integer value the USB/IP wire protocol uses.
fn speed_to_usbip(speed: Option<nusb::Speed>) -> u32 {
    match speed {
        Some(nusb::Speed::Low) => 1,
        Some(nusb::Speed::Full) => 2,
        Some(nusb::Speed::High) => 3,
        Some(nusb::Speed::Super) => 5,
        Some(nusb::Speed::SuperPlus) => 6,
        _ => 0,
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
