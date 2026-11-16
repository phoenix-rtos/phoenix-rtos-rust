//! rsramfs - a RAM filesystem served over Phoenix-RTOS messages.
//!
//! Mounted on a directory - created if it does not exist - it is used through
//! the ordinary file API. Start it from `/etc/rc.psh` like any other server, e.g.
//!
//! ```text
//! X /sbin/rsramfs -m /ram -s 1024
//! ```

use phoenix::msg::{AttrAll, AttrType, CreateKind, Dirent, FileType, Mount, Port, Reply, Request, Statvfs, POLL_READY};
use phoenix::{Errno, Oid, Result};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

type Ino = phoenix::Id;

const ROOT: Ino = 0;
const NAME_MAX: usize = 255;
const PERMS: u32 = 0o7777;
const BLOCK_SIZE: i64 = 512;
const IO_BLOCK: i64 = 4096;

enum Content {
    Dir(Dir),
    File(Vec<u8>),
    Symlink(Vec<u8>),
    Device { kind: FileType, dev: Oid },
}

struct Dir {
    parent: Ino,
    entries: BTreeMap<OsString, Ino>,
    /// Another server's root, when something is mounted here.
    mount: Option<Oid>,
}

struct Inode {
    content: Content,
    perms: u32,
    uid: i64,
    gid: i64,
    /// Directory entries naming this inode; `.` and `..` are not counted.
    links: u32,
    /// Opens not yet closed. Together with `links`, keeps the inode alive.
    opens: u32,
    atime: i64,
    mtime: i64,
    ctime: i64,
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Inode {
    fn new(content: Content, perms: u32) -> Self {
        let t = now();
        Inode { content, perms: perms & PERMS, uid: 0, gid: 0, links: 0, opens: 0, atime: t, mtime: t, ctime: t }
    }

    fn file_type(&self) -> FileType {
        match self.content {
            Content::Dir(_) => FileType::Directory,
            Content::File(_) => FileType::Regular,
            Content::Symlink(_) => FileType::Symlink,
            Content::Device { kind, .. } => kind,
        }
    }

    fn dir(&self) -> Result<&Dir> {
        match &self.content {
            Content::Dir(d) => Ok(d),
            _ => Err(Errno::ENOTDIR),
        }
    }

    fn dir_mut(&mut self) -> Result<&mut Dir> {
        match &mut self.content {
            Content::Dir(d) => Ok(d),
            _ => Err(Errno::ENOTDIR),
        }
    }

    /// Bytes held in memory, which count against the capacity.
    fn data_len(&self) -> usize {
        match &self.content {
            Content::File(b) | Content::Symlink(b) => b.len(),
            Content::Dir(_) | Content::Device { .. } => 0,
        }
    }

    fn size(&self) -> usize {
        match &self.content {
            Content::Dir(d) => d.entries.len() + 2,
            _ => self.data_len(),
        }
    }

    /// The server a lookup reaching this inode is handed over to, if any.
    fn redirect(&self) -> Option<Oid> {
        match &self.content {
            Content::Dir(d) => d.mount,
            Content::Device { dev, .. } => Some(*dev),
            _ => None,
        }
    }
}

fn check_name(name: &OsStr) -> Result<()> {
    match name.as_bytes() {
        [] | b"." | b".." => Err(Errno::EINVAL),
        n if n.contains(&b'/') => Err(Errno::EINVAL),
        n if n.len() > NAME_MAX => Err(Errno::ENAMETOOLONG),
        _ => Ok(()),
    }
}

pub struct RamFs {
    port: u32,
    inodes: BTreeMap<Ino, Inode>,
    next_ino: Ino,
    /// Bytes of file and symlink contents held, at most `capacity`.
    used: usize,
    capacity: usize,
}

impl RamFs {
    /// An empty filesystem for objects on `port`, holding at most `capacity`
    /// bytes of file contents.
    pub fn new(port: u32, capacity: usize) -> Self {
        let root = Dir { parent: ROOT, entries: BTreeMap::new(), mount: None };
        let mut root = Inode::new(Content::Dir(root), 0o777);
        root.links = 1; // never freed
        RamFs { port, inodes: BTreeMap::from([(ROOT, root)]), next_ino: ROOT + 1, used: 0, capacity }
    }

    pub fn oid(&self, ino: Ino) -> Oid {
        Oid::new(self.port, ino)
    }

