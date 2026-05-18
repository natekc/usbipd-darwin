# usbipd CLI reference

_Auto-generated from `usbipd --help`. Regenerate with `make docs`._

## `usbipd`

```
macOS USB/IP server daemon

Usage: usbipd <COMMAND>

Commands:
  list             List local USB devices available for sharing
  daemon           Run the USB/IP server
  events           Stream USB hotplug events as NDJSON over a unix socket
  sudoers          Print a `NOPASSWD:NOSETENV` sudoers fragment for an exact `usbipd daemon ...` invocation
  release-capture  Release a force-captured device back to macOS (root only)
  help             Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```

## `usbipd list`

```
List local USB devices available for sharing

Usage: usbipd list [OPTIONS]

Options:
      --json  Emit machine-readable JSON instead of the human-readable table. Stable output, suitable for `jq` and for Lima's device-picker UI
  -h, --help  Print help
```

## `usbipd daemon`

```
Run the USB/IP server.

By default the daemon binds to 127.0.0.1 and refuses to export any device. Specify one or more `--allow VID:PID` flags (or `--allow-all` to skip filtering) to actually export devices, and `--allow-public` to accept binding to a non-loopback address.

Usage: usbipd daemon [OPTIONS]

Options:
      --listen <LISTEN>
          TCP address and port to listen on.
          
          Defaults to 127.0.0.1. Any non-loopback address requires `--allow-public` as a separate acknowledgement. Mutually exclusive with `--socket`.
          
          [default: 127.0.0.1:3240]

      --socket <PATH>
          Unix-domain socket path to listen on instead of TCP. The socket is created with mode 0600 (owner-only) so the only access control needed is filesystem permissions on the socket path. Intended for integrations like Lima that forward USB/IP over an already-authenticated transport

      --allow <VID:PID>
          Allow a specific (VendorID:ProductID) pair. Hex, no `0x` prefix. Example: `--allow 1050:0407` (`YubiKey` 5). Repeatable

      --allow-all
          Allow every USB device on this host. Only safe on a fully- trusted single-user machine. Mutually exclusive with `--allow`

      --allow-public
          Permit binding `--listen` to a non-loopback TCP address. Ignored when `--socket` is used. The USB/IP protocol is unauthenticated; the only legitimate reason to set this is when fronting the daemon with a firewall or VPN that supplies its own access control

  -h, --help
          Print help (see a summary with '-h')
```

## `usbipd events`

```
Stream USB hotplug events as NDJSON over a unix socket.

Intended for integrations like Lima that want to auto-attach devices as they appear. The socket is created with mode 0600; multiple consumers may subscribe simultaneously, and each new subscriber first receives the current device set as `added` events. See the `events` module docs for the on-wire schema.

Usage: usbipd events --socket <PATH>

Options:
      --socket <PATH>
          Unix-domain socket path to bind. Any existing socket file at this path is replaced; any existing non-socket file is preserved (the command fails instead)

  -h, --help
          Print help (see a summary with '-h')
```

## `usbipd sudoers`

```
Print a `NOPASSWD:NOSETENV` sudoers fragment for an exact `usbipd daemon ...` invocation.

Modeled on `limactl sudoers` / `socket_vmnet` in lima-vm/lima: the generated rule whitelists the absolute path of this binary plus the exact daemon argument list you pass here, scoped to a chosen Unix group. After installing the fragment, members of that group can run `sudo usbipd daemon ...` (with the same args) without a password.

Why root? macOS denies non-root processes exclusive access to USB interfaces that are already bound to a kernel driver (`kIOReturnExclusiveAccess`, 0xe00002c5), which kills bulk transfers to e.g. mass-storage devices. Run as root to break the kernel driver's claim.

Typical use:

usbipd sudoers --listen 127.0.0.1:3240 --allow 0781:5530 \ | sudo tee /etc/sudoers.d/usbipd sudo usbipd daemon --listen 127.0.0.1:3240 --allow 0781:5530

Usage: usbipd sudoers [OPTIONS] [DAEMON_ARGS]...

Arguments:
  [DAEMON_ARGS]...
          Daemon arguments to whitelist, exactly as you would pass them to `usbipd daemon`. Separate from `usbipd sudoers`'s own flags with `--`, e.g. `usbipd sudoers --group admin -- --listen 127.0.0.1:3240`

Options:
      --group <GROUP>
          Unix group whose members get the NOPASSWD rule. The default `admin` matches the local-admin group on macOS and mirrors the convention used by `limactl sudoers`
          
          [default: admin]

      --binary <PATH>
          Override the binary path baked into the rule. Defaults to the absolute path of the currently-running `usbipd`. Useful when generating the fragment on one host for deployment on another, or when the binary is reached via a symlink you'd rather pin

  -h, --help
          Print help (see a summary with '-h')
```

## `usbipd release-capture`

```
Release a force-captured device back to macOS (root only).

Manual escape hatch for the case where the daemon was killed ungracefully (e.g. `SIGKILL`) while holding a device with the `USBDeviceReEnumerate` capture flag set. macOS keeps the device detached from its kernel drivers until either a release re-enumerate or a physical unplug; this command does the former.

Usage: usbipd release-capture <BUSID>

Arguments:
  <BUSID>
          USB/IP busid of the device to release (e.g. `01-1`)

Options:
  -h, --help
          Print help (see a summary with '-h')
```
