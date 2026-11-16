//! libusb, process-driver variant: the driver's description, the event
//! callbacks, and pipes to an inserted device (`usbdriver.h`,
//! `usbprocdriver.h`).
//!
//! The USB server owns the host controller and copies transfer data through
//! its own DMA memory, so a transfer here only lends a buffer for the
//! duration of a blocking call.

use crate::bot::BulkPipes;
use crate::device::Location;
use core::ffi::{c_char, c_int, c_uint, c_void};
use core::ptr::NonNull;
use phoenix::{Errno, Oid, Result};
use std::sync::OnceLock;

const DRVNAME_MAX: usize = 10;
const DEVPATH_MAX: usize = 32;
const ANY: c_uint = c_uint::MAX;

/* usb_transfer_type_t, usb_dir_t */
const TRANSFER_CONTROL: c_int = 0;
const TRANSFER_BULK: c_int = 2;
const DIR_OUT: c_int = 0;
const DIR_IN: c_int = 1;

/// `usb_devinfo_t`
#[repr(C)]
#[derive(Clone, Copy)]
struct DevInfo {
    descriptor: [u8; 18],
    manufacturer: [c_char; 32],
    product: [c_char; 32],
    serial_number: [c_char; 32],
    bus: c_int,
    dev: c_int,
    interface: c_int,
    location_id: c_uint,
}

/// `usb_deletion_t`
#[repr(C)]
struct Deletion {
    bus: c_int,
    dev: c_int,
    interface: c_int,
}

/// `usb_event_insertion_t`
#[repr(C)]
struct InsertionEvent {
    device_created: bool,
    dev: Oid,
    dev_path: [c_char; DEVPATH_MAX],
}

/// `usb_device_id_t`
#[repr(C)]
struct DeviceId {
    vid: c_uint,
    pid: c_uint,
    dclass: c_uint,
    subclass: c_uint,
    protocol: c_uint,
}

type InsertionFn = unsafe extern "C" fn(*mut UsbDriver, *mut DevInfo, *mut InsertionEvent) -> c_int;
type DeletionFn = unsafe extern "C" fn(*mut UsbDriver, *mut Deletion) -> c_int;
type CompletionFn = unsafe extern "C" fn(*mut UsbDriver, *mut c_void, *const c_char, usize) -> c_int;
type InitFn = unsafe extern "C" fn(*mut UsbDriver, *mut c_void) -> c_int;
type DestroyFn = unsafe extern "C" fn(*mut UsbDriver) -> c_int;

/// `usb_handlers_t`
#[repr(C)]
struct Handlers {
    insertion: Option<InsertionFn>,
    deletion: Option<DeletionFn>,
    completion: Option<CompletionFn>,
}

/// `usb_driverOps_t`
#[repr(C)]
struct Ops {
    init: Option<InitFn>,
    destroy: Option<DestroyFn>,
}

/// `struct usb_driver`
#[repr(C)]
struct UsbDriver {
    next: *mut UsbDriver,
    prev: *mut UsbDriver,
    name: [c_char; DRVNAME_MAX],
    handlers: Handlers,
    ops: Ops,
    /// Set by usb_driverProcRun().
    pipe_ops: *const c_void,
    filters: *const DeviceId,
    nfilters: c_uint,
    priv_: *mut c_void,
    host_priv: *mut c_void,
}

#[cfg(target_arch = "x86")]
const _: () = {
    use core::mem::{offset_of, size_of};
    // ia32's i386-pc-phoenix-gcc
    assert!(size_of::<DevInfo>() == 132);
    assert!(offset_of!(DevInfo, bus) == 116);
    assert!(size_of::<Deletion>() == 12);
    assert!(size_of::<InsertionEvent>() == 48);
    assert!(offset_of!(InsertionEvent, dev) == 4);
    assert!(offset_of!(InsertionEvent, dev_path) == 16);
    assert!(size_of::<DeviceId>() == 20);
    assert!(size_of::<UsbDriver>() == 60);
    assert!(offset_of!(UsbDriver, handlers) == 20);
    assert!(offset_of!(UsbDriver, ops) == 32);
    assert!(offset_of!(UsbDriver, filters) == 44);
    assert!(offset_of!(UsbDriver, host_priv) == 56);
};

