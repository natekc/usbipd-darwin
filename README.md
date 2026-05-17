# usbipd-mac

A macOS USB/IP server implementation in Rust, intended for use by [Lima](https://github.com/lima-vm/lima) and other VMMs that need to expose host USB devices to Linux guests.

> **Status:** very early. MVP-3 (server answers `OP_REQ_DEVLIST`) is the current milestone.

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

## Quick start (once MVP-1 lands)

```sh
cargo run --release -- list
```

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option, matching the conventions of the Rust ecosystem and Lima.
