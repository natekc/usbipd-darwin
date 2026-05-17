//! USB/IP op-message types: header, `REQ_DEVLIST` / `REP_DEVLIST`, `REQ_IMPORT` / `REP_IMPORT`.
//!
//! Wire layout follows the Linux kernel implementation; field sizes and
//! offsets are derived from `tools/usb/usbip/src/usbipd.c` and
//! `drivers/usb/usbip/usbip_common.h`.

use crate::ProtoError;

/// USB/IP protocol version emitted by current Linux usbip tooling.
pub const USBIP_VERSION: u16 = 0x0111;

pub const OP_REQ_DEVLIST: u16 = 0x8005;
pub const OP_REP_DEVLIST: u16 = 0x0005;
pub const OP_REQ_IMPORT: u16 = 0x8003;
pub const OP_REP_IMPORT: u16 = 0x0003;

/// Common 8-byte op-message header, big-endian on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpHeader {
    pub version: u16,
    pub code: u16,
    pub status: u32,
}

impl OpHeader {
    pub const SIZE: usize = 8;

    #[must_use]
    pub fn new(code: u16) -> Self {
        Self {
            version: USBIP_VERSION,
            code,
            status: 0,
        }
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(&self.code.to_be_bytes());
        out.extend_from_slice(&self.status.to_be_bytes());
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < Self::SIZE {
            return Err(ProtoError::ShortRead {
                need: Self::SIZE,
                got: buf.len(),
            });
        }
        Ok(Self {
            version: u16::from_be_bytes([buf[0], buf[1]]),
            code: u16::from_be_bytes([buf[2], buf[3]]),
            status: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
        })
    }
}

/// A single interface descriptor as exposed by `OP_REP_DEVLIST`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportedInterface {
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

impl ExportedInterface {
    pub const SIZE: usize = 4;

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.class);
        out.push(self.subclass);
        out.push(self.protocol);
        out.push(0); // padding
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < Self::SIZE {
            return Err(ProtoError::ShortRead {
                need: Self::SIZE,
                got: buf.len(),
            });
        }
        Ok(Self {
            class: buf[0],
            subclass: buf[1],
            protocol: buf[2],
            // padding at buf[3]
        })
    }
}

/// One exported device record inside `OP_REP_DEVLIST`. Wire size is
/// [`Self::HEADER_SIZE`] plus `num_interfaces * ExportedInterface::SIZE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedDevice {
    /// "sysfs-style" path, NUL-padded to 256 bytes on the wire. The Linux
    /// daemon emits something like `/sys/devices/.../1-2`; for our purposes
    /// any unique stable string is sufficient.
    pub path: String,
    /// Bus identifier, e.g. `"1-2"`. NUL-padded to 32 bytes on the wire.
    pub busid: String,

    pub busnum: u32,
    pub devnum: u32,
    pub speed: u32,

    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,

    pub b_device_class: u8,
    pub b_device_subclass: u8,
    pub b_device_protocol: u8,
    pub b_configuration_value: u8,
    pub b_num_configurations: u8,

    pub interfaces: Vec<ExportedInterface>,
}

impl ExportedDevice {
    /// Size of the fixed device-record fields (path + busid + ints), before
    /// the variable-length list of interfaces.
    pub const HEADER_SIZE: usize = 256 + 32 + 4 + 4 + 4 + 2 + 2 + 2 + 1 + 1 + 1 + 1 + 1 + 1;

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_fixed_str(out, &self.path, 256);
        write_fixed_str(out, &self.busid, 32);
        out.extend_from_slice(&self.busnum.to_be_bytes());
        out.extend_from_slice(&self.devnum.to_be_bytes());
        out.extend_from_slice(&self.speed.to_be_bytes());
        out.extend_from_slice(&self.id_vendor.to_be_bytes());
        out.extend_from_slice(&self.id_product.to_be_bytes());
        out.extend_from_slice(&self.bcd_device.to_be_bytes());
        out.push(self.b_device_class);
        out.push(self.b_device_subclass);
        out.push(self.b_device_protocol);
        out.push(self.b_configuration_value);
        out.push(self.b_num_configurations);
        let n_ifaces = u8::try_from(self.interfaces.len()).unwrap_or(u8::MAX);
        out.push(n_ifaces);
        for iface in &self.interfaces {
            iface.encode(out);
        }
    }

    pub fn decode(buf: &[u8]) -> Result<(Self, usize), ProtoError> {
        if buf.len() < Self::HEADER_SIZE {
            return Err(ProtoError::ShortRead {
                need: Self::HEADER_SIZE,
                got: buf.len(),
            });
        }
        let path = read_fixed_str(&buf[0..256]);
        let busid = read_fixed_str(&buf[256..288]);
        let mut o = 288;
        let busnum = read_u32(buf, &mut o);
        let devnum = read_u32(buf, &mut o);
        let speed = read_u32(buf, &mut o);
        let id_vendor = read_u16(buf, &mut o);
        let id_product = read_u16(buf, &mut o);
        let bcd_device = read_u16(buf, &mut o);
        let b_device_class = buf[o];
        let b_device_subclass = buf[o + 1];
        let b_device_protocol = buf[o + 2];
        let b_configuration_value = buf[o + 3];
        let b_num_configurations = buf[o + 4];
        let n_ifaces = buf[o + 5] as usize;
        o += 6;
        let need = n_ifaces * ExportedInterface::SIZE;
        if buf.len() < o + need {
            return Err(ProtoError::ShortRead {
                need: o + need,
                got: buf.len(),
            });
        }
        let mut interfaces = Vec::with_capacity(n_ifaces);
        for _ in 0..n_ifaces {
            interfaces.push(ExportedInterface::decode(
                &buf[o..o + ExportedInterface::SIZE],
            )?);
            o += ExportedInterface::SIZE;
        }
        Ok((
            Self {
                path,
                busid,
                busnum,
                devnum,
                speed,
                id_vendor,
                id_product,
                bcd_device,
                b_device_class,
                b_device_subclass,
                b_device_protocol,
                b_configuration_value,
                b_num_configurations,
                interfaces,
            },
            o,
        ))
    }
}

