//! USB/IP server state machine.
//!
//! Transport-agnostic: callers feed bytes in and pull bytes out. The TCP
//! adapter lives in the `usbipd` binary crate.

#![forbid(unsafe_code)]

use host_mac::UsbDevice;
use usbip_proto::{
    ExportedDevice, ExportedInterface, OP_REP_DEVLIST, OP_REQ_DEVLIST, OpHeader, ProtoError,
};

/// Result of handling one inbound op-message.
#[derive(Debug)]
pub enum Reply {
    /// Bytes to write back to the client.
    Bytes(Vec<u8>),
    /// Client sent something we don't yet support; the caller should log and
    /// close the connection.
    Unsupported(u16),
}

/// Handle a single inbound op-message header (plus any payload, not yet
/// applicable to `OP_REQ_DEVLIST`) and produce a reply.
pub fn handle_op(header: OpHeader, devices: &[UsbDevice]) -> Result<Reply, ProtoError> {
    match header.code {
        OP_REQ_DEVLIST => Ok(Reply::Bytes(encode_rep_devlist(devices))),
        code => Ok(Reply::Unsupported(code)),
    }
}

/// Encode an `OP_REP_DEVLIST` payload: op-header + u32 device count +
/// one [`ExportedDevice`] record per device.
fn encode_rep_devlist(devices: &[UsbDevice]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 4 + devices.len() * 320);
    OpHeader::new(OP_REP_DEVLIST).encode(&mut out);
    let n = u32::try_from(devices.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&n.to_be_bytes());
    for d in devices {
        to_exported(d).encode(&mut out);
    }
    out
}

fn to_exported(d: &UsbDevice) -> ExportedDevice {
    ExportedDevice {
        path: format!("/usbipd-mac/{}", d.busid),
        busid: d.busid.clone(),
        busnum: d.busnum,
        devnum: d.devnum,
        speed: d.speed,
        id_vendor: d.vendor_id,
        id_product: d.product_id,
        bcd_device: d.bcd_device,
        b_device_class: d.class,
        b_device_subclass: d.subclass,
        b_device_protocol: d.protocol,
        b_configuration_value: d.configuration_value,
        b_num_configurations: d.num_configurations,
        interfaces: d
            .interfaces
            .iter()
            .map(|i| ExportedInterface {
                class: i.class,
                subclass: i.subclass,
                protocol: i.protocol,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use host_mac::UsbInterface;

    fn sample_device() -> UsbDevice {
        UsbDevice {
            busid: "01-1".into(),
            busnum: 1,
            devnum: 1,
            speed: 3,
            vendor_id: 0x0781,
            product_id: 0x5530,
            bcd_device: 0x0001,
            class: 0,
            subclass: 0,
            protocol: 0,
            configuration_value: 1,
            num_configurations: 1,
            manufacturer: Some("SanDisk".into()),
            product: Some("Cruzer".into()),
            serial: None,
            interfaces: vec![UsbInterface {
                class: 0x08,
                subclass: 0x06,
                protocol: 0x50,
            }],
        }
    }

    #[test]
    fn req_devlist_returns_rep_devlist_with_count_and_record() {
        let header = OpHeader::new(OP_REQ_DEVLIST);
        let reply = handle_op(header, &[sample_device()]).unwrap();
        let bytes = match reply {
            Reply::Bytes(b) => b,
            Reply::Unsupported(_) => panic!("expected bytes"),
        };
        // op header
        assert_eq!(&bytes[0..2], &[0x01, 0x11]); // version
        assert_eq!(&bytes[2..4], &[0x00, 0x05]); // OP_REP_DEVLIST
        assert_eq!(&bytes[4..8], &[0, 0, 0, 0]); // status
        // device count
        assert_eq!(&bytes[8..12], &[0, 0, 0, 1]);
        // exported device record decodes back round-trip
        let (dev, _) = usbip_proto::ExportedDevice::decode(&bytes[12..]).unwrap();
        assert_eq!(dev.id_vendor, 0x0781);
        assert_eq!(dev.id_product, 0x5530);
        assert_eq!(dev.busid, "01-1");
        assert_eq!(dev.interfaces.len(), 1);
        assert_eq!(dev.interfaces[0].class, 0x08);
    }

    #[test]
    fn unknown_op_code_is_unsupported() {
        let header = OpHeader {
            version: 0x0111,
            code: 0xFFFF,
            status: 0,
        };
        match handle_op(header, &[]).unwrap() {
            Reply::Unsupported(0xFFFF) => {}
            other => panic!("expected Unsupported(0xFFFF), got {other:?}"),
        }
    }

    #[test]
    fn empty_devlist_encodes_zero_count() {
        let header = OpHeader::new(OP_REQ_DEVLIST);
        let reply = handle_op(header, &[]).unwrap();
        let bytes = match reply {
            Reply::Bytes(b) => b,
            Reply::Unsupported(_) => panic!("expected bytes"),
        };
        assert_eq!(bytes.len(), 12); // 8 header + 4 count
        assert_eq!(&bytes[8..12], &[0, 0, 0, 0]);
    }
}
