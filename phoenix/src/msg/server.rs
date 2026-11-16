use super::raw::{self, RawMsg};
use super::{AttrAll, AttrType, FileType, ObjectType, Statvfs};
use crate::{Errno, Id, Oid, Result};
use core::ffi::{c_int, c_ulong, c_void};
use core::marker::PhantomData;
use core::mem::size_of;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};

/// A port this process owns; destroyed when dropped.
///
/// `Port` is `Sync`: several threads may [`serve`](Port::serve) it at once,
/// and any thread may [`close`](Port::close) it to stop them.
#[derive(Debug)]
pub struct Port {
    id: u32,
    closed: AtomicBool,
}

impl Port {
    pub fn new() -> Result<Port> {
        let mut id = 0;
        // SAFETY: `id` is a valid out-pointer.
        Errno::check(unsafe { raw::portCreate(&mut id) })?;
        Ok(Port { id, closed: AtomicBool::new(false) })
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// The oid of object `id` served on this port.
    pub fn oid(&self, id: Id) -> Oid {
        Oid::new(self.id, id)
    }

    /// Destroys the port: pending and future messages are refused and
    /// [`serve`](Port::serve) returns. Idempotent.
    pub fn close(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            // SAFETY: the port is ours and destroyed only once.
            unsafe { raw::portDestroy(self.id) };
        }
    }

