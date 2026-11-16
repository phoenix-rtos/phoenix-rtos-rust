//! Filesystems a partition can be mounted with. None is built in: each is
//! registered (ext2 with the `ext2` feature), and a mount names one by name or
//! picks it by the partition's MBR type.

use phoenix::msg::raw::RawMsg;
use phoenix::{Errno, Id, Oid, Result};
use std::sync::RwLock;

pub trait Filesystem: Send + Sync {
    /// As `mtMount` names it, e.g. "ext2".
    fn name(&self) -> &str;

    /// The MBR partition type it is mounted from by default.
    fn partition_type(&self) -> u8;

    /// Mounts the partition of the device `dev`; its partition port is
    /// `dev.port`. The id of the filesystem's root, and the mounted instance.
    fn mount(&self, dev: Oid) -> Result<(Id, Box<dyn MountedFs>)>;
}

/// A mounted filesystem; it handles the messages sent to its partition port.
pub trait MountedFs: Send + Sync {
    /// Handles one message, leaving the response in it. Calls may overlap.
    fn handle(&self, msg: &mut RawMsg);

    /// Unmounts; no [`handle`](MountedFs::handle) runs meanwhile or after.
    fn unmount(self: Box<Self>) -> Result<()>;
}

/// Sector I/O for a filesystem implementation that reaches the device
/// through callbacks: by the token its mount was given, which names the
/// device that mount was made on, whatever happens to it later.
pub trait BlockIo: Sync {
    fn read(&self, token: Id, offset: i64, buf: &mut [u8]) -> Result<usize>;
    fn write(&self, token: Id, offset: i64, data: &[u8]) -> Result<usize>;
}

#[derive(Default)]
pub struct Filesystems {
    all: Vec<Box<dyn Filesystem>>,
}

impl Filesystems {
    pub fn register(&mut self, fs: Box<dyn Filesystem>) {
        self.all.push(fs);
    }

    pub fn by_name(&self, name: &str) -> Option<&dyn Filesystem> {
        self.all.iter().find(|fs| fs.name() == name).map(|fs| &**fs)
    }

    pub fn by_partition_type(&self, kind: u8) -> Option<&dyn Filesystem> {
        self.all.iter().find(|fs| fs.partition_type() == kind).map(|fs| &**fs)
    }
}

/// A mounted filesystem shared by the threads handling its messages:
/// handling takes the lock shared, unmounting exclusively, so an unmount
/// waits for the messages being handled and none is handled after it.
pub struct Mounted {
    fs: RwLock<Option<Box<dyn MountedFs>>>,
}

impl Mounted {
    pub fn new(fs: Box<dyn MountedFs>) -> Self {
        Mounted { fs: RwLock::new(Some(fs)) }
    }

    /// Handles a message; `false` (the message untouched) once unmounted.
    pub fn handle(&self, msg: &mut RawMsg) -> bool {
        match &*self.fs.read().unwrap_or_else(|e| e.into_inner()) {
            Some(fs) => {
                fs.handle(msg);
                true
            }
            None => false,
        }
    }

    /// Unmounts, once; `ENOENT` if already unmounted.
    pub fn unmount(&self) -> Result<()> {
        let fs = self.fs.write().unwrap_or_else(|e| e.into_inner()).take();
        fs.ok_or(Errno::ENOENT)?.unmount()
    }

    pub fn is_mounted(&self) -> bool {
        self.fs.read().unwrap_or_else(|e| e.into_inner()).is_some()
    }
}