unsafe extern "C" {
    fn usb_driverProcRun(drv: *mut UsbDriver, prio: c_int, nthreads: c_uint, args: *mut c_void) -> !;
    fn usb_open(drv: *mut UsbDriver, dev: *mut DevInfo, kind: c_int, dir: c_int) -> c_int;
    fn usb_setConfiguration(drv: *mut UsbDriver, pipe: c_uint, conf: c_int) -> c_int;
    fn usb_transferBulk(drv: *mut UsbDriver, pipe: c_uint, data: *mut c_void, size: usize, dir: c_int) -> c_int;
}

/// What the driver does with the events.
pub trait Handler: Sync {
    /// A matching device was inserted: probe it and create its device file.
    fn insert(&self, dev: UsbDevice) -> Result<Inserted>;
    /// A device was removed.
    fn delete(&self, at: Location);
}

/// The device file an insertion created.
pub struct Inserted {
    pub oid: Oid,
    pub path: String,
}

/// Matches SCSI transparent command set devices on the Bulk-Only Transport.
const MASS_STORAGE: u32 = 0x08;
const SUBCLASS_SCSI: u32 = 0x06;
const PROTOCOL_BULK_ONLY: u32 = 0x50;

struct Filters([DeviceId; 1]);
// SAFETY: plain integers, never written.
unsafe impl Sync for Filters {}
static FILTERS: Filters = Filters([DeviceId {
    vid: ANY,
    pid: ANY,
    dclass: MASS_STORAGE,
    subclass: SUBCLASS_SCSI,
    protocol: PROTOCOL_BULK_ONLY,
}]);

static HANDLER: OnceLock<&'static dyn Handler> = OnceLock::new();

/// Connects to the USB server and handles its events on `threads` threads
/// (this one among them) at priority `prio`. Does not return.
pub fn run(handler: &'static dyn Handler, prio: i32, threads: u32) -> ! {
    if HANDLER.set(handler).is_err() {
        panic!("usb::run called twice");
    }
    let mut name = [0 as c_char; DRVNAME_MAX];
    for (dst, &src) in name.iter_mut().zip(b"umass") {
        *dst = src as c_char;
    }
    // libusb links and writes it (pipe_ops), and keeps it for good: leaked.
    let drv = Box::leak(Box::new(UsbDriver {
        next: core::ptr::null_mut(),
        prev: core::ptr::null_mut(),
        name,
        handlers: Handlers { insertion: Some(on_insertion), deletion: Some(on_deletion), completion: Some(on_completion) },
        ops: Ops { init: Some(on_init), destroy: Some(on_destroy) },
        pipe_ops: core::ptr::null(),
        filters: FILTERS.0.as_ptr(),
        nfilters: FILTERS.0.len() as c_uint,
        priv_: core::ptr::null_mut(),
        host_priv: core::ptr::null_mut(),
    }));
    // SAFETY: `drv` is a valid, leaked (so 'static) driver whose callbacks
    // match the C signatures; libusb is its only user from here on.
    unsafe { usb_driverProcRun(drv, prio, threads, core::ptr::null_mut()) }
}

fn handler() -> &'static dyn Handler {
    *HANDLER.get().expect("set before usb_driverProcRun()")
}

/// The driver's state lives in Rust; there is nothing to set up.
unsafe extern "C" fn on_init(_: *mut UsbDriver, _: *mut c_void) -> c_int {
    0
}

unsafe extern "C" fn on_destroy(_: *mut UsbDriver) -> c_int {
    0
}

/// Synchronous transfers only: nothing completes asynchronously.
unsafe extern "C" fn on_completion(_: *mut UsbDriver, _: *mut c_void, _: *const c_char, _: usize) -> c_int {
    0
}

