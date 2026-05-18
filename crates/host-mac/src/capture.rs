//! macOS IOKit force-capture for USB devices.
//!
//! Calls `IOUSBDeviceInterface500::USBDeviceReEnumerate` with the
//! `kUSBReEnumerateCaptureDeviceMask` option, which terminates any
//! kernel drivers attached to the device (mass storage, HID, CDC,
//! audio, …) and re-enumerates it without them. After this, normal
//! `nusb::Interface::claim_interface` calls succeed.
//!
//! This is the *force-capture* path. It requires running as root.
//! It does NOT require any Apple entitlement — the DriverKit
//! entitlement that blocks `beriberikix/usbipd-mac` is a different
//! code path (DriverKit driver bundles).
//!
//! The whole file is `unsafe`-permitting; we intentionally isolate
//! every IOKit call here behind a small safe API:
//!
//! ```ignore
//! capture::reenumerate(registry_entry_id)?;
//! ```

#![allow(unsafe_code)]
#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    clippy::missing_safety_doc,
    clippy::upper_case_acronyms,
    clippy::doc_markdown,
    clippy::items_after_statements,
    clippy::used_underscore_binding,
    clippy::borrow_as_ptr,
    clippy::too_many_lines
)]

use std::ffi::c_void;
use std::ptr;

use core_foundation_sys::base::{CFRelease, SInt32};
use core_foundation_sys::uuid::{CFUUIDBytes, CFUUIDRef};
use io_kit_sys::ret::{IOReturn, kIOReturnSuccess};
use io_kit_sys::types::io_service_t;
use io_kit_sys::{IOObjectRelease, IORegistryEntryIDMatching, IOServiceGetMatchingService};
use thiserror::Error;
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// Type aliases (mirroring Apple's IOKit / CoreFoundation headers).
// ---------------------------------------------------------------------------

type kern_return_t = ::std::os::raw::c_int;
type HRESULT = SInt32;
type LPVOID = *mut c_void;
type REFIID = CFUUIDBytes;
type UInt8 = ::std::os::raw::c_uchar;
type UInt32 = ::std::os::raw::c_uint;
type ULONG = ::std::os::raw::c_ulong;

/// `USBDeviceReEnumerate` option that causes the device to come back
/// with all kernel drivers detached. Defined in
/// `<IOKit/usb/IOUSBLib.h>` as `kUSBReEnumerateCaptureDeviceMask`.
pub(crate) const kUSBReEnumerateCaptureDeviceMask: UInt32 = 1 << 30;
/// `USBDeviceReEnumerate` option that requests a release of the
/// capture (kernel drivers may rebind on next re-enumeration).
pub(crate) const kUSBReEnumerateReleaseDeviceMask: UInt32 = 1 << 29;

// ---------------------------------------------------------------------------
// Bare-minimum vtable: we only call Release, USBDeviceOpenSeize,
// USBDeviceClose, and USBDeviceReEnumerate. Every other slot is opaque.
//
// Index layout MUST match Apple's `IOUSBDeviceStruct500`
// (`<IOKit/usb/IOUSBLib.h>`) up to index 37 (USBDeviceReEnumerate).
// ---------------------------------------------------------------------------

type Opaque = *mut c_void;
type IoReturnFn = Option<unsafe extern "C" fn(*mut c_void) -> IOReturn>;
type ReEnumFn = Option<unsafe extern "C" fn(*mut c_void, UInt32) -> IOReturn>;
type ReleaseFn = Option<unsafe extern "C" fn(*mut c_void) -> ULONG>;

