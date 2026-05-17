# `usbip-proto` fuzz targets

These are [`cargo-fuzz`](https://rust-fuzz.github.io/book/) targets for
the wire-protocol decoders. The decoders are the only code path that
runs on attacker-controlled bytes before any policy gate, so they're
the highest-leverage thing to fuzz.

The crate is **excluded from the workspace** because `cargo-fuzz`
requires nightly Rust and a custom `RUSTFLAGS` set. Run it explicitly:

```sh
# One-time:
rustup install nightly
cargo install cargo-fuzz

# Then, from this crate's directory (crates/usbip-proto):
cargo +nightly fuzz run op_header
cargo +nightly fuzz run cmd_submit
cargo +nightly fuzz run urb_header
cargo +nightly fuzz run req_import_busid
```

Each target's contract is the same: `decode` on arbitrary bytes must
never panic. Returning `Err` is fine; aborting the process is a bug.
