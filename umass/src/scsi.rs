//! The SCSI transparent command set over [`Bot`]: the few commands a
//! direct-access device needs, and the retry after a failed command.

use crate::bot::{BulkPipes, Bot, Data, Status};
use phoenix::{Errno, Result};

pub const SECTOR: usize = 512;

const TEST_UNIT_READY: u8 = 0x00;
const REQUEST_SENSE: u8 = 0x03;
const INQUIRY: u8 = 0x12;
const READ_10: u8 = 0x28;
const WRITE_10: u8 = 0x2a;

/// Standard INQUIRY data, as much as is asked for.
pub const INQUIRY_LEN: u8 = 36;
/// Fixed-format sense data.
pub const SENSE_LEN: u8 = 18;

/// Tries of a command the device fails, each followed by REQUEST SENSE.
const TRIES: usize = 3;

/// A 6-byte command block; byte 4 is the allocation length.
fn cdb6(opcode: u8, alloc: u8) -> [u8; 6] {
    [opcode, 0, 0, 0, alloc, 0]
}

/// A 10-byte command block: big-endian LBA and number of blocks.
fn cdb10(opcode: u8, lba: u32, blocks: u16) -> [u8; 10] {
    let [l0, l1, l2, l3] = lba.to_be_bytes();
    let [b0, b1] = blocks.to_be_bytes();
    [opcode, 0, l0, l1, l2, l3, 0, b0, b1, 0]
}

/// What INQUIRY says about the device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inquiry {
    pub device_type: u8,
    pub vendor: String,
    pub product: String,
}

impl Inquiry {
    /// From the bytes the device returned, which may be fewer than asked for.
    pub fn parse(b: &[u8]) -> Option<Inquiry> {
        let text = |r: core::ops::Range<usize>| {
            b.get(r).map(|s| String::from_utf8_lossy(s).trim_end().to_string()).unwrap_or_default()
        };
        Some(Inquiry { device_type: b.first()? & 0x1f, vendor: text(8..16), product: text(16..32) })
    }
}

/// The sense key and its additional code, from fixed-format sense data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sense {
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
}

impl Sense {
    pub fn parse(b: &[u8]) -> Option<Sense> {
        Some(Sense { key: b.get(2)? & 0x0f, asc: b.get(12).copied().unwrap_or(0), ascq: b.get(13).copied().unwrap_or(0) })
    }
}

/// A SCSI device behind the transport.
pub struct Scsi<P> {
    bot: Bot<P>,
}

impl<P: BulkPipes> Scsi<P> {
    pub fn new(pipes: P) -> Self {
        Scsi { bot: Bot::new(pipes) }
    }

    /// Runs a command, retrying it after REQUEST SENSE while the device fails
    /// it. The bytes of the data stage.
    pub fn execute(&mut self, cdb: &[u8], mut data: Data<'_>) -> Result<usize> {
        for _ in 0..TRIES {
            let reborrowed = match &mut data {
                Data::None => Data::None,
                Data::In(b) => Data::In(b),
                Data::Out(b) => Data::Out(b),
            };
            match self.bot.command(cdb, reborrowed)? {
                Status::Passed(n) => return Ok(n),
                Status::Failed => {
                    // Clears the condition; what it says is for debugging.
                    let _ = self.request_sense();
                }
                // TODO: Bulk-Only Mass Storage Reset and clearing both bulk
                // endpoints' halt; that needs their addresses from libusb
                Status::PhaseError => return Err(Errno::EIO),
            }
        }
        Err(Errno::EIO)
    }

    /// REQUEST SENSE, not retried: it is what a retry runs.
    pub fn request_sense(&mut self) -> Result<Sense> {
        let mut buf = [0u8; SENSE_LEN as usize];
        match self.bot.command(&cdb6(REQUEST_SENSE, SENSE_LEN), Data::In(&mut buf))? {
            Status::Passed(n) => Sense::parse(&buf[..n]).ok_or(Errno::EIO),
            _ => Err(Errno::EIO),
        }
    }

    pub fn test_unit_ready(&mut self) -> Result<()> {
        self.execute(&cdb6(TEST_UNIT_READY, 0), Data::None).map(drop)
    }

    pub fn inquiry(&mut self) -> Result<Inquiry> {
        let mut buf = [0u8; INQUIRY_LEN as usize];
        let n = self.execute(&cdb6(INQUIRY, INQUIRY_LEN), Data::In(&mut buf))?;
        Inquiry::parse(&buf[..n]).ok_or(Errno::EIO)
    }

    /// Reads whole sectors from `lba` into `buf`; the bytes read.
    pub fn read(&mut self, lba: u32, buf: &mut [u8]) -> Result<usize> {
        let blocks = blocks(buf.len())?;
        self.execute(&cdb10(READ_10, lba, blocks), Data::In(buf))
    }

    /// Writes whole sectors from `data` at `lba`; the bytes written.
    pub fn write(&mut self, lba: u32, data: &[u8]) -> Result<usize> {
        let blocks = blocks(data.len())?;
        self.execute(&cdb10(WRITE_10, lba, blocks), Data::Out(data))
    }

    #[cfg(test)]
    pub fn bot(&mut self) -> &mut Bot<P> {
        &mut self.bot
    }
}

fn blocks(len: usize) -> Result<u16> {
    if !len.is_multiple_of(SECTOR) {
        return Err(Errno::EINVAL);
    }
    u16::try_from(len / SECTOR).map_err(|_| Errno::EINVAL)
}