    /// Maps a request onto the filesystem.
    pub fn handle(&mut self, req: Request<'_>) -> Result<Reply> {
        let done = |()| Reply::Done;
        match req {
            Request::Open { oid, .. } => self.open(self.ino(oid)?).map(done),
            Request::Close { oid, .. } => self.close(self.ino(oid)?).map(done),
            Request::Read { oid, offset, buf } => self.read(self.ino(oid)?, offset, buf).map(Reply::Bytes),
            Request::Write { oid, offset, append, data } => {
                let ino = self.ino(oid)?;
                let offset = if append { self.file(ino)?.len() as i64 } else { offset };
                let len = self.write(ino, offset, data)?;
                Ok(Reply::Written { len, end: offset + len as i64 })
            }
            Request::Truncate { oid, len } => self.truncate(self.ino(oid)?, len).map(done),
            Request::Create { dir, name, mode, kind } => {
                let ino = self.create(self.ino(dir)?, name, mode, kind)?;
                Ok(Reply::Created(self.oid(ino)))
            }
            Request::Destroy { oid } => self.destroy(self.ino(oid)?).map(done),
            Request::SetAttr { oid, attr, value, data } => self.setattr(self.ino(oid)?, attr, value, data).map(done),
            Request::GetAttr { oid, attr } => self.getattr(self.ino(oid)?, attr).map(Reply::Attr),
            Request::GetAttrAll { oid, out } => out.write(self.getattr_all(self.ino(oid)?)?).map(done),
            Request::Lookup { dir, path } => {
                let (consumed, ino) = self.lookup(self.ino(dir)?, path.as_bytes())?;
                let file = self.oid(ino);
                let dev = self.inodes[&ino].redirect().unwrap_or(file);
                Ok(Reply::Lookup { consumed, file, dev })
            }
            Request::Link { dir, name, target } => self.link(self.ino(dir)?, name, self.ino(target)?).map(done),
            Request::Unlink { dir, name } => self.unlink(self.ino(dir)?, name).map(done),
            Request::Readdir { dir, offset, out } => out.write(self.readdir(self.ino(dir)?, offset)?).map(done),
            Request::StatFs { out, .. } => out.write(self.statfs()).map(done),
            Request::Mount { .. } | Request::Other { .. } => Err(Errno::ENOSYS),
        }
    }

    /// The inode an oid names, if it is one of ours.
    fn ino(&self, oid: Oid) -> Result<Ino> {
        if oid.port != self.port {
            return Err(Errno::EXDEV);
        }
        Ok(oid.id)
    }

    fn inode(&self, ino: Ino) -> Result<&Inode> {
        self.inodes.get(&ino).ok_or(Errno::ENOENT)
    }

    fn inode_mut(&mut self, ino: Ino) -> Result<&mut Inode> {
        self.inodes.get_mut(&ino).ok_or(Errno::ENOENT)
    }

    /// Frees an inode nothing refers to any more.
    fn release(&mut self, ino: Ino) {
        if self.inodes.get(&ino).is_some_and(|n| n.links == 0 && n.opens == 0) {
            let n = self.inodes.remove(&ino).unwrap();
            self.used -= n.data_len();
        }
    }

    /// Walks `path` from `dir`. Returns how much of it was resolved, and to
    /// what: the walk stops early at a mountpoint or a device, where the
    /// kernel carries on with the server behind it.
    pub fn lookup(&self, dir: Ino, path: &[u8]) -> Result<(usize, Ino)> {
        let mut ino = dir;
        let mut pos = 0;
        loop {
            let rest = &path[pos..];
            let start = rest.iter().take_while(|&&c| c == b'/').count();
            if start == rest.len() {
                return Ok((path.len(), ino));
            }
            let len = rest[start..].iter().position(|&c| c == b'/').unwrap_or(rest.len() - start);
            let d = self.inode(ino)?.dir()?;
            ino = match &rest[start..start + len] {
                b"." => ino,
                b".." => d.parent,
                name => *d.entries.get(OsStr::from_bytes(name)).ok_or(Errno::ENOENT)?,
            };
            pos += start + len;
            if self.inode(ino)?.redirect().is_some() {
                return Ok((pos, ino));
            }
        }
    }

