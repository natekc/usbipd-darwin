# Security policy

## Reporting a vulnerability

Please **do not** open a public GitHub issue for security problems.

Report privately via GitHub's [security advisory form](https://github.com/lima-vm/usbipd-darwin/security/advisories/new),
or by email to <nathankcchan@gmail.com> with subject `usbipd-darwin: security`.

You should expect an acknowledgement within 7 days. We will work with you on a
coordinated disclosure timeline; 90 days is the default cap, shorter if the
issue is already being exploited.

## Threat model

`usbipd-darwin` is a privileged daemon (runs as root in its supported
deployment) that exposes host USB devices over the USB/IP wire protocol.
The threats we defend against, in priority order:

1. **Untrusted network peers reaching the listener.** USB/IP itself has no
   authentication or transport encryption. The daemon is therefore
   secure-by-default:
   - Binds to `127.0.0.1:3240` unless `--allow-public` is passed.
   - Refuses to export any device unless `--allow VID:PID` (repeatable) or
     `--allow-all` is passed.
   - Supports `--socket PATH` for a unix-domain socket (mode 0600) so the
     OS-level ACL is the only thing that can talk to the daemon.
2. **Concurrent attach races.** Once a client successfully imports a busid,
   that busid is locked to the importing connection; a second `OP_REQ_IMPORT`
   for the same device is refused until the first session ends.
3. **Malformed wire input.** All four attacker-reachable decoders
   (`OP_REQ_IMPORT`, `OP_REQ_DEVLIST`, `CMD_SUBMIT`, URB header) have
   `cargo-fuzz` targets in [`crates/usbip-proto/fuzz`](crates/usbip-proto/fuzz/).
   The decoders are `#![deny(unsafe_code)]`.
4. **Lingering host-state on crash.** The daemon registers a `SIGINT`/`SIGTERM`
   handler that releases every captured device before exit. If killed with
   `SIGKILL`, captured devices stay detached from macOS kernel drivers until
   physical unplug or `sudo usbipd release-capture <busid>`.

## Non-goals

- **Authenticating USB/IP peers.** Use a unix socket, loopback + VM
  port-forwarding, or a VPN/firewall. We will not invent a custom auth
  layer over a published wire protocol.
- **Sandboxing the daemon below root.** Force-capture of kernel-bound USB
  devices via `IOUSBDeviceInterface500::USBDeviceReEnumerate` requires root
  on macOS. Running unprivileged works but bulk/interrupt transfers will
  fail with `kIOReturnExclusiveAccess` on any device whose interfaces macOS
  auto-binds.
- **Defending against a malicious USB device.** Once a guest attaches a
  device, it is on the guest's kernel attack surface; that is the same
  posture as plugging the device into the guest directly.

## Supported versions

`usbipd-darwin` is pre-1.0. Only the latest released `0.x` tag receives
security fixes. Once 1.0 ships this section will be updated with a real
support matrix.
