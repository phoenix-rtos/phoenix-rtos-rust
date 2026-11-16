//! The C ABI: `#[repr(C)]` mirrors of `msg_t` and the structures exchanged in
//! messages, and the libphoenix functions.
//!
//! Layouts follow `phoenix-rtos-kernel/include/msg.h`,
//! `posix-statvfs.h` and libphoenix's `dirent.h`. The sizes and offsets
//! asserted at the bottom were taken from each target's cross compiler; a
//! mismatch would turn every message into garbage, so it fails the build
//! instead.

use super::AttrType;
use crate::{Errno, Oid, Result};
use core::ffi::{c_char, c_int, c_ulong, c_void};
use core::mem::{offset_of, size_of};

/* Message types (msg.h) */
pub const MT_OPEN: c_int = 0;
pub const MT_CLOSE: c_int = 1;
pub const MT_READ: c_int = 2;
pub const MT_WRITE: c_int = 3;
pub const MT_TRUNCATE: c_int = 4;
pub const MT_DEVCTL: c_int = 5;
pub const MT_CREATE: c_int = 6;
pub const MT_DESTROY: c_int = 7;
pub const MT_SETATTR: c_int = 8;
pub const MT_GETATTR: c_int = 9;
pub const MT_GETATTRALL: c_int = 10;
pub const MT_LOOKUP: c_int = 11;
pub const MT_LINK: c_int = 12;
pub const MT_UNLINK: c_int = 13;
pub const MT_READDIR: c_int = 14;
pub const MT_STAT: c_int = 0xf53;
/* <sys/file.h> */
pub const MT_MOUNT: c_int = 0xf50;
pub const MT_UMOUNT: c_int = 0xf51;

/// `i.io.mode` of a write to a file opened with `O_APPEND` (posix-fcntl.h).
pub const O_APPEND: u32 = 0x8;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IoArgs {
    pub offs: i64,
    pub len: usize,
    pub mode: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CreateArgs {
    pub typ: c_int,
    pub mode: u32,
    pub dev: Oid,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AttrArgs {
    pub val: i64,
    pub typ: c_int,
}

/// The request arguments: the anonymous union in `msg_t.i`.
#[repr(C)]
#[derive(Clone, Copy)]
pub union IArgs {
    pub openclose_flags: u32,
    pub io: IoArgs,
    pub create: CreateArgs,
    pub attr: AttrArgs,
    pub ln_oid: Oid,
    pub readdir_offs: i64,
    pub mount: MountArgs,
    pub raw: [u8; 64],
}

#[repr(C)]
pub struct MsgIn {
    pub args: IArgs,
    pub size: usize,
    pub data: *const c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LookupOut {
    pub fil: Oid,
    pub dev: Oid,
}

/// `mount_i_msg_t` (`<sys/file.h>`): what `mtMount` carries in `i.raw`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MountArgs {
    pub mnt: Oid,
    pub mode: c_ulong,
    pub fstype: [c_char; 16],
}

/// The reply arguments: the anonymous union in `msg_t.o`.
#[repr(C)]
#[derive(Clone, Copy)]
pub union OArgs {
    pub attr_val: i64,
    /// `o.io.offs`: the offset after the bytes a write took.
    pub io_offs: i64,
    pub create_oid: Oid,
    /// `mount_o_msg_t`: the root of what `mtMount` mounted.
    pub mount_oid: Oid,
    pub lookup: LookupOut,
    pub raw: [u8; 64],
}

#[repr(C)]
pub struct MsgOut {
    pub args: OArgs,
    pub err: c_int,
    pub size: usize,
    pub data: *mut c_void,
}

/// `msg_t`.
///
/// Must not move between `msgRecv()` and `msgRespond()`: a payload that fits
/// in the 64-byte union is copied there by the kernel, which then points
/// `i.data`/`o.data` into this very struct.
#[repr(C)]
pub struct RawMsg {
    pub typ: c_int,
    pub pid: c_int,
    pub priority: c_int,
    pub oid: Oid,
    pub i: MsgIn,
    pub o: MsgOut,
}

impl RawMsg {
    pub fn zeroed() -> Self {
        // SAFETY: integers, raw pointers and unions of those; all-zero is
        // valid for each.
        unsafe { core::mem::zeroed() }
    }
}

/// `struct _attr`
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attr {
    pub val: i64,
    pub err: c_int,
}

/// `struct _attrAll`: every attribute of an object at once, each with its own
/// error code. The reply to `mtGetAttrAll`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttrAll {
    pub mode: Attr,
    pub uid: Attr,
    pub gid: Attr,
    pub size: Attr,
    pub blocks: Attr,
    pub ioblock: Attr,
    pub typ: Attr,
    pub port: Attr,
    pub poll_status: Attr,
    pub event_mask: Attr,
    pub ctime: Attr,
    pub mtime: Attr,
    pub atime: Attr,
    pub links: Attr,
    pub dev: Attr,
}

impl AttrAll {
    /// All attributes marked unsupported, ready to be filled in with [`set`](Self::set).
    pub fn unsupported() -> Self {
        let none = Attr { val: 0, err: -Errno::ENOSYS.code() };
        AttrAll {
            mode: none,
            uid: none,
            gid: none,
            size: none,
            blocks: none,
            ioblock: none,
            typ: none,
            port: none,
            poll_status: none,
            event_mask: none,
            ctime: none,
            mtime: none,
            atime: none,
            links: none,
            dev: none,
        }
    }

    fn slot(&mut self, attr: AttrType) -> Option<&mut Attr> {
        Some(match attr {
            AttrType::Mode => &mut self.mode,
            AttrType::Uid => &mut self.uid,
            AttrType::Gid => &mut self.gid,
            AttrType::Size => &mut self.size,
            AttrType::Blocks => &mut self.blocks,
            AttrType::IoBlock => &mut self.ioblock,
            AttrType::Type => &mut self.typ,
            AttrType::Port => &mut self.port,
            AttrType::PollStatus => &mut self.poll_status,
            AttrType::EventMask => &mut self.event_mask,
            AttrType::CTime => &mut self.ctime,
            AttrType::MTime => &mut self.mtime,
            AttrType::ATime => &mut self.atime,
            AttrType::Links => &mut self.links,
            AttrType::Dev => &mut self.dev,
            AttrType::Other(_) => return None,
        })
    }

    /// Marks `attr` supported with value `val`. `AttrType::Other` has no
    /// slot and is ignored.
    pub fn set(&mut self, attr: AttrType, val: i64) {
        if let Some(a) = self.slot(attr) {
            *a = Attr { val, err: 0 };
        }
    }

    pub fn get(&self, attr: AttrType) -> Result<i64> {
        let mut all = *self; // `slot` hands out &mut; reading a copy is cheap
        let a = *all.slot(attr).ok_or(Errno::EINVAL)?;
        if a.err < 0 { Err(Errno::from_raw(a.err)) } else { Ok(a.val) }
    }

    /// Reads the structure back out of a reply buffer.
    pub fn from_ne_bytes(b: &[u8]) -> Option<Self> {
        // SAFETY: `AttrAll` is plain integers, so any bytes are a valid value.
        (b.len() >= size_of::<Self>()).then(|| unsafe { core::ptr::read_unaligned(b.as_ptr().cast()) })
    }
}

/// `struct statvfs`: the reply to `mtStat`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Statvfs {
    pub f_bsize: c_ulong,
    pub f_frsize: c_ulong,
    pub f_blocks: u64,
    pub f_bfree: u64,
    pub f_bavail: u64,
    pub f_files: u64,
    pub f_ffree: u64,
    pub f_favail: u64,
    pub f_fsid: c_ulong,
    pub f_flag: c_ulong,
    pub f_namemax: c_ulong,
}

