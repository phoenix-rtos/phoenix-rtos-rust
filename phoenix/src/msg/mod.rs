//! The message passing API (`<sys/msg.h>`).
//!
//! A Phoenix server owns a port, receives messages on it and responds to
//! them; a client sends a message to a port and blocks until the response.
//! This module wraps both sides:
//!
//! * [`Port`] owns a port. [`Port::serve`] runs the receive loop, decoding
//!   every message into a [`Request`] that borrows the message's buffers and
//!   sending back the [`Reply`] the handler returns.
//! * [`Message`] builds a request and sends it as a client.
//! * [`lookup`] resolves a path to object ids; [`Mount`] attaches a server to
//!   a directory for as long as it is alive.
//!
//! ```no_run
//! use phoenix::msg::{Mount, Port, Reply, Request};
//! use phoenix::Errno;
//!
//! let port = Port::new()?;
//! let _mount = Mount::new("/srv", port.oid(0))?;
//! port.serve(|req| match req {
//!     Request::Open { .. } | Request::Close { .. } => Ok(Reply::Done),
//!     Request::Read { buf, .. } => {
//!         buf.fill(b'x');
//!         Ok(Reply::Bytes(buf.len()))
//!     }
//!     _ => Err(Errno::ENOSYS),
//! })?;
//! # Ok::<(), Errno>(())
//! ```
//!
//! The `#[repr(C)]` layouts and `extern` declarations underneath are in
//! [`raw`], for whatever the safe layer does not cover yet (`mtDevCtl`, for
//! one, is passed through as [`Request::Other`]).

pub mod raw;

mod client;
mod server;
mod types;