unsafe extern "C" fn on_insertion(drv: *mut UsbDriver, info: *mut DevInfo, event: *mut InsertionEvent) -> c_int {
    let (Some(drv), Some(info), Some(event)) = (NonNull::new(drv), NonNull::new(info), NonNull::new(event)) else {
        return -Errno::EINVAL.code();
    };
    // SAFETY: libusb passes a device description valid for the call; it is
    // copied out.
    let info = unsafe { *info.as_ptr() };
    match handler().insert(UsbDevice { drv, info }) {
        Ok(inserted) => {
            let mut path = [0 as c_char; DEVPATH_MAX];
            for (dst, &src) in path.iter_mut().take(DEVPATH_MAX - 1).zip(inserted.path.as_bytes()) {
                *dst = src as c_char;
            }
            // SAFETY: libusb passes an event valid for writing for the call
            // ("implementations can assume `event` is a valid pointer").
            unsafe { event.as_ptr().write(InsertionEvent { device_created: true, dev: inserted.oid, dev_path: path }) };
            0
        }
        Err(e) => -e.code(),
    }
}

unsafe extern "C" fn on_deletion(_: *mut UsbDriver, del: *mut Deletion) -> c_int {
    let Some(del) = NonNull::new(del) else {
        return -Errno::EINVAL.code();
    };
    // SAFETY: libusb passes a deletion event valid for the call.
    let del = unsafe { del.as_ref() };
    handler().delete(Location { bus: del.bus, dev: del.dev, interface: del.interface });
    0
}

/// An inserted device, for the duration of its insertion handling.
pub struct UsbDevice {
    drv: NonNull<UsbDriver>,
    info: DevInfo,
}

impl UsbDevice {
    pub fn location(&self) -> Location {
        Location { bus: self.info.bus, dev: self.info.dev, interface: self.info.interface }
    }

    fn open(&mut self, kind: c_int, dir: c_int) -> Result<Pipe> {
        // SAFETY: `drv` is the leaked driver, `info` a valid description
        // libusb only reads.
        let id = Errno::check(unsafe { usb_open(self.drv.as_ptr(), &mut self.info, kind, dir) })?;
        Ok(Pipe { drv: self.drv, id: id as c_uint })
    }

    /// Selects configuration `conf` through the control pipe.
    pub fn configure(&mut self, conf: i32) -> Result<()> {
        let ctrl = self.open(TRANSFER_CONTROL, DIR_OUT)?;
        // SAFETY: as in `open`; `ctrl.id` is a pipe of this device.
        Errno::check(unsafe { usb_setConfiguration(self.drv.as_ptr(), ctrl.id, conf) }).map(drop)
    }

    /// The bulk IN and OUT pipes.
    pub fn bulk_pipes(&mut self) -> Result<Bulk> {
        let input = self.open(TRANSFER_BULK, DIR_IN)?;
        let output = self.open(TRANSFER_BULK, DIR_OUT)?;
        Ok(Bulk { input, output })
    }
}

/// A pipe the USB server opened. libusb has no call to close one: the server
/// frees a device's pipes when the device goes away.
struct Pipe {
    drv: NonNull<UsbDriver>,
    id: c_uint,
}

/// A device's bulk pipes.
pub struct Bulk {
    input: Pipe,
    output: Pipe,
}

// SAFETY: the driver they point to is leaked and immutable after
// usb_driverProcRun(); a transfer is a message to the USB server, which any
// thread may send.
unsafe impl Send for Bulk {}

impl BulkPipes for Bulk {
    fn bulk_out(&mut self, data: &[u8]) -> Result<usize> {
        let p = &self.output;
        // SAFETY: an OUT transfer only reads `data`, valid for its length for
        // the call; the cast drops a `const` libusb does not declare.
        let n = unsafe { usb_transferBulk(p.drv.as_ptr(), p.id, data.as_ptr().cast_mut().cast(), data.len(), DIR_OUT) };
        Errno::check(n).map(|n| n as usize)
    }

    fn bulk_in(&mut self, data: &mut [u8]) -> Result<usize> {
        let p = &self.input;
        // SAFETY: `data` is valid for writing its length for the call, and
        // nothing else touches it meanwhile.
        let n = unsafe { usb_transferBulk(p.drv.as_ptr(), p.id, data.as_mut_ptr().cast(), data.len(), DIR_IN) };
        // a device cannot report more than it was given room for
        Errno::check(n).map(|n| (n as usize).min(data.len()))
    }
}