#[repr(C)]
struct IOUSBDeviceVTable {
    _reserved: Opaque, // 0
    _query_interface: Option<unsafe extern "C" fn(*mut c_void, REFIID, *mut LPVOID) -> HRESULT>, // 1
    _add_ref: Option<unsafe extern "C" fn(*mut c_void) -> ULONG>, // 2
    release: ReleaseFn,                                           // 3
    _m4: Opaque,                                                  // 4  CreateDeviceAsyncEventSource
    _m5: Opaque,                                                  // 5  GetDeviceAsyncEventSource
    _m6: Opaque,                                                  // 6  CreateDeviceAsyncPort
    _m7: Opaque,                                                  // 7  GetDeviceAsyncPort
    _usb_device_open: IoReturnFn,                                 // 8  USBDeviceOpen
    usb_device_close: IoReturnFn,                                 // 9  USBDeviceClose
    _m10: Opaque,                                                 // 10 GetDeviceClass
    _m11: Opaque,                                                 // 11 GetDeviceSubClass
    _m12: Opaque,                                                 // 12 GetDeviceProtocol
    _m13: Opaque,                                                 // 13 GetDeviceVendor
    _m14: Opaque,                                                 // 14 GetDeviceProduct
    _m15: Opaque,                                                 // 15 GetDeviceReleaseNumber
    _m16: Opaque,                                                 // 16 GetDeviceAddress
    _m17: Opaque,                                                 // 17 GetDeviceBusPowerAvailable
    _m18: Opaque,                                                 // 18 GetDeviceSpeed
    _m19: Opaque,                                                 // 19 GetNumberOfConfigurations
    _m20: Opaque,                                                 // 20 GetLocationID
    _m21: Opaque,                      // 21 GetConfigurationDescriptorPtr
    _m22: Opaque,                      // 22 GetConfiguration
    _m23: Opaque,                      // 23 SetConfiguration
    _m24: Opaque,                      // 24 GetBusFrameNumber
    _m25: Opaque,                      // 25 ResetDevice
    _m26: Opaque,                      // 26 DeviceRequest
    _m27: Opaque,                      // 27 DeviceRequestAsync
    _m28: Opaque,                      // 28 CreateInterfaceIterator
    usb_device_open_seize: IoReturnFn, // 29 USBDeviceOpenSeize
    _m30: Opaque,                      // 30 DeviceRequestTO
    _m31: Opaque,                      // 31 DeviceRequestAsyncTO
    _m32: Opaque,                      // 32 USBDeviceSuspend
    _m33: Opaque,                      // 33 USBDeviceAbortPipeZero
    _m34: Opaque,                      // 34 USBGetManufacturerStringIndex
    _m35: Opaque,                      // 35 USBGetProductStringIndex
    _m36: Opaque,                      // 36 USBGetSerialNumberStringIndex
    usb_device_reenumerate: ReEnumFn,  // 37 USBDeviceReEnumerate
}

// Compile-time guard against accidental layout drift: every slot in the
// vtable above must be exactly one pointer wide, and there must be 38
// of them (indices 0..=37). If anyone adds or removes a field without
// updating Apple's `IOUSBDeviceStruct500` layout assumption, this fails
// to compile.
const _: () = assert!(
    std::mem::size_of::<IOUSBDeviceVTable>() == 38 * std::mem::size_of::<*mut c_void>(),
    "IOUSBDeviceVTable layout drifted from IOUSBDeviceStruct500"
);

#[repr(C)]
struct IOCFPlugInVTable {
    _reserved: Opaque,
    _query_interface: Option<unsafe extern "C" fn(*mut c_void, REFIID, *mut LPVOID) -> HRESULT>,
    _add_ref: Option<unsafe extern "C" fn(*mut c_void) -> ULONG>,
    release: ReleaseFn,
    _version: u16,
    _revision: u16,
    _probe: Opaque,
    _start: Opaque,
    _stop: Opaque,
}

// ---------------------------------------------------------------------------
// FFI: IOKit + CoreFoundation glue not in io-kit-sys.
// ---------------------------------------------------------------------------

unsafe extern "C" {
    fn IOCreatePlugInInterfaceForService(
        service: io_service_t,
        pluginType: CFUUIDRef,
        interfaceType: CFUUIDRef,
        theInterface: *mut *mut *mut IOCFPlugInVTable,
        theScore: *mut SInt32,
    ) -> kern_return_t;

    fn CFUUIDGetConstantUUIDWithBytes(
        alloc: *const c_void,
        b0: UInt8,
        b1: UInt8,
        b2: UInt8,
        b3: UInt8,
        b4: UInt8,
        b5: UInt8,
        b6: UInt8,
        b7: UInt8,
        b8: UInt8,
        b9: UInt8,
        b10: UInt8,
        b11: UInt8,
        b12: UInt8,
        b13: UInt8,
        b14: UInt8,
        b15: UInt8,
    ) -> CFUUIDRef;

    fn CFUUIDGetUUIDBytes(uuid: CFUUIDRef) -> CFUUIDBytes;
}

/// `kIOUsbDeviceUserClientTypeID` — plugin type for USB device user clients.
fn k_io_usb_device_user_client_type_id() -> CFUUIDRef {
    // 9DC7B780-9EC0-11D4-A54F-000A27052861
    unsafe {
        CFUUIDGetConstantUUIDWithBytes(
            ptr::null(),
            0x9d,
            0xc7,
            0xb7,
            0x80,
            0x9e,
            0xc0,
            0x11,
            0xD4,
            0xa5,
            0x4f,
            0x00,
            0x0a,
            0x27,
            0x05,
            0x28,
            0x61,
        )
    }
}