pub use client::{lookup, Message, MsgType, Mount, Resolved, Response};
pub use raw::{AttrAll, Statvfs};
pub use server::{CreateKind, Dirent, DirentOut, Out, Plain, Port, Reply, Request};
pub use types::{AttrType, FileType, ObjectType, POLL_READY};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Errno, Oid, Result};
    use std::ffi::OsStr;
    use std::sync::Arc;

    /// A server on its own thread, closed and joined on drop.
    struct Server {
        port: Arc<Port>,
        thread: Option<std::thread::JoinHandle<Result<()>>>,
    }

    impl Server {
        fn start(handler: impl FnMut(Request<'_>) -> Result<Reply> + Send + 'static) -> Self {
            let port = Arc::new(Port::new().expect("portCreate"));
            let thread = std::thread::spawn({
                let port = port.clone();
                move || port.serve(handler)
            });
            Server { port, thread: Some(thread) }
        }

        fn stop(mut self) -> Result<()> {
            self.port.close();
            self.thread.take().unwrap().join().unwrap()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.port.close();
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// Answers with what it was sent, so the test sees both directions.
    fn echo(req: Request<'_>) -> Result<Reply> {
        match req {
            Request::Write { offset, data, .. } => {
                Ok(Reply::Written { len: data.len() + offset as usize, end: offset + 1000 })
            }
            Request::Read { offset, buf, .. } => {
                for (i, b) in buf.iter_mut().enumerate() {
                    *b = (offset as usize + i) as u8;
                }
                Ok(Reply::Bytes(buf.len()))
            }
            Request::Create { dir, name, mode, kind: CreateKind::Symlink(target) } => {
                assert_eq!((name, target, mode), (OsStr::new("ln"), OsStr::new("to/there"), 0o777));
                Ok(Reply::Created(Oid::new(dir.port, 42)))
            }
            Request::Create { kind: CreateKind::Device(dev), .. } => Ok(Reply::Created(dev)),
            Request::Lookup { dir, path } => {
                Ok(Reply::Lookup { consumed: path.len(), file: Oid::new(dir.port, 7), dev: Oid::new(1, 2) })
            }
            Request::GetAttr { attr: AttrType::Size, .. } => Ok(Reply::Attr(1234)),
            Request::GetAttr { attr: AttrType::Other(n), .. } => Ok(Reply::Attr(n.into())),
            Request::GetAttrAll { out, .. } => {
                let mut a = AttrAll::unsupported();
                a.set(AttrType::Links, 3);
                out.write(a).map(|()| Reply::Done)
            }
            Request::Readdir { offset: 0, out, .. } => out
                .write(Dirent { ino: 9, file_type: FileType::Regular, reclen: 1, name: OsStr::new("entry") })
                .map(|()| Reply::Done),
            Request::Readdir { .. } => Err(Errno::ENOENT),
            _ => Err(Errno::ENOSYS),
        }
    }

    #[test]
    fn requests_and_replies_cross_a_port() {
        let srv = Server::start(echo);
        let oid = srv.port.oid(0);

        // Payloads up to 64 bytes travel inside the message, larger ones are
        // mapped into the server: both, in both directions.
        for len in [5, 10_000] {
            let data = vec![0u8; len];
            let r = Message::new(MsgType::Write, oid).io(3, len).input(&data).send().unwrap();
            assert_eq!((r.result(), r.written_end()), (Ok(len + 3), 1003));

            let mut buf = vec![0u8; len];
            let r = Message::new(MsgType::Read, oid).io(100, len).output(&mut buf).send().unwrap();
            assert_eq!(r.result(), Ok(len));
            assert!(buf.iter().enumerate().all(|(i, &b)| b == (100 + i) as u8));
        }

        let r = Message::new(MsgType::Create, oid).create(ObjectType::Symlink, 0o777, Oid::default())
            .input(b"ln\0to/there\0").send().unwrap();
        assert_eq!((r.result(), r.created()), (Ok(0), Oid::new(oid.port, 42)));
        let dev = Oid::new(3, 4);
        let r = Message::new(MsgType::Create, oid).create(ObjectType::Device, 0o666, dev)
            .input(b"tty\0").send().unwrap();
        assert_eq!(r.created(), dev);

        let r = Message::new(MsgType::Lookup, oid).input(b"a/b\0").send().unwrap();
        assert_eq!(r.result(), Ok(3));
        assert_eq!(r.lookup(), Resolved { file: Oid::new(oid.port, 7), dev: Oid::new(1, 2) });

        let r = Message::new(MsgType::GetAttr, oid).attr(AttrType::Size, 0).send().unwrap();
        assert_eq!(r.attr(), 1234);
        let r = Message::new(MsgType::GetAttr, oid).attr(AttrType::Other(99), 0).send().unwrap();
        assert_eq!(r.attr(), 99);

        let mut attrs = [0u8; core::mem::size_of::<AttrAll>()];
        Message::new(MsgType::GetAttrAll, oid).output(&mut attrs).send().unwrap().result().unwrap();
        let attrs = AttrAll::from_ne_bytes(&attrs).unwrap();
        assert_eq!((attrs.get(AttrType::Links), attrs.get(AttrType::Mode)), (Ok(3), Err(Errno::ENOSYS)));
        // A buffer too small for the reply is refused, not overrun.
        let mut small = [0u8; 8];
        let r = Message::new(MsgType::GetAttrAll, oid).output(&mut small).send().unwrap();
        assert_eq!(r.result(), Err(Errno::EINVAL));

        let mut dent = [0u8; 300];
        let r = Message::new(MsgType::Readdir, oid).readdir_offset(0).output(&mut dent).send().unwrap();
        assert_eq!(r.result(), Ok(0));
        assert_eq!(u64::from_ne_bytes(dent[..8].try_into().unwrap()), 9);
        assert_eq!(&dent[16..22], b"entry\0");
        let r = Message::new(MsgType::Readdir, oid).readdir_offset(1).output(&mut dent).send().unwrap();
        assert_eq!(r.result(), Err(Errno::ENOENT));

        let r = Message::new(MsgType::Truncate, oid).send().unwrap();
        assert_eq!(r.result(), Err(Errno::ENOSYS));

        assert_eq!(srv.stop(), Ok(()), "close() ends serve() cleanly");
    }

    #[test]
    fn closed_port_refuses_messages() {
        let port = Port::new().unwrap();
        let oid = port.oid(0);
        port.close();
        port.close(); // idempotent
        assert!(Message::new(MsgType::Open, oid).send().is_err());
    }

    /// Just enough of a filesystem to be stat()ed: an empty root directory.
    fn empty_dir(req: Request<'_>) -> Result<Reply> {
        let mode = FileType::Directory.mode_bits() as i64 | 0o755;
        match req {
            Request::GetAttr { attr: AttrType::Mode, .. } => Ok(Reply::Attr(mode)),
            Request::GetAttr { attr: AttrType::Type, .. } => Ok(Reply::Attr(ObjectType::Dir.to_raw().into())),
            Request::GetAttr { .. } => Ok(Reply::Attr(0)),
            Request::GetAttrAll { out, .. } => {
                let mut a = AttrAll::unsupported();
                a.set(AttrType::Mode, mode);
                a.set(AttrType::Type, ObjectType::Dir.to_raw().into());
                out.write(a).map(|()| Reply::Done)
            }
            Request::Lookup { dir, path } if path.is_empty() => Ok(Reply::Lookup { consumed: 0, file: dir, dev: dir }),
            _ => Err(Errno::ENOSYS),
        }
    }

    #[test]
    fn mount_redirects_a_directory_until_dropped() {
        let dir = format!("/phoenix-mount-{}", std::process::id());
        std::fs::create_dir(&dir).unwrap();
        let srv = Server::start(empty_dir);
        let root = srv.port.oid(0);

        let before = lookup(&dir).unwrap();
        assert_eq!(before.file, before.dev);
        let mount = Mount::new(&dir, root).unwrap();
        assert_eq!(lookup(&dir).unwrap().dev, root);
        assert_eq!(Mount::new(&dir, root).err(), Some(Errno::EBUSY));
        drop(mount);
        assert_eq!(lookup(&dir).unwrap(), before);

        assert_eq!(Mount::new("/no/such/dir", root).err(), Some(Errno::ENOENT));
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn oid_bytes_roundtrip() {
        let oid = Oid::new(0xdead, 0x89ab_cdef);
        assert_eq!(Oid::from_ne_bytes(&oid.to_ne_bytes()), Some(oid));
        assert_eq!(Oid::from_ne_bytes(&[0; 3]), None);
    }
}
