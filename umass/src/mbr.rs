//! The partition the driver serves: the first one of the MBR.

use crate::scsi::SECTOR;

const MAGIC_OFFSET: usize = 510;
const TABLE_OFFSET: usize = 446;

/// A partition, in sectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    /// The MBR partition type (0x83: Linux).
    pub kind: u8,
    pub start: u32,
    pub sectors: u32,
}

impl Partition {
    pub fn size(&self) -> u64 {
        u64::from(self.sectors) * SECTOR as u64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MbrError {
    BadMagic(u16),
    /// The first entry is unused.
    NoPartition,
}

/// The first partition of the MBR in `sector0`.
pub fn first_partition(sector0: &[u8; SECTOR]) -> Result<Partition, MbrError> {
    let magic = u16::from_le_bytes([sector0[MAGIC_OFFSET], sector0[MAGIC_OFFSET + 1]]);
    if magic != 0xaa55 {
        return Err(MbrError::BadMagic(magic));
    }
    let e = &sector0[TABLE_OFFSET..TABLE_OFFSET + 16];
    let word = |i: usize| u32::from_le_bytes([e[i], e[i + 1], e[i + 2], e[i + 3]]);
    let part = Partition { kind: e[4], start: word(8), sectors: word(12) };
    if part.kind == 0 || part.sectors == 0 {
        return Err(MbrError::NoPartition);
    }
    Ok(part)
}
