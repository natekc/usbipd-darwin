//! HID keyboard proof-of-concept for force-capture.
//!
//! Opens a USB HID boot keyboard by busid, force-captures it on macOS
//! (detaches `IOHIDFamily`), claims interface 0, and reads the
//! interrupt-IN endpoint, decoding 8-byte HID Boot Keyboard reports
//! into a stream of key-press events.
//!
//! Run as root:
//! ```sh
//! cargo build --release --example hid_keyboard -p host-mac
//! sudo target/release/examples/hid_keyboard <busid>
//! ```
//!
//! On exit (Ctrl-C), the keyboard is released back to macOS via the
//! `Drop` impl on `OpenedDevice`.
//!
//! Tested on Apple Silicon macOS against a Megawin Programmable
//! Keyboard (`b404:0101`) — busid is typically `01-1`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use host_mac::OpenedDevice;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let busid = std::env::args()
        .nth(1)
        .ok_or("usage: hid_keyboard <busid>")?;

    eprintln!("opening {busid}");
    let dev = OpenedDevice::open(&busid)?;
    eprintln!("opened, reading interrupt IN on EP 0x81 (HID boot keyboard)");
    eprintln!("press keys; Ctrl-C to exit");

    let running = Arc::new(AtomicBool::new(true));
    {
        let r = Arc::clone(&running);
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
            .unwrap_or_else(|e| eprintln!("ctrlc handler install failed: {e}"));
    }

    let mut prev = [0u8; 8];
    while running.load(Ordering::SeqCst) {
        // 8-byte HID boot keyboard report; 1 s timeout so we can poll
        // the Ctrl-C flag.
        match dev.data_transfer(0x81, 8, &[], Duration::from_secs(1)) {
            Ok(report) if report.len() == 8 => {
                if report != prev {
                    print_report(&report, &prev);
                    prev = report.as_slice().try_into().unwrap_or([0u8; 8]);
                }
            }
            Ok(other) => eprintln!("short report ({} bytes): {other:02x?}", other.len()),
            Err(host_mac::HostError::Transfer(nusb::transfer::TransferError::Cancelled)) => {} // 1 s poll timeout
            Err(e) => {
                eprintln!("transfer error: {e}");
                break;
            }
        }
    }

    eprintln!("exiting; releasing capture");
    Ok(())
}

fn print_report(report: &[u8], prev: &[u8]) {
    // HID Boot Keyboard report layout:
    //   byte 0    : modifier bitmap (LCtrl, LShift, LAlt, LGUI,
    //               RCtrl, RShift, RAlt, RGUI)
    //   byte 1    : reserved
    //   bytes 2-7 : up to 6 currently-pressed key usage IDs
    let mods = report[0];
    let mut keys: Vec<u8> = report[2..].iter().copied().filter(|&k| k != 0).collect();
    keys.sort_unstable();
    let prev_keys: Vec<u8> = prev[2..].iter().copied().filter(|&k| k != 0).collect();
    let pressed: Vec<u8> = keys
        .iter()
        .copied()
        .filter(|k| !prev_keys.contains(k))
        .collect();
    let released: Vec<u8> = prev_keys
        .iter()
        .copied()
        .filter(|k| !keys.contains(k))
        .collect();

    let mod_str = describe_mods(mods);
    for k in &pressed {
        println!("DOWN  {:>10}  usage=0x{:02x}  {}", hid_name(*k), k, mod_str);
    }
    for k in &released {
        println!("UP    {:>10}  usage=0x{:02x}  {}", hid_name(*k), k, mod_str);
    }
    if pressed.is_empty() && released.is_empty() && mods != prev[0] {
        println!("MOD   {mod_str}");
    }
}

fn describe_mods(m: u8) -> String {
    let names = [
        (0b0000_0001, "LCtrl"),
        (0b0000_0010, "LShift"),
        (0b0000_0100, "LAlt"),
        (0b0000_1000, "LGUI"),
        (0b0001_0000, "RCtrl"),
        (0b0010_0000, "RShift"),
        (0b0100_0000, "RAlt"),
        (0b1000_0000, "RGUI"),
    ];
    let active: Vec<&str> = names
        .iter()
        .filter(|(b, _)| m & b != 0)
        .map(|(_, n)| *n)
        .collect();
    if active.is_empty() {
        "[no mods]".to_owned()
    } else {
        format!("[{}]", active.join("+"))
    }
}

/// Map a small subset of HID Usage IDs (Keyboard/Keypad page 0x07) to
/// human names. Unknown usages return `?`.
fn hid_name(usage: u8) -> &'static str {
    match usage {
        0x04..=0x1d => {
            const LETTERS: &[&str] = &[
                "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P",
                "Q", "R", "S", "T", "U", "V", "W", "X", "Y", "Z",
            ];
            LETTERS[(usage - 0x04) as usize]
        }
        0x1e => "1",
        0x1f => "2",
        0x20 => "3",
        0x21 => "4",
        0x22 => "5",
        0x23 => "6",
        0x24 => "7",
        0x25 => "8",
        0x26 => "9",
        0x27 => "0",
        0x28 => "Enter",
        0x29 => "Esc",
        0x2a => "Backspace",
        0x2b => "Tab",
        0x2c => "Space",
        0x2d => "-",
        0x2e => "=",
        0x2f => "[",
        0x30 => "]",
        0x31 => "\\",
        0x33 => ";",
        0x34 => "'",
        0x35 => "`",
        0x36 => ",",
        0x37 => ".",
        0x38 => "/",
        0x39 => "CapsLk",
        0x3a..=0x45 => "Fn",
        0x4f => "Right",
        0x50 => "Left",
        0x51 => "Down",
        0x52 => "Up",
        _ => "?",
    }
}
