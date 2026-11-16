use super::raw::{self, RawMsg};
use super::{AttrType, ObjectType};
use crate::{Errno, Oid, Result};
use core::marker::PhantomData;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Message types a client can send.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MsgType {
    Open = raw::MT_OPEN,
    Close = raw::MT_CLOSE,
    Read = raw::MT_READ,
    Write = raw::MT_WRITE,
    Truncate = raw::MT_TRUNCATE,
    DevCtl = raw::MT_DEVCTL,
    Create = raw::MT_CREATE,
    Destroy = raw::MT_DESTROY,
    SetAttr = raw::MT_SETATTR,
    GetAttr = raw::MT_GETATTR,
    GetAttrAll = raw::MT_GETATTRALL,
    Lookup = raw::MT_LOOKUP,
    Link = raw::MT_LINK,
    Unlink = raw::MT_UNLINK,
    Readdir = raw::MT_READDIR,
    Stat = raw::MT_STAT,
}

/// A request under construction, sent to the port of its oid.
///
/// The buffers it is given are borrowed until it is sent, so the server can
/// never be handed a dangling pointer:
///
/// ```no_run
/// # use phoenix::msg::*;
/// # use phoenix::{Errno, Oid};
/// # let file = Oid::default();
/// let mut buf = [0u8; 128];
/// let n = Message::new(MsgType::Read, file).io(0, buf.len()).output(&mut buf).send()?.result()?;
/// println!("{:?}", &buf[..n]);
/// # Ok::<(), Errno>(())
/// ```
pub struct Message<'a> {
    raw: RawMsg,
    _buffers: PhantomData<&'a mut [u8]>,
}

impl<'a> Message<'a> {
    pub fn new(msg_type: MsgType, oid: Oid) -> Self {
        let mut raw = RawMsg::zeroed();
        raw.typ = msg_type as i32;
        raw.oid = oid;
        Message { raw, _buffers: PhantomData }
    }

    /// Data for the server: written data, a name, a path.
    pub fn input(mut self, data: &'a [u8]) -> Self {
        self.raw.i.data = data.as_ptr().cast();
        self.raw.i.size = data.len();
        self
    }

    /// Room for the server's answer: read data, a `struct dirent`, ...
    pub fn output(mut self, buf: &'a mut [u8]) -> Self {
        self.raw.o.data = buf.as_mut_ptr().cast();
        self.raw.o.size = buf.len();
        self
    }

    /// Offset and length of a read, write or truncate.
    pub fn io(mut self, offset: i64, len: usize) -> Self {
        self.raw.i.args.io = raw::IoArgs { offs: offset, len, mode: 0 };
        self
    }

    pub fn create(mut self, kind: ObjectType, mode: u32, dev: Oid) -> Self {
        self.raw.i.args.create = raw::CreateArgs { typ: kind.to_raw(), mode, dev };
        self
    }

    pub fn attr(mut self, attr: AttrType, value: i64) -> Self {
        self.raw.i.args.attr = raw::AttrArgs { val: value, typ: attr.to_raw() };
        self
    }

    /// The object a link or unlink refers to.
    pub fn link_target(mut self, oid: Oid) -> Self {
        self.raw.i.args.ln_oid = oid;
        self
    }

    pub fn readdir_offset(mut self, offset: i64) -> Self {
        self.raw.i.args.readdir_offs = offset;
        self
    }

    /// Sends and waits for the reply. `Err` only if the message could not be
    /// delivered; the server's own verdict is [`Response::result`].
    pub fn send(mut self) -> Result<Response> {
        // SAFETY: the buffers are borrowed for 'a, which outlives this call.
        Errno::check(unsafe { raw::msgSend(self.raw.oid.port, &mut self.raw) })?;
        Ok(Response { err: self.raw.o.err, args: self.raw.o.args })
    }
}

/// A server's reply.
pub struct Response {
    err: i32,
    args: raw::OArgs,
}

impl Response {
    /// The server's verdict: a byte count or other non-negative value, or
    /// the error it reported.
    pub fn result(&self) -> Result<usize> {
        Errno::check(self.err).map(|n| n as usize)
    }

    // SAFETY (union reads below): every variant is plain integers, so any
    // bytes are a valid value - meaningful only for the matching request.

    /// The new object, after `MsgType::Create`.
    pub fn created(&self) -> Oid {
        unsafe { self.args.create_oid }
    }

    /// The offset after the written bytes, after `MsgType::Write`.
    pub fn written_end(&self) -> i64 {
        unsafe { self.args.io_offs }
    }

    /// The value, after `MsgType::GetAttr`.
    pub fn attr(&self) -> i64 {
        unsafe { self.args.attr_val }
    }

    /// The resolved objects, after `MsgType::Lookup`.
    pub fn lookup(&self) -> Resolved {
        let l = unsafe { self.args.lookup };
        Resolved { file: l.fil, dev: l.dev }
    }
}

/// A resolved path: the object itself, and the object that serves it - the
/// same unless it is a device node or a mountpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub file: Oid,
    pub dev: Oid,
}

/// Resolves an absolute path through the servers it crosses.
pub fn lookup(path: impl AsRef<Path>) -> Result<Resolved> {
    let path = CString::new(path.as_ref().as_os_str().as_bytes()).map_err(|_| Errno::EINVAL)?;
    let (mut file, mut dev) = (Oid::default(), Oid::default());
    // SAFETY: NUL-terminated path, valid out-pointers.
    Errno::check(unsafe { raw::lookup(path.as_ptr(), &mut file, &mut dev) })?;
    Ok(Resolved { file, dev })
}

/// A server mounted on a directory; unmounted when dropped.
///
/// Mounting sets the directory's device to the server's root, which is how
/// the directory's own filesystem learns to forward lookups below it.
#[derive(Debug)]
pub struct Mount {
    dir: Oid,
}

impl Mount {
    /// Mounts `root` on the existing directory `path`.
    pub fn new(path: impl AsRef<Path>, root: Oid) -> Result<Mount> {
        let path = std::fs::canonicalize(path)?;
        if !std::fs::metadata(&path)?.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let dir = lookup(&path)?;
        if dir.file != dir.dev {
            return Err(Errno::EBUSY);
        }
        set_dev(dir.file, root)?;
        Ok(Mount { dir: dir.file })
    }

    /// The directory mounted over.
    pub fn dir(&self) -> Oid {
        self.dir
    }

    /// Unmounts, reporting failure (dropping does the same, silently).
    pub fn unmount(self) -> Result<()> {
        let dir = self.dir;
        core::mem::forget(self);
        set_dev(dir, dir)
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let _ = set_dev(self.dir, self.dir);
    }
}

/// Setting a directory's device to itself is unmounting it.
fn set_dev(dir: Oid, dev: Oid) -> Result<()> {
    let dev = dev.to_ne_bytes();
    Message::new(MsgType::SetAttr, dir).attr(AttrType::Dev, 0).input(&dev).send()?.result().map(drop)
}
