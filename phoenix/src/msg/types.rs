//! Typed forms of the integer codes messages carry.

/// An attribute, as in `mtGetAttr`/`mtSetAttr` (`enum { atMode, ... }`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttrType {
    Mode,
    Uid,
    Gid,
    Size,
    Blocks,
    IoBlock,
    Type,
    Port,
    PollStatus,
    EventMask,
    CTime,
    MTime,
    ATime,
    Links,
    /// Setting it on a directory mounts the given oid there.
    Dev,
    /// A code this crate does not know.
    Other(i32),
}

impl AttrType {
    /// In the order of their C values.
    pub const ALL: [AttrType; 15] = [
        AttrType::Mode,
        AttrType::Uid,
        AttrType::Gid,
        AttrType::Size,
        AttrType::Blocks,
        AttrType::IoBlock,
        AttrType::Type,
        AttrType::Port,
        AttrType::PollStatus,
        AttrType::EventMask,
        AttrType::CTime,
        AttrType::MTime,
        AttrType::ATime,
        AttrType::Links,
        AttrType::Dev,
    ];

    pub fn from_raw(code: i32) -> Self {
        usize::try_from(code).ok().and_then(|i| Self::ALL.get(i).copied()).unwrap_or(AttrType::Other(code))
    }

    pub fn to_raw(self) -> i32 {
        match self {
            AttrType::Other(code) => code,
            known => Self::ALL.iter().position(|&a| a == known).unwrap() as i32,
        }
    }
}

/// Object types (`enum { otDir, otFile, ... }`): what `mtCreate` creates and
/// what `atType` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectType {
    Dir,
    File,
    Device,
    Symlink,
    Unknown,
}

impl ObjectType {
    pub fn from_raw(code: i32) -> Option<Self> {
        Some(match code {
            0 => ObjectType::Dir,
            1 => ObjectType::File,
            2 => ObjectType::Device,
            3 => ObjectType::Symlink,
            4 => ObjectType::Unknown,
            _ => return None,
        })
    }

    pub fn to_raw(self) -> i32 {
        self as i32
    }
}

/// The type of a file, convertible to the `S_IF*` bits of a mode and the
/// `DT_*` value of a directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileType {
    Fifo,
    CharDevice,
    Directory,
    BlockDevice,
    Regular,
    Symlink,
    Socket,
}

impl FileType {
    const S_IFMT: u32 = 0xf000;

    /// The `S_IF*` bits.
    pub fn mode_bits(self) -> u32 {
        match self {
            FileType::Fifo => 0x1000,
            FileType::CharDevice => 0x2000,
            FileType::Directory => 0x4000,
            FileType::BlockDevice => 0x6000,
            FileType::Regular => 0x8000,
            FileType::Symlink => 0xa000,
            FileType::Socket => 0xc000,
        }
    }

    pub fn from_mode(mode: u32) -> Option<Self> {
        [
            FileType::Fifo,
            FileType::CharDevice,
            FileType::Directory,
            FileType::BlockDevice,
            FileType::Regular,
            FileType::Symlink,
            FileType::Socket,
        ]
        .into_iter()
        .find(|t| t.mode_bits() == mode & Self::S_IFMT)
    }

    /// The `DT_*` value.
    pub fn dirent_type(self) -> u32 {
        // DT_x is S_IFx >> 12 on Phoenix, as on most systems.
        self.mode_bits() >> 12
    }

    /// What `atType` reports for it.
    pub fn object_type(self) -> ObjectType {
        match self {
            FileType::Directory => ObjectType::Dir,
            FileType::Regular => ObjectType::File,
            FileType::Symlink => ObjectType::Symlink,
            FileType::Fifo | FileType::CharDevice | FileType::BlockDevice => ObjectType::Device,
            FileType::Socket => ObjectType::Unknown,
        }
    }
}

/// `atPollStatus` of an object that is always ready:
/// `POLLIN | POLLRDNORM | POLLOUT | POLLWRNORM`.
pub const POLL_READY: i64 = 0x1 | 0x2 | 0x10 | 0x20;