/// `kIOUSBDeviceInterfaceID500` — the IOUSBDeviceInterface version 500
/// (macOS 10.7.3+). Has every field through `USBDeviceReEnumerate`.
/// UUID: 396104F7-943D-4893-90F1-69BD6CF5C2EB
fn k_io_usb_device_interface_id_500() -> CFUUIDRef {
    unsafe {
        CFUUIDGetConstantUUIDWithBytes(
            ptr::null(),
            0x39,
            0x61,
            0x04,
            0xF7,
            0x94,
            0x3D,
            0x48,
            0x93,
            0x90,
            0xF1,
            0x69,
            0xBD,
            0x6C,
            0xF5,
            0xC2,
            0xEB,
        )
    }
}

// ---------------------------------------------------------------------------
// Public API.
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("no IOUSBDevice service found for registry_entry_id {0:#x}")]
    NoService(u64),
    #[error("IOCreatePlugInInterfaceForService failed: {0:#x}")]
    PlugIn(kern_return_t),
    #[error("QueryInterface failed: HRESULT {0:#x}")]
    QueryInterface(HRESULT),
    #[error("USBDeviceOpenSeize failed: {0:#x} (run as root?)")]
    OpenSeize(IOReturn),
    #[error("USBDeviceReEnumerate failed: {0:#x}")]
    ReEnumerate(IOReturn),
}

/// `true` if the current process is running as uid 0 (root).
#[must_use]
pub fn is_root() -> bool {
    // SAFETY: `geteuid` is always safe to call.
    unsafe { libc::geteuid() == 0 }
}

/// Force-detach all kernel drivers from the device identified by
/// `registry_entry_id` and re-enumerate it in capture mode. On
/// success the device disappears from the bus for ~100–500 ms and
/// reappears with no kernel drivers attached, ready for `nusb` to
/// claim interfaces normally.
///
/// Requires root.
///
/// `registry_entry_id` is the value returned by
/// `nusb::DeviceInfo::registry_entry_id()` on macOS.
pub fn reenumerate_with_capture(registry_entry_id: u64) -> Result<(), CaptureError> {
    reenumerate_with_options(registry_entry_id, kUSBReEnumerateCaptureDeviceMask)
}

/// Re-enumerate the device and release any prior capture, allowing
/// kernel drivers to rebind. Counterpart to
/// [`reenumerate_with_capture`].
pub fn reenumerate_release(registry_entry_id: u64) -> Result<(), CaptureError> {
    reenumerate_with_options(registry_entry_id, kUSBReEnumerateReleaseDeviceMask)
}