    pub fn create(&mut self, dir: Ino, name: &OsStr, mode: u32, kind: CreateKind<'_>) -> Result<Ino> {
        check_name(name)?;
        if self.inode(dir)?.dir()?.entries.contains_key(name) {
            return Err(Errno::EEXIST);
        }
        let content = match kind {
            CreateKind::Dir => Content::Dir(Dir { parent: dir, entries: BTreeMap::new(), mount: None }),
            CreateKind::File => Content::File(Vec::new()),
            CreateKind::Symlink(target) => {
                self.reserve(target.len())?;
                self.used += target.len();
                Content::Symlink(target.as_bytes().to_vec())
            }
            CreateKind::Device(dev) => {
                let kind = FileType::from_mode(mode)
                    .filter(|t| matches!(t, FileType::CharDevice | FileType::BlockDevice | FileType::Fifo))
                    .unwrap_or(FileType::CharDevice);
                Content::Device { kind, dev }
            }
            CreateKind::Other(_) => return Err(Errno::EINVAL),
        };

        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(ino, Inode::new(content, mode));
        self.link(dir, name, ino).inspect_err(|_| self.release(ino))?;
        Ok(ino)
    }

    /// Names `ino` as `name` in `dir`, replacing an existing non-directory
    /// of that name.
    ///
    /// libphoenix implements `rename()` as `link()` then `unlink()`, so this
    /// is also the first half of a rename: a directory may briefly have two
    /// names, and is re-parented under the new one.
    pub fn link(&mut self, dir: Ino, name: &OsStr, ino: Ino) -> Result<()> {
        check_name(name)?;
        let is_dir = matches!(self.inode(ino)?.content, Content::Dir(_));
        if is_dir && self.is_within(dir, ino)? {
            return Err(Errno::EINVAL);
        }
        let replaced = match self.inode(dir)?.dir()?.entries.get(name) {
            Some(&old) if old == ino => return Err(Errno::EEXIST),
            Some(&old) if matches!(self.inode(old)?.content, Content::Dir(_)) => return Err(Errno::EEXIST),
            other => other.copied(),
        };

        let t = now();
        let parent = self.inode_mut(dir)?;
        parent.dir_mut()?.entries.insert(name.to_owned(), ino);
        parent.mtime = t;
        let n = self.inode_mut(ino)?;
        n.links += 1;
        n.ctime = t;
        if let Content::Dir(d) = &mut n.content {
            d.parent = dir;
        }
        if let Some(old) = replaced {
            self.inode_mut(old)?.links -= 1;
            self.release(old);
        }
        Ok(())
    }

    /// Whether `ino` is `dir` or one of its ancestors.
    fn is_within(&self, mut dir: Ino, ino: Ino) -> Result<bool> {
        loop {
            if dir == ino {
                return Ok(true);
            }
            if dir == ROOT {
                return Ok(false);
            }
            dir = self.inode(dir)?.dir()?.parent;
        }
    }

    /// Removes the name; the inode goes once nothing else refers to it.
    pub fn unlink(&mut self, dir: Ino, name: &OsStr) -> Result<()> {
        let ino = *self.inode(dir)?.dir()?.entries.get(name).ok_or(Errno::ENOENT)?;
        let n = self.inode(ino)?;
        if let Content::Dir(d) = &n.content {
            if d.mount.is_some() {
                return Err(Errno::EBUSY);
            }
            // The last name of a non-empty directory; a second name is the
            // other half of a rename and can go.
            if n.links == 1 && !d.entries.is_empty() {
                return Err(Errno::ENOTEMPTY);
            }
        }

        let t = now();
        let parent = self.inode_mut(dir)?;
        parent.dir_mut()?.entries.remove(name);
        parent.mtime = t;
        let n = self.inode_mut(ino)?;
        n.links -= 1;
        n.ctime = t;
        self.release(ino);
        Ok(())
    }

    /// Sent by the kernel to undo a failed open-with-create. The inode is
    /// already freed by then unless still referenced.
    pub fn destroy(&mut self, ino: Ino) -> Result<()> {
        let n = self.inode(ino)?;
        if n.links != 0 || n.opens != 0 {
            return Err(Errno::EBUSY);
        }
        self.release(ino);
        Ok(())
    }

    pub fn open(&mut self, ino: Ino) -> Result<()> {
        let n = self.inode_mut(ino)?;
        n.opens += 1;
        n.atime = now();
        Ok(())
    }

    pub fn close(&mut self, ino: Ino) -> Result<()> {
        let n = self.inode_mut(ino)?;
        n.opens = n.opens.saturating_sub(1);
        self.release(ino);
        Ok(())
    }

    pub fn read(&mut self, ino: Ino, offset: i64, buf: &mut [u8]) -> Result<usize> {
        let offset = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
        let n = self.inode_mut(ino)?;
        let data = match &n.content {
            Content::File(b) | Content::Symlink(b) => b,
            Content::Dir(_) => return Err(Errno::EISDIR),
            Content::Device { .. } => return Err(Errno::EINVAL),
        };
        let src = data.get(offset..).unwrap_or_default();
        let len = src.len().min(buf.len());
        buf[..len].copy_from_slice(&src[..len]);
        n.atime = now();
        Ok(len)
    }

