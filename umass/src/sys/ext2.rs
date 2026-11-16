//! The C libext2, as a [`Filesystem`].
//!
//! libext2 reads and writes the device through two callbacks, passing back
//! the id it was mounted with. That id is a mount token here, resolved by
//! [`BlockIo`], never a device id: device ids are reused, a mount outlives
//! its device's removal.

use crate::fs::{BlockIo, Filesystem, MountedFs};
use core::ffi::{c_char, c_int, c_uint, c_void};
use core::ptr::NonNull;
use phoenix::msg::raw::RawMsg;
use phoenix::{Errno, Id, Oid, Result};
use std::sync::OnceLock;

const SECTOR: c_uint = crate::scsi::SECTOR as c_uint;
/// `LIBEXT2_TYPE`: Linux native.
const PARTITION_TYPE: u8 = 0x83;

type ReadFn = unsafe extern "C" fn(Id, i64, *mut c_char, usize) -> isize;
type WriteFn = unsafe extern "C" fn(Id, i64, *const c_char, usize) -> isize;

unsafe extern "C" {
    fn libext2_mount(dev: *mut Oid, sectorsz: c_uint, read: ReadFn, write: WriteFn, fdata: *mut *mut c_void) -> c_int;
    fn libext2_handler(fdata: *mut c_void, msg: *mut RawMsg) -> c_int;
    fn libext2_unmount(fdata: *mut c_void) -> c_int;
}

/// The callbacks are plain functions, so what they resolve tokens with is
/// global; one driver per process.
static IO: OnceLock<&'static dyn BlockIo> = OnceLock::new();

pub struct Ext2;

impl Ext2 {
    pub fn new(io: &'static dyn BlockIo) -> Ext2 {
        // a second driver would have the same `io`; keeping the first is fine
        let _ = IO.set(io);
        Ext2
    }
}

impl Filesystem for Ext2 {
    fn name(&self) -> &str {
        "ext2"
    }

    fn partition_type(&self) -> u8 {
        PARTITION_TYPE
    }

    fn mount(&self, dev: Oid) -> Result<(Id, Box<dyn MountedFs>)> {
        let mut dev = dev;
        let mut fdata = core::ptr::null_mut();
        // SAFETY: `dev` and `fdata` are valid out-pointers for the call; the
        // callbacks match libext2's `dev_read`/`dev_write`.
        let root = Errno::check(unsafe { libext2_mount(&mut dev, SECTOR, read, write, &mut fdata) })?;
        let fdata = NonNull::new(fdata).ok_or(Errno::EIO)?;
        Ok((root as Id, Box::new(Mounted { fdata })))
    }
}

/// A mounted libext2 instance.
struct Mounted {
    fdata: NonNull<c_void>,
}

// SAFETY: libext2 locks its own state (ext2.c, obj.c); the C driver called
// one instance from two pool threads at once, as this one does.
unsafe impl Send for Mounted {}
// SAFETY: as above.
unsafe impl Sync for Mounted {}

impl MountedFs for Mounted {
    fn handle(&self, msg: &mut RawMsg) {
        // SAFETY: `fdata` is a live instance (unmount consumes `self`), `msg`
        // a received message valid for the call.
        unsafe { libext2_handler(self.fdata.as_ptr(), msg) };
    }

    fn unmount(self: Box<Self>) -> Result<()> {
        // SAFETY: `fdata` is live and, `self` being consumed, never used again.
        Errno::check(unsafe { libext2_unmount(self.fdata.as_ptr()) }).map(drop)
    }
}

fn io() -> Option<&'static dyn BlockIo> {
    IO.get().copied()
}

fn to_ssize(r: Result<usize>) -> isize {
    match r {
        Ok(n) => n as isize,
        Err(e) => -(e.code() as isize),
    }
}

unsafe extern "C" fn read(token: Id, offs: i64, buf: *mut c_char, len: usize) -> isize {
    if buf.is_null() {
        return -(Errno::EINVAL.code() as isize);
    }
    // SAFETY: libext2 passes a buffer valid for writing `len` bytes for the
    // call, which nothing else touches meanwhile.
    let buf = unsafe { core::slice::from_raw_parts_mut(buf.cast::<u8>(), len) };
    to_ssize(io().ok_or(Errno::ENODEV).and_then(|io| io.read(token, offs, buf)))
}

unsafe extern "C" fn write(token: Id, offs: i64, buf: *const c_char, len: usize) -> isize {
    if buf.is_null() {
        return -(Errno::EINVAL.code() as isize);
    }
    // SAFETY: libext2 passes a buffer valid for reading `len` bytes for the call.
    let data = unsafe { core::slice::from_raw_parts(buf.cast::<u8>(), len) };
    to_ssize(io().ok_or(Errno::ENODEV).and_then(|io| io.write(token, offs, data)))
}
