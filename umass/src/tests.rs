//! Host tests: a mock SCSI device behind the bulk pipes.

use crate::bot::{self, BulkPipes, Bot, Data, Status, CBW_LEN, CSW_LEN};
use crate::device::{Attached, Device, Location, MAX_TRANSFER};
use crate::fs::{Filesystem, Filesystems, Mounted, MountedFs};
use crate::mbr::{self, MbrError, Partition};
use crate::registry::Registry;
use crate::scsi::{Scsi, SECTOR};
use phoenix::msg::raw::RawMsg;
use phoenix::{Errno, Id, Oid, Result};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// (opcode, tag, LBA) of every command the mock received.
type Log = Arc<Mutex<Vec<(u8, u32, u32)>>>;

/// What the mock does next.
enum Phase {
    Cbw,
    DataIn(Vec<u8>, u32, u8),
    DataOut { lba: u32, len: usize, tag: u32 },
    Csw(u32, u8),
}

/// A SCSI direct-access device with an in-memory disk.
struct Mock {
    disk: Vec<u8>,
    phase: Phase,
    /// TEST UNIT READY fails this many more times.
    unit_not_ready: usize,
    /// Every command fails (CSW status 1).
    always_fail: bool,
    /// This opcode ends in a phase error.
    phase_error: Option<u8>,
    /// Added to the tag of every status wrapper.
    tag_skew: u32,
    /// (opcode, tag, lba) of every command.
    log: Log,
}

impl Mock {
    fn new(disk: Vec<u8>) -> Self {
        Mock {
            disk,
            phase: Phase::Cbw,
            unit_not_ready: 0,
            always_fail: false,
            phase_error: None,
            tag_skew: 0,
            log: Arc::default(),
        }
    }

    fn opcodes(&self) -> Vec<u8> {
        self.log.lock().unwrap().iter().map(|&(op, _, _)| op).collect()
    }
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

impl BulkPipes for Mock {
    fn bulk_out(&mut self, data: &[u8]) -> Result<usize> {
        assert!(!data.is_empty(), "zero-length bulk OUT transfer");
        match std::mem::replace(&mut self.phase, Phase::Cbw) {
            Phase::Cbw => {
                assert_eq!(data.len(), CBW_LEN);
                assert_eq!(&data[0..4], &0x4342_5355u32.to_le_bytes());
                let tag = u32::from_le_bytes(data[4..8].try_into().unwrap());
                let data_len = u32::from_le_bytes(data[8..12].try_into().unwrap());
                let cdb = &data[15..15 + data[14] as usize];
                let op = cdb[0];
                let lba = if cdb.len() == 10 { be32(&cdb[2..6]) } else { 0 };
                self.log.lock().unwrap().push((op, tag, lba));
                let fail = self.always_fail as u8;
                self.phase = if self.phase_error == Some(op) {
                    Phase::Csw(tag, 2)
                } else {
                    match op {
                        0x00 => {
                            let ready = self.unit_not_ready == 0;
                            self.unit_not_ready = self.unit_not_ready.saturating_sub(1);
                            Phase::Csw(tag, if ready { fail } else { 1 })
                        }
                        0x12 => {
                            let mut inq = vec![0u8; 36];
                            inq[8..16].copy_from_slice(b"VENDOR  ");
                            inq[16..32].copy_from_slice(b"PRODUCT         ");
                            inq.truncate(cdb[4] as usize);
                            Phase::DataIn(inq, tag, fail)
                        }
                        0x03 => {
                            let mut sense = vec![0u8; 18];
                            sense[2] = 0x06; // UNIT ATTENTION
                            sense.truncate(cdb[4] as usize);
                            Phase::DataIn(sense, tag, 0)
                        }
                        0x28 => {
                            let start = lba as usize * SECTOR;
                            let len = u16::from_be_bytes([cdb[7], cdb[8]]) as usize * SECTOR;
                            match self.disk.get(start..start + len) {
                                Some(d) => Phase::DataIn(d.to_vec(), tag, fail),
                                None => Phase::DataIn(Vec::new(), tag, 1),
                            }
                        }
                        0x2a => {
                            Phase::DataOut { lba, len: u16::from_be_bytes([cdb[7], cdb[8]]) as usize * SECTOR, tag }
                        }
                        _ => Phase::Csw(tag, 1),
                    }
                };
                // no data stage when the CBW announces none
                if data_len == 0 {
                    self.phase = match std::mem::replace(&mut self.phase, Phase::Cbw) {
                        Phase::DataIn(_, tag, status) => Phase::Csw(tag, status),
                        Phase::DataOut { tag, .. } => Phase::Csw(tag, 0),
                        other => other,
                    };
                }
                Ok(CBW_LEN)
            }
            Phase::DataOut { lba, len, tag } => {
                assert_eq!(data.len(), len);
                let start = lba as usize * SECTOR;
                self.disk[start..start + len].copy_from_slice(data);
                self.phase = Phase::Csw(tag, 0);
                Ok(len)
            }
            _ => panic!("bulk OUT out of phase"),
        }
    }