    pub fn write(&mut self, ino: Ino, offset: i64, data: &[u8]) -> Result<usize> {
        let offset = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
        let end = offset.checked_add(data.len()).ok_or(Errno::EINVAL)?;
        if end > self.file(ino)?.len() {
            self.truncate(ino, end)?;
        }
        self.file(ino)?[offset..end].copy_from_slice(data);
        self.inode_mut(ino)?.mtime = now();
        Ok(data.len())
    }

    pub fn truncate(&mut self, ino: Ino, len: usize) -> Result<()> {
        let old = self.file(ino)?.len();
        if len > old {
            self.reserve(len - old)?;
        }
        self.file(ino)?.resize(len, 0);
        self.used = self.used + len - old;
        self.inode_mut(ino)?.mtime = now();
        Ok(())
    }

    /// The contents of a regular file.
    fn file(&mut self, ino: Ino) -> Result<&mut Vec<u8>> {
        match &mut self.inode_mut(ino)?.content {
            Content::File(b) => Ok(b),
            Content::Dir(_) => Err(Errno::EISDIR),
            _ => Err(Errno::EINVAL),
        }
    }

    /// Checks there is room for `more` bytes.
    fn reserve(&self, more: usize) -> Result<()> {
        if more > self.capacity - self.used { Err(Errno::ENOSPC) } else { Ok(()) }
    }

    pub fn getattr(&self, ino: Ino, attr: AttrType) -> Result<i64> {
        let n = self.inode(ino)?;
        let size = n.size() as i64;
        Ok(match attr {
            AttrType::Mode => (n.file_type().mode_bits() | n.perms).into(),
            AttrType::Uid => n.uid,
            AttrType::Gid => n.gid,
            AttrType::Size => size,
            AttrType::Blocks => (size + BLOCK_SIZE - 1) / BLOCK_SIZE,
            AttrType::IoBlock => IO_BLOCK,
            AttrType::Type => n.file_type().object_type().to_raw().into(),
            AttrType::PollStatus => POLL_READY,
            AttrType::CTime => n.ctime,
            AttrType::MTime => n.mtime,
            AttrType::ATime => n.atime,
            AttrType::Links => match &n.content {
                // Its name, its ".", and the ".." of each subdirectory.
                Content::Dir(d) => {
                    let subdirs = d.entries.values().filter(|i| matches!(self.inodes[i].content, Content::Dir(_)));
                    2 + subdirs.count() as i64
                }
                _ => n.links.into(),
            },
            AttrType::Port | AttrType::EventMask | AttrType::Dev | AttrType::Other(_) => return Err(Errno::EINVAL),
        })
    }

    pub fn getattr_all(&self, ino: Ino) -> Result<AttrAll> {
        self.inode(ino)?;
        let mut all = AttrAll::unsupported();
        for attr in AttrType::ALL {
            if let Ok(val) = self.getattr(ino, attr) {
                all.set(attr, val);
            }
        }
        Ok(all)
    }

    pub fn setattr(&mut self, ino: Ino, attr: AttrType, value: i64, data: &[u8]) -> Result<()> {
        if attr == AttrType::Size {
            return self.truncate(ino, usize::try_from(value).map_err(|_| Errno::EINVAL)?);
        }
        let own = self.oid(ino);
        let n = self.inode_mut(ino)?;
        match attr {
            AttrType::Mode => n.perms = value as u32 & PERMS,
            AttrType::Uid => n.uid = value,
            AttrType::Gid => n.gid = value,
            AttrType::MTime => n.mtime = value,
            AttrType::ATime => n.atime = value,
            AttrType::Dev => {
                let dev = Oid::from_ne_bytes(data).ok_or(Errno::EINVAL)?;
                // Mounting a directory's own oid on it unmounts.
                n.dir_mut()?.mount = (dev != own).then_some(dev);
            }
            _ => return Err(Errno::EINVAL),
        }
        if !matches!(attr, AttrType::MTime | AttrType::ATime) {
            n.ctime = now();
        }
        Ok(())
    }

