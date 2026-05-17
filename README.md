# usbipd-mac

A macOS USB/IP server implementation in Rust, intended for use by [Lima](https://github.com/lima-vm/lima) and other VMMs that need to expose host USB devices to Linux guests.

> **Status:** MVP-4 — full protocol implementation; Linux clients can `usbip list -r`, `usbip attach`, and enumerate the device end-to-end. Bulk/interrupt transfers are blocked on macOS auto-binding kernel drivers to recognized device classes (see *Known limitations* below). MVP-5 will address this with IOKit force-capture.

## Why another `usbipd-mac`?

[`beriberikix/usbipd-mac`](https://github.com/beriberikix/usbipd-mac) is the existing Swift implementation. It requires an Apple-approved DriverKit System Extension entitlement to claim USB devices, and has been blocked on that approval since August 2025.

This project takes a different bet:

- **Sudo-mode forever** (or until/unless Apple grants a similar entitlement we could pair with). We assume the daemon runs as root, claims devices via [`nusb`](https://github.com/kevinmehall/nusb) directly through IOKit, and is invoked from an unprivileged client (e.g. Lima) through a narrow `sudoers.d` rule — the same pattern Lima already uses for [`socket_vmnet`](https://github.com/lima-vm/socket_vmnet).
- **Rust** — memory safety in a privileged daemon, no `libusb` runtime dependency, no Xcode required to build.
- **Lima-first integration** — designed to slot into Lima's existing `pkg/networks` sudoers/lifecycle pattern.

We deliberately give up the DriverKit promotion path: a pure-Rust codebase cannot ship a dext bundle. If Apple ever grants the necessary entitlement to small projects, we would add a Swift sidecar that XPCs to this daemon rather than rewriting in Swift.

## Layout

```
crates/
├── usbip-proto/    # USB/IP wire protocol (no_std-friendly codec)
├── host-mac/       # nusb-backed USB enumeration and device claim
├── usbip-server/   # transport-agnostic server state machine
└── usbipd/         # the `usbipd` binary
```

## Quick start

List local USB devices:

```sh
cargo run --release -- list
```

Run the daemon (default `127.0.0.1:3240`):

```sh
cargo run --release -- daemon --listen 0.0.0.0:3240
```

From a Linux client:

```sh
sudo modprobe vhci-hcd
sudo usbip list -r <mac-host>
sudo usbip attach -r <mac-host> -b <busid>
```

## Known limitations (current state)

The daemon currently implements the full USB/IP protocol — `OP_REQ_DEVLIST`, `OP_REQ_IMPORT`, `USBIP_CMD_SUBMIT` (control / bulk / interrupt), `USBIP_CMD_UNLINK` — but data-stage transfers (bulk / interrupt) are subject to macOS's kernel-driver auto-bind:

- **Control transfers (endpoint 0) always work**. The Linux guest successfully reads the device, configuration, and string descriptors over USB/IP, and the device is fully enumerated in the guest kernel.
- **Bulk and interrupt transfers fail with `kIOReturnExclusiveAccess` (0xE00002C5)** for any device whose interface class is recognized by macOS (mass storage, HID, CDC, printer, audio, etc.). macOS's in-kernel `IOUSBHost` framework binds the appropriate class driver before any userspace process can claim the interface, and `nusb 0.2.3` does not implement driver detach on macOS.
- **Vendor-specific (class 0xFF) interfaces are not auto-bound** and should work today, although not yet tested.

**MVP-5 plan:** call `IOUSBHostInterface::open(IOUSBHostObjectInitOptionsCaptureDevice)` from `IOUSBHost.framework` via a thin `unsafe` IOKit FFI layer (gated behind a non-default crate feature, and requiring root). This is force-capture and does not require any Apple entitlement — the DriverKit entitlement that blocks `beriberikix/usbipd-mac` is for a different code path (DriverKit driver bundles). Until then, MVP-4 is useful as a "device enumeration over USB/IP" service and as a fully tested protocol stack for the URB layer.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option, matching the conventions of the Rust ecosystem and Lima.