    fn bulk_in(&mut self, buf: &mut [u8]) -> Result<usize> {
        assert!(!buf.is_empty(), "zero-length bulk IN transfer");
        match std::mem::replace(&mut self.phase, Phase::Cbw) {
            Phase::DataIn(d, tag, status) => {
                let n = d.len().min(buf.len());
                buf[..n].copy_from_slice(&d[..n]);
                self.phase = Phase::Csw(tag, status);
                Ok(n)
            }
            Phase::Csw(tag, status) => {
                assert_eq!(buf.len(), CSW_LEN);
                buf[0..4].copy_from_slice(&0x5342_5355u32.to_le_bytes());
                buf[4..8].copy_from_slice(&tag.wrapping_add(self.tag_skew).to_le_bytes());
                buf[8..12].fill(0);
                buf[12] = status;
                Ok(CSW_LEN)
            }
            _ => panic!("bulk IN out of phase"),
        }
    }
}

/// A disk whose MBR has one partition of `sectors` from `start`.
fn disk(kind: u8, start: u32, sectors: u32, total: usize) -> Vec<u8> {
    let mut d = vec![0u8; total * SECTOR];
    d[510] = 0x55;
    d[511] = 0xaa;
    let e = &mut d[446..462];
    e[4] = kind;
    e[8..12].copy_from_slice(&start.to_le_bytes());
    e[12..16].copy_from_slice(&sectors.to_le_bytes());
    for (i, b) in d.iter_mut().enumerate().skip(SECTOR) {
        *b = (i / SECTOR) as u8;
    }
    d
}

fn location() -> Location {
    Location { bus: 1, dev: 2, interface: 0 }
}

/// A device probed from `mock`, and the mock's command log.
fn probe(mock: Mock) -> Result<(Device, Log)> {
    let log = mock.log.clone();
    let pipes: Box<dyn BulkPipes> = Box::new(mock);
    let (scsi, part) = Attached::new(pipes).init()?.partition()?;
    Ok((Device::new(7, location(), scsi, part), log))
}

// --- transport ---

#[test]
fn cbw_and_csw_encoding() {
    let cbw = bot::encode_cbw(0xdead_beef, 512, true, &[0x28; 10]).unwrap();
    assert_eq!(&cbw[0..4], b"USBC");
    assert_eq!(&cbw[4..8], &0xdead_beefu32.to_le_bytes());
    assert_eq!(&cbw[8..12], &512u32.to_le_bytes());
    assert_eq!((cbw[12], cbw[13], cbw[14]), (0x80, 0, 10));
    assert_eq!(bot::encode_cbw(0, 0, false, &[0; 17]), Err(Errno::EINVAL));
    assert_eq!(bot::encode_cbw(0, 0, false, &[]), Err(Errno::EINVAL));

    let mut csw = [0u8; CSW_LEN];
    csw[0..4].copy_from_slice(b"USBS");
    csw[4] = 9;
    csw[8] = 3;
    csw[12] = 1;
    assert_eq!(bot::decode_csw(&csw), Some(bot::Csw { tag: 9, residue: 3, status: 1 }));
    csw[0] = b'X';
    assert_eq!(bot::decode_csw(&csw), None);
}

#[test]
fn tags_wrap_around() {
    let mut bot = Bot::new(Mock::new(disk(0x83, 1, 4, 8)));
    bot.set_tag(u32::MAX);
    assert_eq!(bot.command(&[0; 6], Data::None), Ok(Status::Passed(0)));
    assert_eq!(bot.command(&[0; 6], Data::None), Ok(Status::Passed(0)));
    let tags: Vec<u32> = bot.pipes().log.lock().unwrap().iter().map(|&(_, t, _)| t).collect();
    assert_eq!(tags, [u32::MAX, 0]);
}

#[test]
fn status_wrapper_of_another_command_is_an_error() {
    let mut mock = Mock::new(disk(0x83, 1, 4, 8));
    mock.tag_skew = 1;
    assert_eq!(Bot::new(mock).command(&[0; 6], Data::None), Err(Errno::EIO));
}

// --- SCSI ---

#[test]
fn failed_command_is_retried_after_request_sense() {
    let mut mock = Mock::new(disk(0x83, 1, 4, 8));
    mock.unit_not_ready = 2;
    let mut scsi = Scsi::new(mock);
    assert_eq!(scsi.test_unit_ready(), Ok(()));
    assert_eq!(scsi.bot().pipes().opcodes(), [0x00, 0x03, 0x00, 0x03, 0x00]);

    let mut mock = Mock::new(disk(0x83, 1, 4, 8));
    mock.always_fail = true;
    let mut scsi = Scsi::new(mock);
    assert_eq!(scsi.test_unit_ready(), Err(Errno::EIO), "gives up after 3 tries");
    assert_eq!(scsi.bot().pipes().opcodes().len(), 6);
}

#[test]
fn phase_error_is_not_retried() {
    let mut mock = Mock::new(disk(0x83, 1, 4, 8));
    mock.phase_error = Some(0x00);
    let mut scsi = Scsi::new(mock);
    assert_eq!(scsi.test_unit_ready(), Err(Errno::EIO));
    assert_eq!(scsi.bot().pipes().opcodes(), [0x00]);
}

#[test]
fn inquiry_asks_for_its_whole_length() {
    // the C driver put htons(36) in a one-byte field: 0 on little-endian
    let mut scsi = Scsi::new(Mock::new(disk(0x83, 1, 4, 8)));
    let inq = scsi.inquiry().unwrap();
    assert_eq!((inq.vendor.as_str(), inq.product.as_str()), ("VENDOR", "PRODUCT"));
}

#[test]
fn sense_is_parsed() {
    let mut scsi = Scsi::new(Mock::new(disk(0x83, 1, 4, 8)));
    assert_eq!(scsi.request_sense().map(|s| s.key), Ok(0x06));
}

// --- probing ---

#[test]
fn probe_needs_a_ready_unit_and_inquiry_in_one_round() {
    let mut mock = Mock::new(disk(0x83, 1, 4, 8));
    mock.unit_not_ready = 5; // two rounds' worth with the retries
    assert!(probe(mock).is_ok());

    let mut mock = Mock::new(disk(0x83, 1, 4, 8));
    mock.unit_not_ready = usize::MAX;
    assert_eq!(probe(mock).err(), Some(Errno::EIO));
}

#[test]
fn mbr_first_partition() {
    let d = disk(0x83, 2048, 100, 1);
    let sector0: &[u8; SECTOR] = d[..SECTOR].try_into().unwrap();
    assert_eq!(mbr::first_partition(sector0), Ok(Partition { kind: 0x83, start: 2048, sectors: 100 }));

    let mut bad = *sector0;
    bad[510] = 0;
    assert_eq!(mbr::first_partition(&bad), Err(MbrError::BadMagic(0xaa00)));
    let empty = disk(0, 0, 0, 1);
    assert_eq!(mbr::first_partition(empty[..SECTOR].try_into().unwrap()), Err(MbrError::NoPartition));
    assert_eq!(probe(Mock::new(disk(0, 0, 0, 2))).err(), Some(Errno::EINVAL));
}

// --- I/O ---

#[test]
fn reads_and_writes_land_in_the_partition() {
    let (dev, log) = probe(Mock::new(disk(0x83, 2, 4, 8))).unwrap();
    assert_eq!(dev.size(), 4 * SECTOR as u64);

    let mut buf = vec![0u8; SECTOR];
    assert_eq!(dev.read(SECTOR as i64, &mut buf), Ok(SECTOR));
    assert!(buf.iter().all(|&b| b == 3), "partition sector 1 is disk sector 3");

    let data = vec![0xa5u8; 2 * SECTOR];
    assert_eq!(dev.write(0, &data), Ok(2 * SECTOR));
    let mut back = vec![0u8; 2 * SECTOR];
    assert_eq!(dev.read(0, &mut back), Ok(2 * SECTOR));
    assert_eq!(back, data);
    assert_eq!(log.lock().unwrap().last().map(|&(op, _, lba)| (op, lba)), Some((0x28, 2)));
}

#[test]
fn zero_length_requests_send_nothing() {
    // libext2 reads the whole-block part of a GDT smaller than a block: 0 bytes
    let (dev, log) = probe(Mock::new(disk(0x83, 2, 4, 8))).unwrap();
    let before = log.lock().unwrap().len();
    assert_eq!(dev.read(2 * SECTOR as i64, &mut []), Ok(0));
    assert_eq!(dev.write(0, &[]), Ok(0));
    assert_eq!(log.lock().unwrap().len(), before);
    assert_eq!(dev.read(5 * SECTOR as i64, &mut []), Err(Errno::EINVAL), "still bounds-checked");
}

#[test]
fn command_without_data_has_no_data_stage() {
    // a READ(10) of 0 blocks: the mock refuses zero-length transfers
    let mut scsi = Scsi::new(Mock::new(disk(0x83, 2, 4, 8)));
    assert_eq!(scsi.read(2, &mut []), Ok(0));
}

#[test]
fn out_of_bounds_requests_are_refused() {
    let (dev, log) = probe(Mock::new(disk(0x83, 2, 4, 8))).unwrap();
    let before = log.lock().unwrap().len();
    let mut buf = vec![0u8; SECTOR];
    assert_eq!(dev.read(-(SECTOR as i64), &mut buf), Err(Errno::EINVAL), "negative offset");
    assert_eq!(dev.read(1, &mut buf), Err(Errno::EINVAL), "unaligned offset");
    assert_eq!(dev.read(0, &mut buf[..100]), Err(Errno::EINVAL), "unaligned length");
    assert_eq!(dev.read(4 * SECTOR as i64, &mut buf), Err(Errno::EINVAL), "past the end");
    assert_eq!(dev.read(i64::MAX - 511, &mut buf), Err(Errno::EINVAL), "offset + length overflows");
    assert_eq!(log.lock().unwrap().len(), before, "nothing reached the device");
}

#[test]
fn requests_are_cut_to_the_transfer_limit() {
    let (dev, _) = probe(Mock::new(disk(0x83, 1, 16, 20))).unwrap();
    let mut buf = vec![0u8; 12 * SECTOR];
    assert_eq!(dev.read(0, &mut buf), Ok(MAX_TRANSFER));
}

#[test]
fn partitions_past_4_gib() {
    // 16 Mi sectors = 8 GiB: 32-bit `sectors * 512` wrapped to 0 in C
    let part = Partition { kind: 0x83, start: 1, sectors: 1 << 24 };
    let mock = Mock::new(disk(0x83, 1, 4, 8));
    let log = mock.log.clone();
    let dev = Device::new(0, location(), Scsi::new(Box::new(mock) as Box<dyn BulkPipes>), part);
    assert_eq!(dev.size(), 8 << 30);
    let mut buf = vec![0u8; SECTOR];
    let offset = 5i64 << 30;
    // the mock disk is small, so the command fails, but it is sent, to the right sector
    let _ = dev.read(offset, &mut buf);
    assert_eq!(log.lock().unwrap()[0], (0x28, 0, 1 + (offset / SECTOR as i64) as u32));

    let end = Partition { kind: 0x83, start: u32::MAX - 1, sectors: 8 };
    let dev = Device::new(0, location(), Scsi::new(Box::new(Mock::new(Vec::new())) as Box<dyn BulkPipes>), end);
    assert_eq!(dev.read(4 * SECTOR as i64, &mut buf), Err(Errno::EINVAL), "LBA past 32 bits");
}

#[test]
fn unplugged_device_fails_requests() {
    let (dev, _) = probe(Mock::new(disk(0x83, 1, 4, 8))).unwrap();
    dev.detach();
    assert!(dev.is_gone());
    assert_eq!(dev.read(0, &mut [0u8; SECTOR]), Err(Errno::ENODEV));
    assert_eq!(dev.write(0, &[0u8; SECTOR]), Err(Errno::ENODEV));
}

// --- registry ---

#[test]
fn registry_reuses_the_lowest_free_id() {
    let reg: Registry<u32> = Registry::default();
    let a = reg.reserve();
    let b = reg.reserve();
    assert_eq!((a.id(), b.id()), (0, 1));
    assert_eq!(reg.get(0), None, "reserved, not yet visible");
    a.fill(Arc::new(10));
    drop(b); // probing failed
    assert_eq!(reg.reserve().id(), 1);
    let c = reg.reserve();
    c.fill(Arc::new(20));
    assert_eq!(reg.get(0).as_deref(), Some(&10));
    assert_eq!(reg.get(1).as_deref(), Some(&20));

    let removed = reg.remove_where(|&v| v == 10);
    assert_eq!(removed.len(), 1);
    assert_eq!(reg.get(0), None);
    assert_eq!(reg.reserve().id(), 0);
}

// --- filesystems ---

struct SlowFs {
    active: Arc<AtomicUsize>,
    handled: Arc<AtomicUsize>,
}

impl MountedFs for SlowFs {
    fn handle(&self, msg: &mut RawMsg) {
        self.active.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(50));
        msg.o.err = 0;
        self.handled.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_sub(1, Ordering::SeqCst);
    }

