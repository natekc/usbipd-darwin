# Changelog

All notable changes to `usbipd-darwin` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-05-17

First public preview release. End-to-end verified against Linux `usbip` against
a HID keyboard, mass storage, and CDC devices on macOS 14 and 15 with Lima as
the consumer.

### Added

- USB/IP wire protocol codec (`usbip-proto`): `OP_REQ_DEVLIST`,
  `OP_REQ_IMPORT`, `CMD_SUBMIT`, `CMD_UNLINK`, URB header.
- Host backend (`host-mac`): `nusb`-backed enumeration; deterministic
  busnum/devnum from bus+port chain; per-endpoint locking for parallel
  transfers; alt-aware endpoint discovery with `SET_CONFIGURATION`/
  `SET_INTERFACE` intercept; short-write `actual_length` reporting.
- Force-capture (`host-mac/capture`): `IOUSBDeviceInterface500::USBDeviceReEnumerate`
  with `kUSBReEnumerateCaptureDeviceMask`; release-on-drop ordering that
  closes the `nusb::Device` before issuing the release re-enumerate; VID:PID
  verification before release; vtable-size assertion guard.
- Daemon (`usbipd`):
  - Subcommands: `list` (with `--json`), `daemon`, `release-capture`,
    `events` (NDJSON hotplug stream), `sudoers`.
  - Concurrent in-flight URBs per session; `CMD_UNLINK` with cancellation.
  - Hotplug watchdog closes the session on device unplug.
  - `SIGINT`/`SIGTERM` handler releases every captured device before exit.
  - Per-device mutex prevents two clients from racing on the same busid.
- Security:
  - Secure-by-default: binds `127.0.0.1:3240`, exports nothing without an
    explicit `--allow` or `--allow-all`.
  - `--allow-public` gate required to bind a non-loopback address.
  - `--socket PATH` unix-domain socket transport (mode 0600).
  - Rejects bad protocol versions and filters out hubs.
  - Documents Linux-ABI errno values in `RET_SUBMIT` status.
- Packaging: install `Makefile` with `build`/`codesign`/`install`/`notarize`
  targets; hardened-runtime entitlements file; sample launchd plist.
- Fuzzing: `cargo-fuzz` targets for `op_header`, `cmd_submit`, `urb_header`,
  `req_import_busid`.
- Benchmarks: `criterion` bench for hot codec paths.
- Docs: README with rationale vs `usbipd-win` and `beriberikix/usbipd-mac`,
  security model section, Lima integration recipe.

### Project

- Renamed from `usbipd-mac` to `usbipd-darwin` to avoid confusion with
  `beriberikix/usbipd-mac`.
- MSRV: Rust 1.85 (edition 2024).
- Dual-licensed Apache-2.0 OR MIT.

[Unreleased]: https://github.com/lima-vm/usbipd-darwin/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/lima-vm/usbipd-darwin/releases/tag/v0.1.0
