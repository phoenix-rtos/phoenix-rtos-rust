//! Bulk-Only Transport (USB Mass Storage Class, BBB): every command is a
//! Command Block Wrapper on the bulk OUT pipe, an optional data stage, and a
//! Command Status Wrapper on the bulk IN pipe.

use phoenix::{Errno, Result};

const CBW_SIG: u32 = 0x4342_5355;
const CSW_SIG: u32 = 0x5342_5355;
pub const CBW_LEN: usize = 31;
pub const CSW_LEN: usize = 13;
/// A command block is at most 16 bytes.
pub const CDB_MAX: usize = 16;

/// The device's two bulk pipes.
pub trait BulkPipes: Send {
    /// Sends `data` on the bulk OUT pipe; the number of bytes taken.
    fn bulk_out(&mut self, data: &[u8]) -> Result<usize>;
    /// Fills `data` from the bulk IN pipe; the number of bytes received.
    fn bulk_in(&mut self, data: &mut [u8]) -> Result<usize>;
}

impl<T: BulkPipes + ?Sized> BulkPipes for Box<T> {
    fn bulk_out(&mut self, data: &[u8]) -> Result<usize> {
        (**self).bulk_out(data)
    }

    fn bulk_in(&mut self, data: &mut [u8]) -> Result<usize> {
        (**self).bulk_in(data)
    }
}

/// The data stage of a command.
#[derive(Debug)]
pub enum Data<'a> {
    None,
    In(&'a mut [u8]),
    Out(&'a [u8]),
}

impl Data<'_> {
    fn len(&self) -> usize {
        match self {
            Data::None => 0,
            Data::In(b) => b.len(),
            Data::Out(b) => b.len(),
        }
    }
}

/// How a command ended, per its status wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Passed, with the bytes of the data stage.
    Passed(usize),
    /// Failed: the sense data says why.
    Failed,
    /// The device lost track of the protocol and needs a reset recovery.
    PhaseError,
}

/// A Command Block Wrapper.
pub fn encode_cbw(tag: u32, data_len: u32, dir_in: bool, cdb: &[u8]) -> Result<[u8; CBW_LEN]> {
    if cdb.is_empty() || cdb.len() > CDB_MAX {
        return Err(Errno::EINVAL);
    }
    let mut cbw = [0u8; CBW_LEN];
    cbw[0..4].copy_from_slice(&CBW_SIG.to_le_bytes());
    cbw[4..8].copy_from_slice(&tag.to_le_bytes());
    cbw[8..12].copy_from_slice(&data_len.to_le_bytes());
    cbw[12] = if dir_in { 0x80 } else { 0 };
    cbw[13] = 0; // LUN
    cbw[14] = cdb.len() as u8;
    cbw[15..15 + cdb.len()].copy_from_slice(cdb);
    Ok(cbw)
}

/// A Command Status Wrapper, if it is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Csw {
    pub tag: u32,
    pub residue: u32,
    pub status: u8,
}

pub fn decode_csw(b: &[u8; CSW_LEN]) -> Option<Csw> {
    let word = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    (word(0) == CSW_SIG).then(|| Csw { tag: word(4), residue: word(8), status: b[12] })
}

/// The transport: the pipes and the tag of the next command.
pub struct Bot<P> {
    pipes: P,
    tag: u32,
}

impl<P: BulkPipes> Bot<P> {
    pub fn new(pipes: P) -> Self {
        Bot { pipes, tag: 0 }
    }

    /// Runs one command. `Err` is a transport failure (or a status wrapper
    /// that does not belong to the command), which only a reset recovery
    /// could clear; a command the device rejects is `Ok(Status::Failed)`.
    pub fn command(&mut self, cdb: &[u8], data: Data<'_>) -> Result<Status> {
        // The tag only has to differ between consecutive commands; it wraps.
        let tag = self.tag;
        self.tag = self.tag.wrapping_add(1);

        let len = u32::try_from(data.len()).map_err(|_| Errno::EINVAL)?;
        let cbw = encode_cbw(tag, len, matches!(data, Data::In(_)), cdb)?;
        if self.pipes.bulk_out(&cbw)? != CBW_LEN {
            return Err(Errno::EIO);
        }

        // a data stage only if the CBW announced data: a zero-length one
        // would take the device's status wrapper as data
        let transferred = match data {
            Data::In(buf) if !buf.is_empty() => self.pipes.bulk_in(buf)?,
            Data::Out(buf) if !buf.is_empty() => self.pipes.bulk_out(buf)?,
            _ => 0,
        };

        let mut raw = [0u8; CSW_LEN];
        if self.pipes.bulk_in(&mut raw)? != CSW_LEN {
            return Err(Errno::EIO);
        }
        match decode_csw(&raw) {
            Some(csw) if csw.tag == tag => Ok(match csw.status {
                0 => Status::Passed(transferred),
                1 => Status::Failed,
                _ => Status::PhaseError,
            }),
            _ => Err(Errno::EIO),
        }
    }

    #[cfg(test)]
    pub fn pipes(&mut self) -> &mut P {
        &mut self.pipes
    }

    #[cfg(test)]
    pub fn set_tag(&mut self, tag: u32) {
        self.tag = tag;
    }
}