    fn unmount(self: Box<Self>) -> Result<()> {
        assert_eq!(self.active.load(Ordering::SeqCst), 0, "unmounted while a message was being handled");
        Ok(())
    }
}

#[test]
fn unmount_waits_for_messages_being_handled() {
    let active = Arc::new(AtomicUsize::new(0));
    let handled = Arc::new(AtomicUsize::new(0));
    let mounted = Arc::new(Mounted::new(Box::new(SlowFs { active: active.clone(), handled: handled.clone() })));

    let workers: Vec<_> = (0..2)
        .map(|_| {
            let m = mounted.clone();
            std::thread::spawn(move || m.handle(&mut RawMsg::zeroed()))
        })
        .collect();
    // both inside the filesystem at once: handling takes the lock shared
    while active.load(Ordering::SeqCst) < 2 {
        std::thread::yield_now();
    }
    assert_eq!(mounted.unmount(), Ok(()));
    for w in workers {
        assert!(w.join().unwrap(), "both were handled before the unmount");
    }
    assert_eq!(handled.load(Ordering::SeqCst), 2);
    assert!(!mounted.handle(&mut RawMsg::zeroed()), "nothing is handled after it");
    assert_eq!(mounted.unmount(), Err(Errno::ENOENT));
}

struct NamedFs(&'static str, u8);

impl Filesystem for NamedFs {
    fn name(&self) -> &str {
        self.0
    }

    fn partition_type(&self) -> u8 {
        self.1
    }

    fn mount(&self, _: Oid) -> Result<(Id, Box<dyn MountedFs>)> {
        Err(Errno::ENOSYS)
    }
}

#[test]
fn filesystems_by_name_and_partition_type() {
    let mut fss = Filesystems::default();
    assert!(fss.by_name("ext2").is_none(), "none built in");
    fss.register(Box::new(NamedFs("ext2", 0x83)));
    fss.register(Box::new(NamedFs("fat", 0x0c)));
    assert_eq!(fss.by_name("fat").map(|f| f.partition_type()), Some(0x0c));
    assert_eq!(fss.by_partition_type(0x83).map(|f| f.name()), Some("ext2"));
    assert!(fss.by_partition_type(0x07).is_none());
}