/// Size of `struct dirent` without its flexible `d_name`:
/// `d_ino` (u64), `d_type` (u32), `d_reclen` (u16), `d_namlen` (u16).
pub const DIRENT_HEADER: usize = 16;

unsafe extern "C" {
    pub fn portCreate(port: *mut u32) -> c_int;
    pub fn portDestroy(port: u32);
    pub fn lookup(name: *const c_char, file: *mut Oid, dev: *mut Oid) -> c_int;
    pub fn msgSend(port: u32, m: *mut RawMsg) -> c_int;
    pub fn msgRecv(port: u32, m: *mut RawMsg, rid: *mut c_int) -> c_int;
    pub fn msgRespond(port: u32, m: *mut RawMsg, rid: c_int) -> c_int;
}

/// `msg_t`'s layout per target, from its C compiler: the size, then the
/// offsets of `oid`, `i`, `i.data`, `o`, `o.err` and `o.data`; then the sizes
/// of `struct _attrAll` and `struct statvfs`.
#[cfg(target_pointer_width = "64")]
const LAYOUT: [usize; 9] = [200, 16, 32, 104, 112, 176, 192, 240, 88];
/// 64-bit integers 4-aligned.
#[cfg(target_arch = "x86")]
const LAYOUT: [usize; 9] = [172, 12, 24, 92, 96, 160, 168, 180, 68];
/// 64-bit integers 8-aligned, a 64-bit `id_t` (armv7a).
#[cfg(all(target_arch = "arm", not(target_env = "libphoenix_nommu")))]
const LAYOUT: [usize; 9] = [184, 16, 32, 100, 104, 168, 176, 240, 72];
/// 64-bit integers 8-aligned, a 32-bit `id_t`.
#[cfg(any(target_env = "libphoenix_nommu", target_arch = "sparc"))]
const LAYOUT: [usize; 9] = [176, 12, 24, 92, 96, 160, 168, 240, 72];

const _: () = {
    assert!(size_of::<RawMsg>() == LAYOUT[0]);
    assert!(offset_of!(RawMsg, oid) == LAYOUT[1]);
    assert!(offset_of!(RawMsg, i) == LAYOUT[2]);
    assert!(offset_of!(RawMsg, i) + offset_of!(MsgIn, data) == LAYOUT[3]);
    assert!(offset_of!(RawMsg, o) == LAYOUT[4]);
    assert!(offset_of!(RawMsg, o) + offset_of!(MsgOut, err) == LAYOUT[5]);
    assert!(offset_of!(RawMsg, o) + offset_of!(MsgOut, data) == LAYOUT[6]);
    assert!(size_of::<AttrAll>() == LAYOUT[7]);
    assert!(size_of::<Statvfs>() == LAYOUT[8]);
    assert!(offset_of!(CreateArgs, dev) == 8);
};
