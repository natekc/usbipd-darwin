//! USB/IP URB protocol: `USBIP_CMD_SUBMIT`, `USBIP_RET_SUBMIT`,
//! `USBIP_CMD_UNLINK`, `USBIP_RET_UNLINK`.
//!
//! These messages are exchanged after a successful `OP_REQ_IMPORT`. They share
//! a 20-byte basic header followed by 28 bytes of opcode-specific fields, for
//! a fixed 48-byte total header size.
//!
//! Reference: `drivers/usb/usbip/usbip_common.h` (`struct usbip_header`).

use crate::ProtoError;

pub const USBIP_CMD_SUBMIT: u32 = 0x0000_0001;
pub const USBIP_RET_SUBMIT: u32 = 0x0000_0003;
pub const USBIP_CMD_UNLINK: u32 = 0x0000_0002;
pub const USBIP_RET_UNLINK: u32 = 0x0000_0004;

/// Fixed wire size of every URB-mode header (basic + union).
pub const URB_HEADER_SIZE: usize = 48;

/// USB/IP direction field. Note this is the URB direction, not the high bit
/// of the endpoint address.
pub const USBIP_DIR_OUT: u32 = 0;
pub const USBIP_DIR_IN: u32 = 1;

/// 20-byte basic header common to every URB message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UrbHeader {
    pub command: u32,
    pub seqnum: u32,
    /// `(busnum << 16) | devnum`.
    pub devid: u32,
    /// 0 = OUT (host -> device), 1 = IN (device -> host).
    pub direction: u32,
    /// Endpoint number (0..15), without the direction bit.
    pub ep: u32,
}

impl UrbHeader {
    pub const SIZE: usize = 20;

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.command.to_be_bytes());
        out.extend_from_slice(&self.seqnum.to_be_bytes());
        out.extend_from_slice(&self.devid.to_be_bytes());
        out.extend_from_slice(&self.direction.to_be_bytes());
        out.extend_from_slice(&self.ep.to_be_bytes());
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < Self::SIZE {
            return Err(ProtoError::ShortRead {
                need: Self::SIZE,
                got: buf.len(),
            });
        }
        Ok(Self {
            command: u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
            seqnum: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
            devid: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
            direction: u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]),
            ep: u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]),
        })
    }
}

/// `CMD_SUBMIT`-specific tail (28 bytes after the basic header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmdSubmit {
    pub transfer_flags: u32,
    /// Signed on the wire; negative means "indeterminate" historically but
    /// modern clients always send a non-negative byte count.
    pub transfer_buffer_length: i32,
    pub start_frame: i32,
    /// `0xFFFF_FFFF` (i.e. `-1`) for non-isochronous transfers.
    pub number_of_packets: i32,
    pub interval: i32,
    /// Raw 8-byte USB SETUP packet, used for control transfers; little-endian
    /// per the USB spec. All zeros for non-control transfers.
    pub setup: [u8; 8],
}

impl CmdSubmit {
    pub const SIZE: usize = 28;

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.transfer_flags.to_be_bytes());
        out.extend_from_slice(&self.transfer_buffer_length.to_be_bytes());
        out.extend_from_slice(&self.start_frame.to_be_bytes());
        out.extend_from_slice(&self.number_of_packets.to_be_bytes());
        out.extend_from_slice(&self.interval.to_be_bytes());
        out.extend_from_slice(&self.setup);
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < Self::SIZE {
            return Err(ProtoError::ShortRead {
                need: Self::SIZE,
                got: buf.len(),
            });
        }
        let mut setup = [0u8; 8];
        setup.copy_from_slice(&buf[20..28]);
        Ok(Self {
            transfer_flags: u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
            transfer_buffer_length: i32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
            start_frame: i32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
            number_of_packets: i32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]),
            interval: i32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]),
            setup,
        })
    }
}

/// `RET_SUBMIT`-specific tail (28 bytes after the basic header).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetSubmit {
    /// 0 on success; negative errno otherwise (Linux conventions).
    pub status: i32,
    pub actual_length: i32,
    pub start_frame: i32,
    pub number_of_packets: i32,
    pub error_count: i32,
    /// 8 bytes of padding to align with the union size in the kernel struct.
    pub padding: [u8; 8],
}

impl RetSubmit {
    pub const SIZE: usize = 28;

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.status.to_be_bytes());
        out.extend_from_slice(&self.actual_length.to_be_bytes());
        out.extend_from_slice(&self.start_frame.to_be_bytes());
        out.extend_from_slice(&self.number_of_packets.to_be_bytes());
        out.extend_from_slice(&self.error_count.to_be_bytes());
        out.extend_from_slice(&self.padding);
    }
}

