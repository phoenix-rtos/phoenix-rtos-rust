use core::ffi::c_int;
use std::{fmt, io};

/// A Phoenix error number (positive, as in `errno`).
///
/// Messages carry errors as negative return values; [`Errno::check`] and the
/// reply encoding convert at that boundary, so nothing else deals in signs.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Errno(i32);

impl Errno {
    pub const EPERM: Errno = Errno(1);
    pub const ENOENT: Errno = Errno(2);
    pub const EINTR: Errno = Errno(4);
    pub const EIO: Errno = Errno(5);
    pub const ENOMEM: Errno = Errno(12);
    pub const EBUSY: Errno = Errno(16);
    pub const EEXIST: Errno = Errno(17);
    pub const EXDEV: Errno = Errno(18);
    pub const ENODEV: Errno = Errno(19);
    pub const ENOTDIR: Errno = Errno(20);
    pub const EISDIR: Errno = Errno(21);
    pub const EINVAL: Errno = Errno(22);
    pub const ENOSPC: Errno = Errno(28);
    pub const ENAMETOOLONG: Errno = Errno(36);
    pub const ENOSYS: Errno = Errno(38);
    pub const ENOTEMPTY: Errno = Errno(39);

    /// From an error code of either sign.
    pub const fn from_raw(code: i32) -> Self {
        Errno(code.unsigned_abs() as i32)
    }

    pub const fn code(self) -> i32 {
        self.0
    }

    /// Splits a libphoenix return value into success and error.
    pub fn check(ret: c_int) -> Result<c_int, Errno> {
        if ret < 0 { Err(Errno::from_raw(ret)) } else { Ok(ret) }
    }
}

impl fmt::Debug for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Errno({})", self.0)
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        io::Error::from(*self).fmt(f)
    }
}

impl std::error::Error for Errno {}

impl From<Errno> for io::Error {
    fn from(e: Errno) -> Self {
        io::Error::from_raw_os_error(e.0)
    }
}

/// For errors that come back from `std` (e.g. path resolution); anything
/// without an OS error code becomes `EIO`.
impl From<io::Error> for Errno {
    fn from(e: io::Error) -> Self {
        e.raw_os_error().map_or(Errno::EIO, Errno::from_raw)
    }
}
