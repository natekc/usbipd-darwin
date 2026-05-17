//! Micro-benchmarks for the hot wire-format paths.
//!
//! These cover what the daemon's URB loop runs on every packet:
//! `UrbHeader::{encode,decode}`, `CmdSubmit::decode`,
//! `write_ret_submit`, plus the lower-frequency `OpHeader` /
//! `ExportedDevice` paths used for `OP_REQ_DEVLIST` and
//! `OP_REQ_IMPORT`.
//!
//! Run with `cargo bench -p usbip-proto`.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use usbip_proto::{
    CmdSubmit, ExportedDevice, ExportedInterface, OpHeader, RetSubmit, URB_HEADER_SIZE,
    USBIP_CMD_SUBMIT, USBIP_DIR_IN, UrbHeader, write_ret_submit,
};

fn sample_urb_header() -> UrbHeader {
    UrbHeader {
        command: USBIP_CMD_SUBMIT,
        seqnum: 0x1234_5678,
        devid: 0x0001_0001,
        direction: USBIP_DIR_IN,
        ep: 0x81,
    }
}

fn sample_cmd_submit() -> CmdSubmit {
    CmdSubmit {
        transfer_flags: 0,
        transfer_buffer_length: 512,
        start_frame: 0,
        number_of_packets: -1,
        interval: 0,
        setup: [0; 8],
    }
}

fn sample_ret_submit() -> RetSubmit {
    RetSubmit {
        status: 0,
        actual_length: 512,
        start_frame: 0,
        number_of_packets: 0,
        error_count: 0,
        padding: [0; 8],
    }
}

fn sample_exported_device() -> ExportedDevice {
    ExportedDevice {
        path: "/usbipd-darwin/01-1".into(),
        busid: "01-1".into(),
        busnum: 1,
        devnum: 1,
        speed: 3,
        id_vendor: 0x0781,
        id_product: 0x5530,
        bcd_device: 0x0001,
        b_device_class: 0,
        b_device_subclass: 0,
        b_device_protocol: 0,
        b_configuration_value: 1,
        b_num_configurations: 1,
        interfaces: vec![ExportedInterface {
            class: 0x08,
            subclass: 0x06,
            protocol: 0x50,
        }],
    }
}

fn bench_urb_header(c: &mut Criterion) {
    let hdr = sample_urb_header();
    let mut buf = Vec::with_capacity(URB_HEADER_SIZE);
    c.bench_function("UrbHeader::encode", |b| {
        b.iter(|| {
            buf.clear();
            black_box(&hdr).encode(&mut buf);
        });
    });
    let mut encoded = Vec::with_capacity(20);
    hdr.encode(&mut encoded);
    c.bench_function("UrbHeader::decode", |b| {
        b.iter(|| {
            let _ = UrbHeader::decode(black_box(&encoded)).unwrap();
        });
    });
}

fn bench_cmd_submit(c: &mut Criterion) {
    let cmd = sample_cmd_submit();
    let mut encoded = Vec::with_capacity(28);
    cmd.encode(&mut encoded);
    c.bench_function("CmdSubmit::decode", |b| {
        b.iter(|| {
            let _ = CmdSubmit::decode(black_box(&encoded)).unwrap();
        });
    });
}

fn bench_ret_submit_write(c: &mut Criterion) {
    let ret = sample_ret_submit();
    let payload = vec![0u8; 512];
    let mut out = Vec::with_capacity(URB_HEADER_SIZE + payload.len());
    c.bench_function("write_ret_submit/512B", |b| {
        b.iter(|| {
            out.clear();
            write_ret_submit(&mut out, 0x1234, black_box(&ret), black_box(&payload));
        });
    });
}

fn bench_op_header(c: &mut Criterion) {
    let h = OpHeader::new(0x0005);
    let mut encoded = Vec::with_capacity(OpHeader::SIZE);
    h.encode(&mut encoded);
    c.bench_function("OpHeader::decode", |b| {
        b.iter(|| {
            let _ = OpHeader::decode(black_box(&encoded)).unwrap();
        });
    });
}

fn bench_exported_device(c: &mut Criterion) {
    let dev = sample_exported_device();
    let mut buf = Vec::with_capacity(320);
    c.bench_function("ExportedDevice::encode", |b| {
        b.iter(|| {
            buf.clear();
            black_box(&dev).encode(&mut buf);
        });
    });
    let mut encoded = Vec::with_capacity(320);
    dev.encode(&mut encoded);
    c.bench_function("ExportedDevice::decode", |b| {
        b.iter(|| {
            let (_, _) = ExportedDevice::decode(black_box(&encoded)).unwrap();
        });
    });
}

criterion_group!(
    benches,
    bench_urb_header,
    bench_cmd_submit,
    bench_ret_submit_write,
    bench_op_header,
    bench_exported_device,
);
criterion_main!(benches);