    /// The entry at position `offset`: `.`, `..`, then the names in order.
    /// Each entry advances the client's position by one.
    pub fn readdir(&self, dir: Ino, offset: i64) -> Result<Dirent<'_>> {
        let d = self.inode(dir)?.dir()?;
        let offset = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
        let (name, ino) = match offset {
            0 => (OsStr::new("."), dir),
            1 => (OsStr::new(".."), d.parent),
            n => d.entries.iter().nth(n - 2).map(|(name, &ino)| (name.as_os_str(), ino)).ok_or(Errno::ENOENT)?,
        };
        // `d_ino` is 64-bit everywhere, `Ino` (`id_t`) only on some targets
        #[allow(clippy::useless_conversion)]
        let ino64 = u64::from(ino);
        Ok(Dirent { ino: ino64, file_type: self.inode(ino)?.file_type(), reclen: 1, name })
    }

    pub fn statfs(&self) -> Statvfs {
        let free = (self.capacity - self.used) as u64;
        Statvfs {
            f_bsize: 1,
            f_frsize: 1,
            f_blocks: self.capacity as u64,
            f_bfree: free,
            f_bavail: free,
            f_files: self.inodes.len() as u64,
            // `c_ulong`: a no-op on 32-bit targets
            #[allow(clippy::useless_conversion)]
            f_fsid: self.port.into(),
            f_namemax: NAME_MAX as _,
            ..Statvfs::default()
        }
    }
}

const USAGE: &str = "usage: rsramfs -m MOUNTPOINT [-s SIZE_KB]
  -m MOUNTPOINT  directory to mount the filesystem on, created if missing
  -s SIZE_KB     capacity for file contents (default 4096)";

