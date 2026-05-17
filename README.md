# usbipd-mac

A macOS USB/IP server implementation in Rust, intended for use by [Lima](https://github.com/lima-vm/lima) and other VMMs that need to expose host USB devices to Linux guests.

> **Status:** MVP-5 — full protocol implementation plus IOKit force-capture, so Linux clients can `usbip list -r`, `usbip attach`, and use any USB device class end-to-end (mass storage, HID, CDC, printer, audio, …). Verified with a USB HID keyboard: both interfaces bind to `usbhid` on the guest, full input event chain works.

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

Run the daemon on loopback TCP (you must pick a policy — the default is
DenyAll, which exports nothing):

```sh
# Allow a single device by VID:PID (repeatable).
sudo cargo run --release -- daemon --allow 1050:0407

# Or, on a fully-trusted single-user machine, allow everything.
sudo cargo run --release -- daemon --allow-all
```

From a Linux client:

```sh
sudo modprobe vhci-hcd
sudo usbip list -r <mac-host>
sudo usbip attach -r <mac-host> -b <busid>
```

## Security model

**USB/IP has no authentication and no transport encryption.** Anyone who
can reach the listener can enumerate and attach any allow-listed device.
This daemon therefore ships secure-by-default:

| Knob | Default | What it does |
| --- | --- | --- |
| `--listen ADDR:PORT` | `127.0.0.1:3240` | Bind to loopback TCP only. |
| `--allow VID:PID` | (none) | Allow-list one device. Repeatable. |
| `--allow-all` | off | Allow every device. Equivalent to upstream Linux `usbipd` behaviour. |
| `--allow-public` | off | Explicit ack required to bind a non-loopback TCP address. |
| `--socket PATH` | (none) | Listen on a unix-domain socket (mode 0600) instead of TCP. |

Recommended deployments, in order of preference:

1. **Unix socket** (`--socket /run/usbipd.sock`) when the consumer is on
   the same machine. Filesystem permissions are the only ACL needed.
2. **Loopback TCP** when the consumer is a local VM forwarder (e.g. Lima
   port-forwarding 3240 into the guest over its already-authenticated
   SSH/vsock channel).
3. **Public TCP** (`--allow-public`) only when a separate firewall or
   VPN is providing transport-level authentication.

Each successfully-imported device is also locked to its connecting
client: a second concurrent `OP_REQ_IMPORT` for the same busid is
refused so two clients can't fight over the same device.

## Lima integration

The intended Lima integration is the same shape as `socket_vmnet`:

1. Lima writes a `--socket` path under its instance directory.
2. Lima spawns `sudo usbipd daemon --socket <path> --allow VID:PID ...`
   per the user's `usb:` config block.
3. Lima forwards bytes between the unix socket and TCP 3240 in the
   guest, where the guest kernel's `vhci-hcd` driver speaks USB/IP.

Because the unix socket is mode 0600 owned by root, the only thing on
the host that can talk to the daemon is Lima itself (running as the
same user that started the VM and ran `limactl sudoers`). No network
exposure, no protocol-level auth needed.

## Running with force-capture (root)

When the daemon runs as root, it automatically force-detaches macOS kernel
drivers from any device a client imports, using
[`USBDeviceReEnumerate`](https://developer.apple.com/documentation/iokit/iousbdeviceinterface500/usbdevicereenumerate)
with `kUSBReEnumerateCaptureDeviceMask`. This is the same mechanism
Apple's own developer tooling uses to claim devices from in-kernel drivers,
and it does **not** require any Apple entitlement (the DriverKit
entitlement that blocks `beriberikix/usbipd-mac` is for a different code
path — DriverKit driver bundles).

When the daemon receives `SIGINT` (or a client disconnects cleanly), the
capture is automatically released so macOS rebinds its built-in drivers.
If the daemon is killed ungracefully (`SIGKILL`, panic), the capture
persists across process death — the device stays detached from macOS
kernel drivers until either a physical unplug or:

```sh
sudo usbipd release-capture <busid>
```

When running as a non-root user, the daemon still works but only control
transfers on endpoint 0 are guaranteed to succeed; bulk/interrupt
transfers will fail with `kIOReturnExclusiveAccess` for any device whose
interfaces macOS auto-binds.

The canonical deployment pattern (matching what Lima already does for
[`socket_vmnet`](https://github.com/lima-vm/socket_vmnet)) is a narrow
`sudoers.d` rule:

```
# /etc/sudoers.d/usbipd
%admin ALL=(ALL) NOPASSWD: /usr/local/bin/usbipd daemon *
%admin ALL=(ALL) NOPASSWD: /usr/local/bin/usbipd release-capture *
```

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option, matching the conventions of the Rust ecosystem and Lima.