/// `CMD_UNLINK`-specific tail (28 bytes after the basic header). Currently
/// the server treats unlink as best-effort and always replies success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmdUnlink {
    pub unlink_seqnum: u32,
    pub padding: [u8; 24],
}

impl CmdUnlink {
    pub const SIZE: usize = 28;

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < Self::SIZE {
            return Err(ProtoError::ShortRead {
                need: Self::SIZE,
                got: buf.len(),
            });
        }
        let mut pad = [0u8; 24];
        pad.copy_from_slice(&buf[4..28]);
        Ok(Self {
            unlink_seqnum: u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
            padding: pad,
        })
    }
}

/// `RET_UNLINK`-specific tail (28 bytes after the basic header).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetUnlink {
    pub status: i32,
    pub padding: [u8; 24],
}

impl RetUnlink {
    pub const SIZE: usize = 28;

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.status.to_be_bytes());
        out.extend_from_slice(&self.padding);
    }
}

/// Helper: write a complete `RET_SUBMIT` (header + tail + optional IN payload).
pub fn write_ret_submit(out: &mut Vec<u8>, seqnum: u32, ret: &RetSubmit, payload: &[u8]) {
    let hdr = UrbHeader {
        command: USBIP_RET_SUBMIT,
        seqnum,
        devid: 0,
        direction: 0,
        ep: 0,
    };
    hdr.encode(out);
    ret.encode(out);
    out.extend_from_slice(payload);
}

/// Helper: write a complete `RET_UNLINK`.
pub fn write_ret_unlink(out: &mut Vec<u8>, seqnum: u32, status: i32) {
    let hdr = UrbHeader {
        command: USBIP_RET_UNLINK,
        seqnum,
        devid: 0,
        direction: 0,
        ep: 0,
    };
    hdr.encode(out);
    RetUnlink {
        status,
        padding: [0; 24],
    }
    .encode(out);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urb_header_roundtrip() {
        let h = UrbHeader {
            command: USBIP_CMD_SUBMIT,
            seqnum: 0x1234_5678,
            devid: (1 << 16) | 1,
            direction: USBIP_DIR_IN,
            ep: 1,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        assert_eq!(buf.len(), UrbHeader::SIZE);
        // command on the wire
        assert_eq!(&buf[0..4], &[0, 0, 0, 1]);
        // seqnum
        assert_eq!(&buf[4..8], &0x1234_5678u32.to_be_bytes());
        let decoded = UrbHeader::decode(&buf).unwrap();
        assert_eq!(h, decoded);
    }

    #[test]
    fn cmd_submit_decodes_get_descriptor() {
        // A typical control IN GET_DESCRIPTOR(Device) URB:
        //   transfer_flags=0
        //   transfer_buffer_length=18
        //   start_frame=0, number_of_packets=-1, interval=0
        //   setup = 80 06 00 01 00 00 12 00 (LE wValue=0x0100, wLength=18)
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_be_bytes()); // flags
        buf.extend_from_slice(&18i32.to_be_bytes()); // tbl
        buf.extend_from_slice(&0i32.to_be_bytes()); // start_frame
        buf.extend_from_slice(&(-1i32).to_be_bytes()); // n_pkts
        buf.extend_from_slice(&0i32.to_be_bytes()); // interval
        buf.extend_from_slice(&[0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]);
        let cmd = CmdSubmit::decode(&buf).unwrap();
        assert_eq!(cmd.transfer_buffer_length, 18);
        assert_eq!(cmd.number_of_packets, -1);
        assert_eq!(cmd.setup, [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]);
    }

    #[test]
    fn ret_submit_known_bytes() {
        let mut buf = Vec::new();
        write_ret_submit(
            &mut buf,
            42,
            &RetSubmit {
                status: 0,
                actual_length: 4,
                ..Default::default()
            },
            &[0xDE, 0xAD, 0xBE, 0xEF],
        );
        assert_eq!(buf.len(), URB_HEADER_SIZE + 4);
        assert_eq!(&buf[0..4], &3u32.to_be_bytes()); // USBIP_RET_SUBMIT
        assert_eq!(&buf[4..8], &42u32.to_be_bytes()); // seqnum
        assert_eq!(&buf[20..24], &0i32.to_be_bytes()); // status
        assert_eq!(&buf[24..28], &4i32.to_be_bytes()); // actual_length
        assert_eq!(&buf[48..52], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn cmd_unlink_decodes() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&77u32.to_be_bytes()); // unlink_seqnum
        buf.extend_from_slice(&[0u8; 24]);
        let u = CmdUnlink::decode(&buf).unwrap();
        assert_eq!(u.unlink_seqnum, 77);
    }
}