struct Options {
    mountpoint: PathBuf,
    capacity: usize,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Option<Options> {
    let (mut mountpoint, mut size_kb) = (None, 4096usize);
    while let Some(flag) = args.next() {
        let value = args.next()?;
        match flag.as_str() {
            "-m" => mountpoint = Some(PathBuf::from(value)),
            "-s" => size_kb = value.parse().ok()?,
            _ => return None,
        }
    }
    Some(Options { mountpoint: mountpoint?, capacity: size_kb.checked_mul(1024)? })
}

fn run(opts: &Options) -> core::result::Result<(), String> {
    let dir = &opts.mountpoint;
    if !dir.exists() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let port = Port::new().map_err(|e| format!("cannot create a port: {e}"))?;
    let mut fs = RamFs::new(port.id(), opts.capacity);
    // Dropped (unmounting) before the port, if serving ever stops.
    let _mount = Mount::new(dir, port.oid(ROOT)).map_err(|e| format!("cannot mount on {}: {e}", dir.display()))?;
    println!("rsramfs: {} KiB at {}", opts.capacity / 1024, dir.display());
    port.serve(|req| fs.handle(req)).map_err(|e| format!("cannot receive messages: {e}"))
}

fn main() -> ExitCode {
    let Some(opts) = parse_args(std::env::args().skip(1)) else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    match run(&opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rsramfs: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;
    use phoenix::msg::{DirentOut, ObjectType, Out};

    fn new_fs() -> RamFs {
        RamFs::new(7, 1 << 20)
    }

    fn os(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    fn mkdir(fs: &mut RamFs, dir: Ino, name: &str) -> Ino {
        fs.create(dir, os(name), 0o755, CreateKind::Dir).unwrap()
    }

    fn mkfile(fs: &mut RamFs, dir: Ino, name: &str, contents: &[u8]) -> Ino {
        let ino = fs.create(dir, os(name), 0o644, CreateKind::File).unwrap();
        fs.write(ino, 0, contents).unwrap();
        ino
    }

    fn contents(fs: &mut RamFs, ino: Ino) -> Vec<u8> {
        let mut buf = vec![0; fs.getattr(ino, AttrType::Size).unwrap() as usize];
        let n = fs.read(ino, 0, &mut buf).unwrap();
        buf.truncate(n);
        buf
    }

    fn listing(fs: &RamFs, dir: Ino) -> Vec<String> {
        (0..).map_while(|i| fs.readdir(dir, i).ok()).map(|d| d.name.to_str().unwrap().to_owned()).collect()
    }

    /* The filesystem on its own */

    #[test]
    fn lookup_resolves_paths() {
        let mut fs = new_fs();
        let a = mkdir(&mut fs, ROOT, "a");
        let b = mkdir(&mut fs, a, "b");
        let c = mkfile(&mut fs, b, "c", b"");

        assert_eq!(fs.lookup(ROOT, b"a/b/c"), Ok((5, c)));
        assert_eq!(fs.lookup(ROOT, b"//a//b/"), Ok((7, b)));
        assert_eq!(fs.lookup(a, b"./b/../b/c"), Ok((10, c)));
        assert_eq!(fs.lookup(ROOT, b".."), Ok((2, ROOT)));
        assert_eq!(fs.lookup(ROOT, b""), Ok((0, ROOT)));
        assert_eq!(fs.lookup(ROOT, b"a/nope"), Err(Errno::ENOENT));
        assert_eq!(fs.lookup(ROOT, b"a/b/c/d"), Err(Errno::ENOTDIR));
    }

    #[test]
    fn lookup_stops_where_another_server_takes_over() {
        let mut fs = new_fs();
        let mnt = mkdir(&mut fs, ROOT, "mnt");
        let other = Oid::new(99, 5);
        fs.setattr(mnt, AttrType::Dev, 0, &other.to_ne_bytes()).unwrap();
        // Up to and excluding the separator: the kernel skips it itself.
        assert_eq!(fs.lookup(ROOT, b"mnt/x/y"), Ok((3, mnt)));
        assert_eq!(fs.unlink(ROOT, os("mnt")), Err(Errno::EBUSY));

        fs.setattr(mnt, AttrType::Dev, 0, &fs.oid(mnt).to_ne_bytes()).unwrap();
        assert_eq!(fs.lookup(ROOT, b"mnt/x"), Err(Errno::ENOENT), "unmounted");

        let tty = fs.create(ROOT, os("tty"), 0o666, CreateKind::Device(Oid::new(3, 1))).unwrap();
        assert_eq!(fs.lookup(ROOT, b"/tty/x"), Ok((4, tty)));
        assert_eq!(fs.getattr(tty, AttrType::Type), Ok(ObjectType::Device.to_raw().into()));
    }

    #[test]
    fn files_grow_shrink_and_respect_capacity() {
        let mut fs = RamFs::new(7, 100);
        let f = mkfile(&mut fs, ROOT, "f", b"hello");

        assert_eq!(fs.write(f, 8, b"!"), Ok(1));
        assert_eq!(contents(&mut fs, f), b"hello\0\0\0!");
        let mut buf = [0u8; 4];
        assert_eq!(fs.read(f, 7, &mut buf), Ok(2));
        assert_eq!(fs.read(f, 100, &mut buf), Ok(0));
        assert_eq!(fs.read(f, -1, &mut buf), Err(Errno::EINVAL));

        let oid = fs.oid(f);
        let w = |offset, append| Request::Write { oid, offset, append, data: b"ab" };
        assert_eq!(fs.handle(w(5, true)), Ok(Reply::Written { len: 2, end: 11 }), "O_APPEND: at the end");
        assert_eq!(fs.handle(w(5, false)), Ok(Reply::Written { len: 2, end: 7 }));
        assert_eq!(contents(&mut fs, f), b"helloab\0!ab");

        fs.truncate(f, 2).unwrap();
        assert_eq!(contents(&mut fs, f), b"he");

        assert_eq!(fs.write(f, 0, &[0; 101]), Err(Errno::ENOSPC));
        assert_eq!(contents(&mut fs, f), b"he", "a refused write changes nothing");
        assert_eq!(fs.write(f, 0, &[1; 100]), Ok(100));
        assert_eq!(fs.statfs().f_bfree, 0);
        assert_eq!(fs.create(ROOT, os("l"), 0o777, CreateKind::Symlink(os("f"))), Err(Errno::ENOSPC));

        assert_eq!(fs.read(ROOT, 0, &mut buf), Err(Errno::EISDIR));
        assert_eq!(fs.truncate(ROOT, 0), Err(Errno::EISDIR));
    }

    #[test]
    fn directories_list_and_count_links() {
        let mut fs = new_fs();
        let d = mkdir(&mut fs, ROOT, "dir");
        mkfile(&mut fs, ROOT, "file", b"");
        fs.create(ROOT, os("link"), 0o777, CreateKind::Symlink(os("file"))).unwrap();

        assert_eq!(listing(&fs, ROOT), [".", "..", "dir", "file", "link"]);
        let types: Vec<_> = (0..5).map(|i| fs.readdir(ROOT, i).unwrap().file_type).collect();
        use FileType::*;
        assert_eq!(types, [Directory, Directory, Directory, Regular, Symlink]);
        assert_eq!(fs.readdir(ROOT, -1).err(), Some(Errno::EINVAL));

        assert_eq!(fs.getattr(ROOT, AttrType::Links), Ok(3));
        assert_eq!(fs.getattr(d, AttrType::Links), Ok(2));
        #[allow(clippy::useless_conversion)] // as in readdir()
        let root = u64::from(ROOT);
        assert_eq!(fs.readdir(d, 1).unwrap().ino, root, "'..' of dir");
    }

    #[test]
    fn create_rejects_bad_names() {
        let mut fs = new_fs();
        let f = mkfile(&mut fs, ROOT, "f", b"");
        let file = || CreateKind::File;
        assert_eq!(fs.create(ROOT, os("f"), 0, file()), Err(Errno::EEXIST));
        assert_eq!(fs.create(ROOT, os("a/b"), 0, file()), Err(Errno::EINVAL));
        assert_eq!(fs.create(ROOT, os(".."), 0, CreateKind::Dir), Err(Errno::EINVAL));
        assert_eq!(fs.create(ROOT, os(&"x".repeat(256)), 0, file()), Err(Errno::ENAMETOOLONG));
        assert_eq!(fs.create(f, os("x"), 0, file()), Err(Errno::ENOTDIR));
        assert_eq!(listing(&fs, ROOT), [".", "..", "f"]);
    }

    #[test]
    fn inodes_live_while_named_or_open() {
        let mut fs = new_fs();
        let f = mkfile(&mut fs, ROOT, "f", b"abc");
        fs.link(ROOT, os("g"), f).unwrap();
        assert_eq!(fs.getattr(f, AttrType::Links), Ok(2));
        fs.unlink(ROOT, os("f")).unwrap();
        assert_eq!(contents(&mut fs, f), b"abc", "still named g");

        fs.open(f).unwrap();
        fs.unlink(ROOT, os("g")).unwrap();
        assert_eq!(fs.destroy(f), Err(Errno::EBUSY));
        assert_eq!(contents(&mut fs, f), b"abc", "still open");
        fs.close(f).unwrap();
        assert_eq!(fs.getattr(f, AttrType::Mode), Err(Errno::ENOENT));
        assert_eq!(fs.statfs().f_bfree, 1 << 20);
    }

    #[test]
    fn rename_is_link_then_unlink() {
        let mut fs = new_fs();
        let a = mkdir(&mut fs, ROOT, "a");
        let b = mkdir(&mut fs, ROOT, "b");
        let src = mkfile(&mut fs, a, "src", b"new");
        let victim = mkfile(&mut fs, b, "dst", b"old");

        // A file over an existing one.
        fs.link(b, os("dst"), src).unwrap();
        fs.unlink(a, os("src")).unwrap();
        assert_eq!(fs.lookup(ROOT, b"b/dst"), Ok((5, src)));
        assert_eq!(fs.getattr(victim, AttrType::Mode), Err(Errno::ENOENT));

        // A non-empty directory, into another one.
        fs.link(a, os("moved"), b).unwrap();
        fs.unlink(ROOT, os("b")).unwrap();
        assert_eq!(fs.lookup(ROOT, b"a/moved/dst"), Ok((11, src)));
        assert_eq!(fs.lookup(b, b".."), Ok((2, a)), "re-parented");

        // Not into itself, not over a directory.
        assert_eq!(fs.link(b, os("loop"), a), Err(Errno::EINVAL));
        assert_eq!(fs.link(a, os("moved"), src), Err(Errno::EEXIST));

        assert_eq!(fs.unlink(ROOT, os("a")), Err(Errno::ENOTEMPTY));
        assert_eq!(fs.unlink(ROOT, os("..")), Err(Errno::ENOENT));
    }

    /* Requests, as the server loop hands them over */

    #[test]
    fn requests_map_onto_the_filesystem() {
        let mut fs = new_fs();
        let root = fs.oid(ROOT);

        let req = Request::Create { dir: root, name: os("ln"), mode: 0o777, kind: CreateKind::Symlink(os("x/y")) };
        let Ok(Reply::Created(ln)) = fs.handle(req) else { panic!("create failed") };

        let mut buf = [0u8; 16];
        assert_eq!(fs.handle(Request::Read { oid: ln, offset: 0, buf: &mut buf }), Ok(Reply::Bytes(3)));
        assert_eq!(&buf[..3], b"x/y");

        let req = Request::Lookup { dir: root, path: os("ln") };
        assert_eq!(fs.handle(req), Ok(Reply::Lookup { consumed: 2, file: ln, dev: ln }));

        let mut dent = [0u8; 300];
        let req = Request::Readdir { dir: root, offset: 2, out: DirentOut::new(&mut dent) };
        assert_eq!(fs.handle(req), Ok(Reply::Done));
        assert_eq!(&dent[16..19], b"ln\0");

        let mut attrs = [0u8; size_of::<AttrAll>()];
        fs.handle(Request::GetAttrAll { oid: ln, out: Out::new(&mut attrs) }).unwrap();
        let attrs = AttrAll::from_ne_bytes(&attrs).unwrap();
        assert_eq!(attrs.get(AttrType::Mode), Ok((FileType::Symlink.mode_bits() | 0o777).into()));
        assert_eq!(attrs.get(AttrType::Port), Err(Errno::ENOSYS));

        let foreign = Oid::new(fs.port + 1, ROOT);
        assert_eq!(fs.handle(Request::Open { oid: foreign, flags: 0 }), Err(Errno::EXDEV));
        assert_eq!(fs.handle(Request::Other { msg_type: 5, oid: root }), Err(Errno::ENOSYS));
    }

    /* Mounted in the rootfs and used through std::fs */

    /// rsramfs serving on its own thread, mounted on a fresh directory.
    struct Mounted {
        dir: PathBuf,
        port: std::sync::Arc<Port>,
        mount: Option<Mount>,
        thread: Option<std::thread::JoinHandle<Result<()>>>,
    }

    impl Mounted {
        fn new() -> Self {
            let dir = PathBuf::from(format!("/rsramfs-test-{}", std::process::id()));
            std::fs::create_dir(&dir).unwrap();
            let port = std::sync::Arc::new(Port::new().unwrap());
            let thread = std::thread::spawn({
                let port = port.clone();
                move || {
                    let mut fs = RamFs::new(port.id(), 1 << 20);
                    port.serve(|req| fs.handle(req))
                }
            });
            let mount = Mount::new(&dir, port.oid(ROOT)).unwrap();
            Mounted { dir, port, mount: Some(mount), thread: Some(thread) }
        }
    }

    impl Drop for Mounted {
        fn drop(&mut self) {
            drop(self.mount.take());
            self.port.close();
            let _ = self.thread.take().unwrap().join();
            let _ = std::fs::remove_dir(&self.dir);
        }
    }

    #[test]
    fn std_fs_through_the_kernel() {
        use std::fs;
        use std::io::{Seek, SeekFrom, Write};

        let m = Mounted::new();
        let p = |rel: &str| m.dir.join(rel);

        fs::create_dir(p("sub")).unwrap();
        fs::write(p("sub/hello.txt"), "hello from rust").unwrap();
        assert_eq!(fs::read_to_string(p("sub/hello.txt")).unwrap(), "hello from rust");
        assert_eq!(fs::metadata(p("sub/hello.txt")).unwrap().len(), 15);
        assert!(fs::metadata(p("sub")).unwrap().is_dir());

        let mut f = fs::OpenOptions::new().write(true).open(p("sub/hello.txt")).unwrap();
        f.seek(SeekFrom::Start(6)).unwrap();
        f.write_all(b"FROM").unwrap();
        drop(f);
        fs::OpenOptions::new().append(true).open(p("sub/hello.txt")).unwrap().write_all(b"!").unwrap();
        assert_eq!(fs::read_to_string(p("sub/hello.txt")).unwrap(), "hello FROM rust!");

        // Far bigger than a message's inline payload.
        let big: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        fs::write(p("big"), &big).unwrap();
        assert!(fs::read(p("big")).unwrap() == big);

        std::os::unix::fs::symlink("sub/hello.txt", p("link")).unwrap();
        assert_eq!(fs::read_link(p("link")).unwrap(), PathBuf::from("sub/hello.txt"));

        let mut names: Vec<_> = fs::read_dir(&m.dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        assert_eq!(names, ["big", "link", "sub"]);

        fs::rename(p("sub/hello.txt"), p("moved.txt")).unwrap();
        assert!(!p("sub/hello.txt").exists());
        fs::rename(p("sub"), p("renamed")).unwrap();
        fs::write(p("renamed/inner"), "x").unwrap();

        fs::remove_dir(p("renamed")).unwrap_err();
        fs::remove_file(p("renamed/inner")).unwrap();
        fs::remove_dir(p("renamed")).unwrap();
        for f in ["moved.txt", "big", "link"] {
            fs::remove_file(p(f)).unwrap();
        }
        assert_eq!(fs::read_dir(&m.dir).unwrap().count(), 0);
    }

    #[test]
    fn arguments() {
        let parse = |s: &str| parse_args(s.split_whitespace().map(String::from)).map(|o| (o.mountpoint, o.capacity));
        assert_eq!(parse("-m /ram"), Some((PathBuf::from("/ram"), 4096 * 1024)));
        assert_eq!(parse("-s 8 -m /ram"), Some((PathBuf::from("/ram"), 8 * 1024)));
        assert_eq!(parse("-s 8"), None);
        assert_eq!(parse("-m"), None);
        assert_eq!(parse("-x 1 -m /ram"), None);
    }
}