fn reenumerate_with_options(registry_entry_id: u64, options: UInt32) -> Result<(), CaptureError> {
    debug!(
        registry_entry_id = format!("{registry_entry_id:#x}"),
        options = format!("{options:#x}"),
        "USBDeviceReEnumerate"
    );

    // 1. Find the IOService.
    let service = unsafe {
        let matching = IORegistryEntryIDMatching(registry_entry_id);
        if matching.is_null() {
            return Err(CaptureError::NoService(registry_entry_id));
        }
        // IOServiceGetMatchingService consumes one reference of `matching`.
        IOServiceGetMatchingService(0, matching)
    };
    if service == 0 {
        return Err(CaptureError::NoService(registry_entry_id));
    }

    // RAII guard for the IOService.
    struct ServiceGuard(io_service_t);
    impl Drop for ServiceGuard {
        fn drop(&mut self) {
            // SAFETY: we own a +1 retain on `self.0` from
            // IOServiceGetMatchingService.
            unsafe { IOObjectRelease(self.0) };
        }
    }
    let _service_guard = ServiceGuard(service);

    // 2. Create the user-client plug-in.
    let mut plugin: *mut *mut IOCFPlugInVTable = ptr::null_mut();
    let mut score: SInt32 = 0;
    let kr = unsafe {
        IOCreatePlugInInterfaceForService(
            service,
            k_io_usb_device_user_client_type_id(),
            // kIOCFPlugInInterfaceID — see <IOKit/IOCFPlugIn.h>.
            // C244E858-109C-11D4-91D4-0050E4C6426F
            CFUUIDGetConstantUUIDWithBytes(
                ptr::null(),
                0xC2,
                0x44,
                0xE8,
                0x58,
                0x10,
                0x9C,
                0x11,
                0xD4,
                0x91,
                0xD4,
                0x00,
                0x50,
                0xE4,
                0xC6,
                0x42,
                0x6F,
            ),
            &mut plugin,
            &mut score,
        )
    };
    if kr != kIOReturnSuccess as kern_return_t || plugin.is_null() {
        return Err(CaptureError::PlugIn(kr));
    }

    // 3. QueryInterface for IOUSBDeviceInterface500.
    let mut dev_iface: *mut *mut IOUSBDeviceVTable = ptr::null_mut();
    let hr = unsafe {
        let plugin_vt = &**plugin;
        let qi = plugin_vt
            ._query_interface
            .expect("plugin vtable QueryInterface");
        qi(
            plugin.cast::<c_void>(),
            CFUUIDGetUUIDBytes(k_io_usb_device_interface_id_500()),
            (&raw mut dev_iface).cast::<*mut c_void>(),
        )
    };
    // Release the plugin regardless of QI result; we either have a refcounted
    // dev_iface now or we're erroring out.
    unsafe {
        if let Some(release) = (**plugin).release {
            release(plugin.cast::<c_void>());
        }
    }
    if hr != 0 || dev_iface.is_null() {
        return Err(CaptureError::QueryInterface(hr));
    }

    // RAII guard for the IOUSBDeviceInterface.
    struct DevGuard(*mut *mut IOUSBDeviceVTable);
    impl Drop for DevGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(release) = (**self.0).release {
                    release(self.0.cast::<c_void>());
                }
            }
        }
    }
    let dev_guard = DevGuard(dev_iface);

    // 4. OpenSeize — yanks the device away from any current owner. Needs root.
    let ret = unsafe {
        let f = (**dev_guard.0)
            .usb_device_open_seize
            .expect("vtable USBDeviceOpenSeize");
        f(dev_guard.0.cast::<c_void>())
    };
    if ret != kIOReturnSuccess {
        return Err(CaptureError::OpenSeize(ret));
    }

    // 5. ReEnumerate with the requested options. This invalidates `dev_iface`
    //    because the underlying IOService is destroyed and recreated. We must
    //    close before the interface goes away.
    let ret = unsafe {
        let f = (**dev_guard.0)
            .usb_device_reenumerate
            .expect("vtable USBDeviceReEnumerate");
        f(dev_guard.0.cast::<c_void>(), options)
    };

    // 6. Close — best effort; the device may already be gone post-reenumerate.
    unsafe {
        if let Some(close) = (**dev_guard.0).usb_device_close {
            let _ = close(dev_guard.0.cast::<c_void>());
        }
    }

    drop(dev_guard);

    if ret != kIOReturnSuccess {
        return Err(CaptureError::ReEnumerate(ret));
    }

    info!(
        registry_entry_id = format!("{registry_entry_id:#x}"),
        options = format!("{options:#x}"),
        "ReEnumerate succeeded"
    );
    Ok(())
}

// `CFRelease` is intentionally referenced so that future expansions of
// this module (e.g. CFNumber-based matching) don't drift in their imports.
#[allow(dead_code)]
fn _cf_release_keepalive(r: *const c_void) {
    unsafe { CFRelease(r) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pin Apple's `<IOKit/usb/IOUSBLib.h>` constants so that anyone hand-
    // editing the bit positions notices immediately. Drift here would
    // silently swap the meaning of capture/release, with no IOKit error.
    #[test]
    fn capture_mask_matches_apple_header() {
        assert_eq!(kUSBReEnumerateCaptureDeviceMask, 0x4000_0000);
    }

    #[test]
    fn release_mask_matches_apple_header() {
        assert_eq!(kUSBReEnumerateReleaseDeviceMask, 0x2000_0000);
    }

    #[test]
    fn capture_and_release_masks_are_disjoint() {
        assert_eq!(
            kUSBReEnumerateCaptureDeviceMask & kUSBReEnumerateReleaseDeviceMask,
            0,
            "capture and release flags must not overlap"
        );
    }

    #[test]
    fn no_service_error_includes_hex_registry_id() {
        let err = CaptureError::NoService(0xdead_beef);
        let s = format!("{err}");
        assert!(s.contains("0xdeadbeef"), "got: {s}");
    }

    #[test]
    fn open_seize_error_mentions_root() {
        #[allow(clippy::cast_possible_wrap)]
        let err = CaptureError::OpenSeize(0xe000_02c5_u32 as i32);
        let s = format!("{err}");
        assert!(
            s.contains("root"),
            "OpenSeize message should hint that root is required; got: {s}"
        );
    }

    // Tests don't run as root, so this should be false. If someone runs
    // `cargo test` under sudo, we don't want to fail — just confirm the
    // type is a bool.
    #[test]
    fn is_root_returns_bool() {
        let _: bool = is_root();
    }
}
