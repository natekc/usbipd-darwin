//! macOS host-side USB backend.
//!
//! Wraps [`nusb`] to provide device enumeration and (eventually) device claim
//! / URB submission against `IOKit` / `IOUSBHost`.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use nusb::MaybeFuture;
use nusb::transfer::{
    Bulk, ControlIn, ControlOut, ControlType, In, Interrupt, Out, Recipient, TransferError,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HostError {
    #[error("nusb error: {0}")]
    Nusb(#[from] nusb::Error),
    #[error("transfer error: {0}")]
    Transfer(#[from] TransferError),
    #[error("device not found: busid {0}")]
    NotFound(String),
    #[error("endpoint 0x{0:02x} not found in active configuration")]
    EndpointNotFound(u8),
    #[error("unsupported transfer type for endpoint 0x{0:02x}")]
    UnsupportedTransfer(u8),
    #[error("invalid setup packet")]
    InvalidSetup,
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

// ---------------------------------------------------------------------------
// Opened device + transfer wrapper.
// ---------------------------------------------------------------------------

/// Parsed 8-byte USB SETUP packet (little-endian on the wire).
#[derive(Debug, Clone, Copy)]
pub struct SetupPacket {
    pub bm_request_type: u8,
    pub b_request: u8,
    pub w_value: u16,
    pub w_index: u16,
    pub w_length: u16,
}

impl SetupPacket {
    #[must_use]
    pub fn from_bytes(b: [u8; 8]) -> Self {
        Self {
            bm_request_type: b[0],
            b_request: b[1],
            w_value: u16::from_le_bytes([b[2], b[3]]),
            w_index: u16::from_le_bytes([b[4], b[5]]),
            w_length: u16::from_le_bytes([b[6], b[7]]),
        }
    }

    /// `true` if the request is device-to-host (bit 7 of `bmRequestType`).
    #[must_use]
    pub fn is_in(&self) -> bool {
        self.bm_request_type & 0x80 != 0
    }

    fn control_type(self) -> ControlType {
        match (self.bm_request_type >> 5) & 0x3 {
            0 => ControlType::Standard,
            1 => ControlType::Class,
            _ => ControlType::Vendor,
        }
    }

    fn recipient(self) -> Recipient {
        match self.bm_request_type & 0x1F {
            0 => Recipient::Device,
            1 => Recipient::Interface,
            2 => Recipient::Endpoint,
            _ => Recipient::Other,
        }
    }
}

enum AnyEp {
    BulkIn(nusb::Endpoint<Bulk, In>),
    BulkOut(nusb::Endpoint<Bulk, Out>),
    InterruptIn(nusb::Endpoint<Interrupt, In>),
    InterruptOut(nusb::Endpoint<Interrupt, Out>),
}

#[derive(Clone, Copy)]
enum EpKind {
    Bulk,
    Interrupt,
}

/// An opened USB device with lazily-claimed interfaces and lazily-opened
/// endpoints. All transfer methods are blocking; callers should invoke them
/// from a thread that may block (e.g. `tokio::task::spawn_blocking`).
pub struct OpenedDevice {
    busid: String,
    device: nusb::Device,
    interfaces: Mutex<HashMap<u8, nusb::Interface>>,
    /// Endpoint cache keyed by raw endpoint address (`bEndpointAddress`,
    /// including the direction bit).
    endpoints: Mutex<HashMap<u8, AnyEp>>,
}

impl std::fmt::Debug for OpenedDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenedDevice")
            .field("busid", &self.busid)
            .finish_non_exhaustive()
    }
}

impl OpenedDevice {
    /// Open a device by USB/IP busid (e.g. `"01-1"`).
    pub fn open(busid: &str) -> Result<Self, HostError> {
        let info = nusb::list_devices()
            .wait()?
            .find(|i| format_busid(i) == busid)
            .ok_or_else(|| HostError::NotFound(busid.to_owned()))?;
        let device = info.open().wait()?;
        Ok(Self {
            busid: busid.to_owned(),
            device,
            interfaces: Mutex::new(HashMap::new()),
            endpoints: Mutex::new(HashMap::new()),
        })
    }

    pub fn busid(&self) -> &str {
        &self.busid
    }

    /// Issue a control transfer on endpoint 0. `setup` is the raw 8-byte USB
    /// SETUP packet (little-endian, as it appears on the bus). For IN
    /// transfers the returned `Vec` contains the response data; for OUT the
    /// caller-supplied `data` is sent and an empty `Vec` is returned.
    pub fn control_transfer(
        &self,
        setup: SetupPacket,
        out_data: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, HostError> {
        let ct = setup.control_type();
        let rec = setup.recipient();
        if setup.is_in() {
            let req = ControlIn {
                control_type: ct,
                recipient: rec,
                request: setup.b_request,
                value: setup.w_value,
                index: setup.w_index,
                length: setup.w_length,
            };
            let data = self.device.control_in(req, timeout).wait()?;
            Ok(data)
        } else {
            let req = ControlOut {
                control_type: ct,
                recipient: rec,
                request: setup.b_request,
                value: setup.w_value,
                index: setup.w_index,
                data: out_data,
            };
            self.device.control_out(req, timeout).wait()?;
            Ok(Vec::new())
        }
    }

    /// Issue a bulk or interrupt transfer on `ep_addr` (raw address, with
    /// direction bit). For IN transfers `out_data` is ignored and `length`
    /// bytes are returned; for OUT transfers `out_data` is sent and an empty
    /// vec is returned.
    pub fn data_transfer(
        &self,
        ep_addr: u8,
        length: usize,
        out_data: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, HostError> {
        let kind = self.endpoint_kind(ep_addr)?;
        self.ensure_endpoint(ep_addr, kind)?;
        let mut endpoints = self.endpoints.lock().expect("endpoint cache poisoned");
        let ep = endpoints
            .get_mut(&ep_addr)
            .expect("endpoint inserted by ensure_endpoint");
        let buf = if ep_addr & 0x80 != 0 {
            nusb::transfer::Buffer::new(length)
        } else {
            let mut b = nusb::transfer::Buffer::new(out_data.len());
            b.extend_from_slice(out_data);
            b
        };
        let completion = match ep {
            AnyEp::BulkIn(e) => e.transfer_blocking(buf, timeout),
            AnyEp::BulkOut(e) => e.transfer_blocking(buf, timeout),
            AnyEp::InterruptIn(e) => e.transfer_blocking(buf, timeout),
            AnyEp::InterruptOut(e) => e.transfer_blocking(buf, timeout),
        };
        let data = completion.into_result()?;
        if ep_addr & 0x80 != 0 {
            Ok(data.into_vec())
        } else {
            Ok(Vec::new())
        }
    }

    /// Look up which interface owns the given endpoint and what transfer
    /// type it uses, by walking the active configuration descriptors.
    fn endpoint_kind(&self, ep_addr: u8) -> Result<(u8, EpKind), HostError> {
        use nusb::descriptors::TransferType;
        let cfg = self
            .device
            .active_configuration()
            .map_err(|_| HostError::EndpointNotFound(ep_addr))?;
        for iface in cfg.interface_alt_settings() {
            for ep in iface.endpoints() {
                if ep.address() == ep_addr {
                    let kind = match ep.transfer_type() {
                        TransferType::Bulk => EpKind::Bulk,
                        TransferType::Interrupt => EpKind::Interrupt,
                        _ => return Err(HostError::UnsupportedTransfer(ep_addr)),
                    };
                    return Ok((iface.interface_number(), kind));
                }
            }
        }
        Err(HostError::EndpointNotFound(ep_addr))
    }

    fn ensure_endpoint(&self, ep_addr: u8, k: (u8, EpKind)) -> Result<(), HostError> {
        {
            let endpoints = self.endpoints.lock().expect("endpoint cache poisoned");
            if endpoints.contains_key(&ep_addr) {
                return Ok(());
            }
        }
        let (iface_num, kind) = k;
        let iface = self.ensure_interface(iface_num)?;
        let any = match (kind, ep_addr & 0x80 != 0) {
            (EpKind::Bulk, true) => AnyEp::BulkIn(iface.endpoint::<Bulk, In>(ep_addr)?),
            (EpKind::Bulk, false) => AnyEp::BulkOut(iface.endpoint::<Bulk, Out>(ep_addr)?),
            (EpKind::Interrupt, true) => {
                AnyEp::InterruptIn(iface.endpoint::<Interrupt, In>(ep_addr)?)
            }
            (EpKind::Interrupt, false) => {
                AnyEp::InterruptOut(iface.endpoint::<Interrupt, Out>(ep_addr)?)
            }
        };
        let mut endpoints = self.endpoints.lock().expect("endpoint cache poisoned");
        endpoints.entry(ep_addr).or_insert(any);
        Ok(())
    }

    fn ensure_interface(&self, num: u8) -> Result<nusb::Interface, HostError> {
        {
            let ifaces = self.interfaces.lock().expect("interface cache poisoned");
            if let Some(i) = ifaces.get(&num) {
                return Ok(i.clone());
            }
        }
        let iface = self.device.claim_interface(num).wait()?;
        let mut ifaces = self.interfaces.lock().expect("interface cache poisoned");
        Ok(ifaces.entry(num).or_insert(iface).clone())
    }
}
