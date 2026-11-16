//! libphoenix: device files and registered names.

use core::ffi::{c_char, c_int};
use phoenix::{Errno, Oid, Result};
use std::ffi::CString;

unsafe extern "C" {
    fn create_dev(oid: *mut Oid, path: *const c_char) -> c_int;
    fn portRegister(port: u32, name: *const c_char, oid: *mut Oid) -> c_int;
}

/// libphoenix returns a negative errno here, or -1 with `errno` set.
fn check(ret: c_int) -> Result<()> {
    match ret {
        0.. => Ok(()),
        -1 => Err(std::io::Error::last_os_error().into()),
        e => Err(Errno::from_raw(e)),
    }
}

/// Creates the device file `path` for `oid`.
pub fn create_device(oid: Oid, path: &str) -> Result<()> {
    let path = CString::new(path).map_err(|_| Errno::EINVAL)?;
    let mut oid = oid;
    // SAFETY: both pointers are valid for the call; create_dev copies what
    // it keeps.
    check(unsafe { create_dev(&mut oid, path.as_ptr()) })
}

/// Registers `oid` under `name` in the kernel's namespace (`"/"`: the root).
pub fn register(name: &str, oid: Oid) -> Result<()> {
    let name = CString::new(name).map_err(|_| Errno::EINVAL)?;
    let mut oid = oid;
    // SAFETY: both pointers are valid for the call; the kernel copies them.
    check(unsafe { portRegister(oid.port, name.as_ptr(), &mut oid) })
}
