# Contributing to usbipd-darwin

Thanks for your interest! This project is small and focused — please skim
this whole file before opening a PR.

## Scope

`usbipd-darwin` is a USB/IP **server** for macOS. Things that fit the scope:

- Wire-protocol correctness fixes (compare against the Linux kernel
  `drivers/usb/usbip/` reference and `usbipd-win` behaviour).
- macOS-specific capture/release improvements.
- New `usbipd` subcommands that are useful to a host-side daemon
  (enumeration, hotplug, diagnostics).
- Lima integration ergonomics.

Things that do **not** fit:

- A USB/IP **client** for macOS. That is a separate project (would need a
  vhci-style virtual-HCI driver, almost certainly a DriverKit dext).
- Inventing a custom authentication layer over USB/IP. See [SECURITY.md](SECURITY.md).
- Rewrites away from Rust, or pulling in `libusb` as a runtime dep.

If you're not sure whether something fits, please open an issue first.

## Development

Requirements:

- macOS 14 or newer (CI covers 14 and 15).
- Rust stable, at least the MSRV declared in [Cargo.toml](Cargo.toml)
  (`rust-version`).
- For fuzz targets: nightly Rust + `cargo install cargo-fuzz`.

Build & test:

```sh
cargo build --release
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

Force-capture code paths require running as root and a real USB device
plugged in. There is no good way to mock IOKit; manual verification with a
HID keyboard or USB stick is the expected workflow before sending a PR
that touches `crates/host-mac/src/capture.rs`. Please describe what device
you tested with in the PR.

End-to-end against a Linux guest:

```sh
sudo cargo run --release -- daemon --allow VID:PID
# in a Lima VM (or any Linux box on the same network):
sudo modprobe vhci-hcd
sudo usbip list -r <mac-host>
sudo usbip attach -r <mac-host> -b <busid>
```

## Pull requests

- **One logical change per PR.** Rebase / squash to a clean history before
  asking for review.
- **Commit messages**: short imperative subject (`crate: change`), wrap body
  at 72 cols, explain *why* not *what*. Look at `git log` for examples.
- **No new `unsafe` blocks** without an accompanying SAFETY comment that
  explains every invariant. The workspace denies `unsafe_code` by default;
  if you need it, scope the allow as narrowly as possible.
- **Tests**: add unit tests for protocol/codec changes. Capture-path
  changes should describe the device class verified.
- **Docs**: if you change a flag, default, or invariant, update README
  and CHANGELOG (`Unreleased` section).
- **CI must be green** (fmt, clippy, test, build) before merge.

## Releasing (maintainers)

1. Move `Unreleased` items in [CHANGELOG.md](CHANGELOG.md) under a new
   `[X.Y.Z] — YYYY-MM-DD` heading.
2. Bump `workspace.package.version` and each crate's internal-dep `version`
   in [Cargo.toml](Cargo.toml).
3. `cargo build --release && cargo test --all-features`.
4. Commit `chore: release vX.Y.Z`, tag `vX.Y.Z`, push tag.
5. The `release` GitHub Actions workflow builds, notarizes, and uploads
   a signed binary to the GitHub Release.
6. Update the Homebrew tap (see [dist/homebrew/README.md](dist/homebrew/README.md)).

## License & DCO

By contributing, you agree that your contributions are dual-licensed under
[Apache-2.0](LICENSE-APACHE) and [MIT](LICENSE-MIT), the same as the rest
of the project.

We do not require a CLA. We do require sign-off on commits — add
`Signed-off-by: Your Name <you@example.com>` (e.g. `git commit -s`) to
certify the [Developer Certificate of Origin](https://developercertificate.org/).