    /// Receives messages and answers each with what `handler` returns, until
    /// the port is closed (`Ok`) or receiving fails (`Err`).
    ///
    /// The [`Request`] borrows the message's buffers - including the client's
    /// memory, which the kernel maps into this process for large transfers -
    /// so it cannot outlive the call.
    pub fn serve<F>(&self, mut handler: F) -> Result<()>
    where
        F: FnMut(Request<'_>) -> Result<Reply>,
    {
        while let Some(mut received) = self.receive()? {
            let reply = handler(received.request());
            received.reply(reply);
            // A failure means the sender is gone, which is no reason to stop.
            let _ = self.respond(received);
        }
        Ok(())
    }

    /// Receives one message, to be answered with [`respond`](Port::respond),
    /// possibly by another thread. `None` once the port is closed.
    pub fn receive(&self) -> Result<Option<Received>> {
        // Boxed so it cannot move while the kernel's pointers into it are live.
        let mut msg = Box::new(RawMsg::zeroed());
        loop {
            let mut rid = 0;
            // SAFETY: `msg` and `rid` are valid out-pointers.
            match Errno::check(unsafe { raw::msgRecv(self.id, &mut *msg, &mut rid) }) {
                Ok(_) => return Ok(Some(Received { msg, rid })),
                Err(Errno::EINTR) => continue,
                Err(_) if self.closed.load(Ordering::SeqCst) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    /// Sends back the response of a message this port received. `Err` if the
    /// sender is gone.
    pub fn respond(&self, mut received: Received) -> Result<()> {
        // SAFETY: `received.msg` is the message msgRecv() filled in for
        // `received.rid` on this port, still in place in its box.
        Errno::check(unsafe { raw::msgRespond(self.id, &mut *received.msg, received.rid) }).map(drop)
    }
}

/// A message received by [`Port::receive`]; its sender waits until it is
/// passed to [`Port::respond`], so it must not be dropped instead.
pub struct Received {
    msg: Box<RawMsg>,
    rid: c_int,
}

// SAFETY: the message's buffers belong to this process (the kernel maps the
// sender's memory in until the response), not to the receiving thread.
unsafe impl Send for Received {}

impl Received {
    /// The message decoded; its buffers borrow `self`.
    pub fn request(&mut self) -> Request<'_> {
        // SAFETY: filled in by msgRecv() and kept in place in its box; the
        // returned request borrows `self`, so nothing else touches it meanwhile.
        unsafe { Request::from_raw(&mut self.msg) }
    }

    /// Puts `reply` in the message, to be sent by [`Port::respond`].
    pub fn reply(&mut self, reply: Result<Reply>) {
        reply_into(&mut self.msg, reply);
    }

    /// The message as received, for handing to C code that takes a `msg_t`.
    pub fn raw(&mut self) -> &mut RawMsg {
        &mut self.msg
    }
}

impl core::fmt::Debug for Received {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Received {{ type: {}, rid: {} }}", self.msg.typ, self.rid)
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        self.close();
    }
}

/// What to create, with what only that kind of object needs.
#[derive(Debug, PartialEq, Eq)]
pub enum CreateKind<'a> {
    Dir,
    File,
    /// A device node standing for the given object.
    Device(Oid),
    /// A symbolic link to the given target.
    Symlink(&'a OsStr),
    /// `otUnknown` or a code this crate does not know.
    Other(i32),
}

/// A decoded message. Buffers borrow the message and, for large transfers,
/// the client's memory mapped into this process.
#[derive(Debug)]
pub enum Request<'a> {
    Open { oid: Oid, flags: u32 },
    Close { oid: Oid, flags: u32 },
    /// Reply with [`Reply::Bytes`], the number of bytes put in `buf`.
    Read { oid: Oid, offset: i64, buf: &'a mut [u8] },
    /// Reply with [`Reply::Written`]. With `append` (a file opened with
    /// `O_APPEND`), `data` goes at the end of the file instead of `offset`.
    Write { oid: Oid, offset: i64, append: bool, data: &'a [u8] },
    Truncate { oid: Oid, len: usize },
    /// Reply with [`Reply::Created`].
    Create { dir: Oid, name: &'a OsStr, mode: u32, kind: CreateKind<'a> },
    Destroy { oid: Oid },
    /// `data` carries what does not fit in a number: the oid to mount for
    /// [`AttrType::Dev`] (see [`Oid::from_ne_bytes`]).
    SetAttr { oid: Oid, attr: AttrType, value: i64, data: &'a [u8] },
    /// Reply with [`Reply::Attr`].
    GetAttr { oid: Oid, attr: AttrType },
    GetAttrAll { oid: Oid, out: Out<'a, AttrAll> },
    /// Resolve `path` (relative, `/`-separated) from `dir`. Reply with
    /// [`Reply::Lookup`].
    Lookup { dir: Oid, path: &'a OsStr },
    Link { dir: Oid, name: &'a OsStr, target: Oid },
    Unlink { dir: Oid, name: &'a OsStr },
    /// Fill `out` with the entry at directory position `offset`; the client
    /// advances its position by the entry's [`Dirent::reclen`]. `ENOENT`
    /// marks the end.
    Readdir { dir: Oid, offset: i64, out: DirentOut<'a> },
    StatFs { oid: Oid, out: Out<'a, Statvfs> },
    /// Mount a filesystem of type `fstype` from the device `oid`. Reply with
    /// [`Reply::Mounted`].
    Mount { oid: Oid, fstype: &'a OsStr, mode: c_ulong },
    /// `mtDevCtl`, or a message type this crate does not decode.
    Other { msg_type: i32, oid: Oid },
}

impl<'a> Request<'a> {
    /// # Safety
    /// `msg` must be as `msgRecv()` left it: `i.data`/`o.data` null or valid
    /// for `i.size`/`o.size` bytes, not overlapping each other.
    unsafe fn from_raw(msg: &'a mut RawMsg) -> Request<'a> {
        // Made from the raw pointers, so they do not borrow `msg` itself;
        // nothing touches `msg` while the request is alive.
        let data: &'a [u8] = unsafe { slice(msg.i.data, msg.i.size) };
        let buf: &'a mut [u8] = unsafe { slice_mut(msg.o.data, msg.o.size) };
        let oid = msg.oid;
        let args = msg.i.args;

        // SAFETY (union reads): every variant is plain integers.
        unsafe {
            match msg.typ {
                raw::MT_OPEN => Request::Open { oid, flags: args.openclose_flags },
                raw::MT_CLOSE => Request::Close { oid, flags: args.openclose_flags },
                raw::MT_READ => Request::Read { oid, offset: args.io.offs, buf },
                raw::MT_WRITE => {
                    Request::Write { oid, offset: args.io.offs, append: args.io.mode & raw::O_APPEND != 0, data }
                }
                raw::MT_TRUNCATE => Request::Truncate { oid, len: args.io.len },
                raw::MT_CREATE => {
                    let c = args.create;
                    let (name, rest) = split_cstr(data);
                    let kind = match ObjectType::from_raw(c.typ) {
                        Some(ObjectType::Dir) => CreateKind::Dir,
                        Some(ObjectType::File) => CreateKind::File,
                        Some(ObjectType::Device) => CreateKind::Device(c.dev),
                        // libphoenix's symlink() sends "name\0target\0".
                        Some(ObjectType::Symlink) => CreateKind::Symlink(split_cstr(rest).0),
                        Some(ObjectType::Unknown) | None => CreateKind::Other(c.typ),
                    };
                    Request::Create { dir: oid, name, mode: c.mode, kind }
                }
                raw::MT_DESTROY => Request::Destroy { oid },
                raw::MT_SETATTR => {
                    Request::SetAttr { oid, attr: AttrType::from_raw(args.attr.typ), value: args.attr.val, data }
                }
                raw::MT_GETATTR => Request::GetAttr { oid, attr: AttrType::from_raw(args.attr.typ) },
                raw::MT_GETATTRALL => Request::GetAttrAll { oid, out: Out::new(buf) },
                raw::MT_LOOKUP => Request::Lookup { dir: oid, path: split_cstr(data).0 },
                raw::MT_LINK => Request::Link { dir: oid, name: split_cstr(data).0, target: args.ln_oid },
                raw::MT_UNLINK => Request::Unlink { dir: oid, name: split_cstr(data).0 },
                raw::MT_READDIR => Request::Readdir { dir: oid, offset: args.readdir_offs, out: DirentOut::new(buf) },
                raw::MT_STAT => Request::StatFs { oid, out: Out::new(buf) },
                raw::MT_MOUNT => {
                    // From the message itself (not the copy in `args`), so
                    // the name can borrow it like `data` does.
                    let fstype: &'a [u8] = slice((&raw const msg.i.args.mount.fstype).cast(), 16);
                    Request::Mount { oid, fstype: split_cstr(fstype).0, mode: args.mount.mode }
                }
                msg_type => Request::Other { msg_type, oid },
            }
        }
    }
}

/// The successful answer to a [`Request`]; errors are the `Err` beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    Done,
    Bytes(usize),
    /// `len` bytes taken from `data`, ending at offset `end`: the client's
    /// file offset after an `append` write.
    Written { len: usize, end: i64 },
    Created(Oid),
    Attr(i64),
    /// `consumed` bytes of the path were resolved to `file`. When the walk
    /// stops early at a mountpoint or device, `dev` is where the kernel
    /// continues with the rest; otherwise it is `file` itself.
    Lookup { consumed: usize, file: Oid, dev: Oid },
    /// The root of what a [`Request::Mount`] mounted.
    Mounted(Oid),
}

fn reply_into(msg: &mut RawMsg, reply: Result<Reply>) {
    let count = |n: usize| i32::try_from(n).unwrap_or(i32::MAX);
    msg.o.err = match reply {
        Err(e) => -e.code(),
        Ok(Reply::Done) => 0,
        Ok(Reply::Bytes(n)) => count(n),
        Ok(Reply::Written { len, end }) => {
            msg.o.args.io_offs = end;
            count(len)
        }
        Ok(Reply::Created(oid)) => {
            msg.o.args.create_oid = oid;
            0
        }
        Ok(Reply::Mounted(oid)) => {
            msg.o.args.mount_oid = oid;
            0
        }
        Ok(Reply::Attr(val)) => {
            msg.o.args.attr_val = val;
            0
        }
        Ok(Reply::Lookup { consumed, file, dev }) => {
            msg.o.args.lookup = raw::LookupOut { fil: file, dev };
            count(consumed)
        }
    };
}

/// A reply buffer for a C structure; [`write`](Out::write) checks it fits.
pub struct Out<'a, T> {
    buf: &'a mut [u8],
    _type: PhantomData<T>,
}

/// Types an [`Out`] can carry: the plain `#[repr(C)]` replies.
pub trait Plain: Copy + private::Sealed {}
impl Plain for AttrAll {}
impl Plain for Statvfs {}
mod private {
    pub trait Sealed {}
    impl Sealed for super::AttrAll {}
    impl Sealed for super::Statvfs {}
}

impl<'a, T: Plain> Out<'a, T> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Out { buf, _type: PhantomData }
    }

    /// `EINVAL` if the client's buffer is too small for a `T`.
    pub fn write(self, value: T) -> Result<()> {
        if self.buf.len() < size_of::<T>() {
            return Err(Errno::EINVAL);
        }
        // SAFETY: the buffer has room for a T; the write may be unaligned.
        unsafe { core::ptr::write_unaligned(self.buf.as_mut_ptr().cast::<T>(), value) };
        Ok(())
    }
}

impl<T> core::fmt::Debug for Out<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Out<{}>({} bytes)", core::any::type_name::<T>(), self.buf.len())
    }
}

/// A directory entry for a [`Request::Readdir`] reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dirent<'a> {
    pub ino: u64,
    pub file_type: FileType,
    /// How far the client advances its directory position past this entry.
    pub reclen: u16,
    pub name: &'a OsStr,
}

/// The reply buffer of a [`Request::Readdir`]: one `struct dirent`.
#[derive(Debug)]
pub struct DirentOut<'a> {
    buf: &'a mut [u8],
}

impl<'a> DirentOut<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        DirentOut { buf }
    }

    /// `EINVAL` if the entry does not fit the client's buffer.
    pub fn write(self, d: Dirent<'_>) -> Result<()> {
        let name = d.name.as_bytes();
        let namlen = u16::try_from(name.len()).map_err(|_| Errno::ENAMETOOLONG)?;
        let end = raw::DIRENT_HEADER + name.len();
        let out = self.buf.get_mut(..end + 1).ok_or(Errno::EINVAL)?;
        out[0..8].copy_from_slice(&d.ino.to_ne_bytes());
        out[8..12].copy_from_slice(&d.file_type.dirent_type().to_ne_bytes());
        out[12..14].copy_from_slice(&d.reclen.to_ne_bytes());
        out[14..16].copy_from_slice(&namlen.to_ne_bytes());
        out[raw::DIRENT_HEADER..end].copy_from_slice(name);
        out[end] = 0;
        Ok(())
    }
}

/// The first NUL-terminated string in `data` (all of it, if there is no
/// NUL), and what follows.
fn split_cstr(data: &[u8]) -> (&OsStr, &[u8]) {
    match data.iter().position(|&c| c == 0) {
        Some(p) => (OsStr::from_bytes(&data[..p]), &data[p + 1..]),
        None => (OsStr::from_bytes(data), &[]),
    }
}

/// # Safety
/// `ptr` null or valid for reading `len` bytes for `'a`, unmodified meanwhile.
unsafe fn slice<'a>(ptr: *const c_void, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 { &[] } else { unsafe { core::slice::from_raw_parts(ptr.cast(), len) } }
}

/// # Safety
/// `ptr` null or valid for reading and writing `len` bytes for `'a`, with no
/// other access meanwhile.
unsafe fn slice_mut<'a>(ptr: *mut c_void, len: usize) -> &'a mut [u8] {
    if ptr.is_null() || len == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(ptr.cast(), len) } }
}