fn write_fixed_str(out: &mut Vec<u8>, s: &str, width: usize) {
    let bytes = s.as_bytes();
    let take = bytes.len().min(width);
    out.extend_from_slice(&bytes[..take]);
    out.extend(std::iter::repeat_n(0u8, width - take));
}

fn read_fixed_str(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn read_u16(buf: &[u8], o: &mut usize) -> u16 {
    let v = u16::from_be_bytes([buf[*o], buf[*o + 1]]);
    *o += 2;
    v
}

fn read_u32(buf: &[u8], o: &mut usize) -> u32 {
    let v = u32::from_be_bytes([buf[*o], buf[*o + 1], buf[*o + 2], buf[*o + 3]]);
    *o += 4;
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_header_known_bytes_req_devlist() {
        // OP_REQ_DEVLIST: version 0x0111, code 0x8005, status 0.
        let h = OpHeader::new(OP_REQ_DEVLIST);
        let mut buf = Vec::new();
        h.encode(&mut buf);
        assert_eq!(buf, [0x01, 0x11, 0x80, 0x05, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn op_header_roundtrip() {
        let h = OpHeader {
            version: 0x0111,
            code: OP_REP_IMPORT,
            status: 0,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        let decoded = OpHeader::decode(&buf).unwrap();
        assert_eq!(h, decoded);
    }

    #[test]
    fn op_header_short_read() {
        assert!(matches!(
            OpHeader::decode(&[0; 4]),
            Err(ProtoError::ShortRead { need: 8, got: 4 })
        ));
    }

    #[test]
    fn exported_interface_roundtrip() {
        let i = ExportedInterface {
            class: 0x08,
            subclass: 0x06,
            protocol: 0x50,
        };
        let mut buf = Vec::new();
        i.encode(&mut buf);
        assert_eq!(buf.len(), ExportedInterface::SIZE);
        assert_eq!(buf, [0x08, 0x06, 0x50, 0x00]); // last byte is padding
        let decoded = ExportedInterface::decode(&buf).unwrap();
        assert_eq!(i, decoded);
    }

    #[test]
    fn exported_device_roundtrip_sandisk() {
        // Modeled on the real SanDisk Cruzer we enumerate (0781:5530),
        // a mass-storage device with one interface (class 0x08).
        let d = ExportedDevice {
            path: "/usbipd-mac/01-1".into(),
            busid: "01-1".into(),
            busnum: 1,
            devnum: 1,
            speed: 3, // USBIP_SPEED_HIGH
            id_vendor: 0x0781,
            id_product: 0x5530,
            bcd_device: 0x0001,
            b_device_class: 0x00,
            b_device_subclass: 0x00,
            b_device_protocol: 0x00,
            b_configuration_value: 1,
            b_num_configurations: 1,
            interfaces: vec![ExportedInterface {
                class: 0x08,
                subclass: 0x06,
                protocol: 0x50,
            }],
        };
        let mut buf = Vec::new();
        d.encode(&mut buf);
        assert_eq!(
            buf.len(),
            ExportedDevice::HEADER_SIZE + ExportedInterface::SIZE
        );
        let (decoded, consumed) = ExportedDevice::decode(&buf).unwrap();
        assert_eq!(d, decoded);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn exported_device_busid_offset_and_length() {
        // Verify the fixed-width string layout: busid lives at offset 256
        // and is NUL-padded to 32 bytes.
        let d = ExportedDevice {
            path: "p".into(),
            busid: "1-2.3".into(),
            busnum: 0,
            devnum: 0,
            speed: 0,
            id_vendor: 0,
            id_product: 0,
            bcd_device: 0,
            b_device_class: 0,
            b_device_subclass: 0,
            b_device_protocol: 0,
            b_configuration_value: 0,
            b_num_configurations: 0,
            interfaces: vec![],
        };
        let mut buf = Vec::new();
        d.encode(&mut buf);
        assert_eq!(&buf[256..256 + 5], b"1-2.3");
        assert!(buf[256 + 5..288].iter().all(|&b| b == 0));
    }
}
