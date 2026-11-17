//! A storage device: probed in stages, then read and written until it is
//! removed.

use crate::bot::BulkPipes;
use crate::mbr::{self, Partition};
use crate::scsi::{Inquiry, Scsi, SECTOR};
use phoenix::{Errno, Id, Result};
use std::sync::Mutex;

/// A request is cut to this many bytes; also a cache line, read in one
/// command (the C driver cut requests to 4 KiB).
pub const MAX_TRANSFER: usize = 32 * SECTOR;

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

/// A read cache in front of a device: a miss fetches a whole line, so the
/// sectors after it are read ahead. Writes bypass it and invalidate what it
/// holds of them.
pub trait ReadCache: Send {
    /// Bytes from offset 0 it can hold; beyond them, reads bypass it.
    fn limit(&self) -> u64;
    /// Reads `buf` at byte `offset`; `fetch(line_offset, line)` fills a line
    /// it does not hold.
    fn read(&mut self, offset: u64, buf: &mut [u8], fetch: &mut dyn FnMut(u64, &mut [u8]) -> Result<usize>)
        -> Result<usize>;
    /// Forgets what it holds of `[offset, offset + len)`.
    fn invalidate(&mut self, offset: u64, len: u64);
}

/// The device's link to the hardware.
enum Link {
    Present { scsi: Scsi<Box<dyn BulkPipes>>, cache: Option<Box<dyn ReadCache>> },
    /// Unplugged: requests still in flight fail.
    Gone,
}

/// Fills `line` from byte `offset` of the partition (`start`, `size` bytes):
/// a cache line, which past the partition's end is zeros.
fn fetch_line(scsi: &mut Scsi<Box<dyn BulkPipes>>, start: u32, size: u64, offset: u64, line: &mut [u8]) -> Result<usize> {
    let inside = size.saturating_sub(offset).min(line.len() as u64) as usize;
    let (data, past_end) = line.split_at_mut(inside);
    past_end.fill(0);
    if !data.is_empty() {
        let lba = u32::try_from(offset / SECTOR as u64)
            .ok()
            .and_then(|sector| start.checked_add(sector))
            .ok_or(Errno::EINVAL)?;
        if scsi.read(lba, data)? != data.len() {
            return Err(Errno::EIO);
        }
    }
    Ok(line.len())
}

pub struct Device {
    id: Id,
    location: Location,
    partition: Partition,
    /// One command at a time: the transport's tag and pipes.
    link: Mutex<Link>,
}

impl Device {
    pub fn new(
        id: Id,
        location: Location,
        scsi: Scsi<Box<dyn BulkPipes>>,
        partition: Partition,
        cache: Option<Box<dyn ReadCache>>,
    ) -> Device {
        Device { id, location, partition, link: Mutex::new(Link::Present { scsi, cache }) }
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
        let (start, size, offset) = (self.partition.start, self.size(), offset as u64);
        match &mut *self.link.lock().unwrap_or_else(|e| e.into_inner()) {
            Link::Present { scsi, cache: Some(cache) } if offset + len as u64 <= cache.limit() => {
                cache.read(offset, &mut buf[..len], &mut |at, line| fetch_line(scsi, start, size, at, line))
            }
            Link::Present { scsi, .. } => scsi.read(lba, &mut buf[..len]),
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
            Link::Present { scsi, cache } => {
                let written = scsi.write(lba, &data[..len]);
                // after a failure too: what is on the device is unknown
                if let Some(cache) = cache {
                    cache.invalidate(offset as u64, len as u64);
                }
                written
            }
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
