//! A storage device: probed in stages, then read and written until it is
//! removed.

use crate::bot::BulkPipes;
use crate::mbr::{self, Partition};
use crate::scsi::{Inquiry, Scsi, SECTOR};
use phoenix::{Errno, Id, Result};
use std::sync::Mutex;

/// A request is cut to this many bytes, as the C driver did.
pub const MAX_TRANSFER: usize = 8 * SECTOR;

/// Rounds of TEST UNIT READY and INQUIRY before giving up on a device.
const INIT_ROUNDS: usize = 10;

/// Where the device is on the bus, as insertion and deletion name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location {
    pub bus: i32,
    pub dev: i32,
    pub interface: i32,
}

/// Probing, stage one: the pipes are open.
pub struct Attached<P> {
    scsi: Scsi<P>,
}

/// Probing, stage two: the device answers SCSI commands.
pub struct Ready<P> {
    scsi: Scsi<P>,
    pub inquiry: Inquiry,
}

impl<P: BulkPipes> Attached<P> {
    pub fn new(pipes: P) -> Self {
        Attached { scsi: Scsi::new(pipes) }
    }

    /// Ready once TEST UNIT READY and INQUIRY both pass in the same round:
    /// some devices are said not to work without the INQUIRY.
    pub fn init(mut self) -> Result<Ready<P>> {
        for _ in 0..INIT_ROUNDS {
            let unit_ready = self.scsi.test_unit_ready().is_ok();
            if let Ok(inquiry) = self.scsi.inquiry() {
                if unit_ready {
                    return Ok(Ready { scsi: self.scsi, inquiry });
                }
            }
        }
        Err(Errno::EIO)
    }
}

impl<P: BulkPipes> Ready<P> {
    /// Reads the MBR: the device serves its first partition.
    pub fn partition(mut self) -> Result<(Scsi<P>, Partition)> {
        let mut sector0 = [0u8; SECTOR];
        if self.scsi.read(0, &mut sector0)? != SECTOR {
            return Err(Errno::EIO);
        }
        let part = mbr::first_partition(&sector0).map_err(|_| Errno::EINVAL)?;
        Ok((self.scsi, part))
    }
}

/// The device's link to the hardware.
enum Link {
    Present(Scsi<Box<dyn BulkPipes>>),
    /// Unplugged: requests still in flight fail.
    Gone,
}

pub struct Device {
    id: Id,
    location: Location,
    partition: Partition,
    /// One command at a time: the transport's tag and pipes.
    link: Mutex<Link>,
}

impl Device {
    pub fn new(id: Id, location: Location, scsi: Scsi<Box<dyn BulkPipes>>, partition: Partition) -> Device {
        Device { id, location, partition, link: Mutex::new(Link::Present(scsi)) }
    }

    pub fn id(&self) -> Id {
        self.id
    }

    pub fn location(&self) -> Location {
        self.location
    }

    pub fn partition(&self) -> Partition {
        self.partition
    }

    /// The partition's size in bytes.
    pub fn size(&self) -> u64 {
        self.partition.size()
    }

    /// Reads whole sectors at byte `offset` of the partition, at most
    /// [`MAX_TRANSFER`] bytes; the bytes read. Nothing is no command:
    /// libext2 reads 0 bytes (its whole-block part of a small GDT).
    pub fn read(&self, offset: i64, buf: &mut [u8]) -> Result<usize> {
        let (lba, len) = self.locate(offset, buf.len())?;
        if len == 0 {
            return Ok(0);
        }
        match &mut *self.link.lock().unwrap_or_else(|e| e.into_inner()) {
            Link::Present(scsi) => scsi.read(lba, &mut buf[..len]),
            Link::Gone => Err(Errno::ENODEV),
        }
    }

    /// Writes whole sectors at byte `offset` of the partition, at most
    /// [`MAX_TRANSFER`] bytes; the bytes written.
    pub fn write(&self, offset: i64, data: &[u8]) -> Result<usize> {
        let (lba, len) = self.locate(offset, data.len())?;
        if len == 0 {
            return Ok(0);
        }
        match &mut *self.link.lock().unwrap_or_else(|e| e.into_inner()) {
            Link::Present(scsi) => scsi.write(lba, &data[..len]),
            Link::Gone => Err(Errno::ENODEV),
        }
    }

    /// The device was unplugged: drops the pipes, after the command in
    /// flight if there is one.
    pub fn detach(&self) {
        *self.link.lock().unwrap_or_else(|e| e.into_inner()) = Link::Gone;
    }

    pub fn is_gone(&self) -> bool {
        matches!(*self.link.lock().unwrap_or_else(|e| e.into_inner()), Link::Gone)
    }

    /// The device LBA of byte `offset` of the partition, and how many bytes
    /// of `len` one command moves.
    fn locate(&self, offset: i64, len: usize) -> Result<(u32, usize)> {
        let offset = u64::try_from(offset).map_err(|_| Errno::EINVAL)?;
        if !offset.is_multiple_of(SECTOR as u64) || !len.is_multiple_of(SECTOR) {
            return Err(Errno::EINVAL);
        }
        let end = offset.checked_add(len as u64).ok_or(Errno::EINVAL)?;
        if end > self.size() {
            return Err(Errno::EINVAL);
        }
        let lba = u32::try_from(offset / SECTOR as u64)
            .ok()
            .and_then(|sector| self.partition.start.checked_add(sector))
            .ok_or(Errno::EINVAL)?;
        Ok((lba, len.min(MAX_TRANSFER)))
    }
}
