//! The driver: USB insertion and deletion, the `/dev/umassN` server, and the
//! servers of mounted partitions.
//!
//! Threads: libusb's event thread (insertion, deletion); `MSG_THREADS`
//! serving the device files; per mounted partition, one receiving its
//! messages; `POOL_THREADS` handling those.
//!
//! Locks, outermost first: `mounts` → `tokens` → `devices` → a device's link.
//! Each is held for a map access or one SCSI command, never across a message
//! wait. `mounts` is held across a filesystem's mount, whose I/O takes only
//! the inner ones.

use crate::bot::BulkPipes;
use crate::device::{Attached, Device, Location};
use crate::fs::{BlockIo, Filesystem, Filesystems, Mounted};
use crate::registry::Registry;
use crate::sys;
use crate::sys::usb::{Handler, Inserted, UsbDevice};
use phoenix::msg::raw::MT_UMOUNT;
use phoenix::msg::{AttrType, Port, Received, Reply, Request};
use phoenix::{Errno, Id, Oid, Result};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;

const MSG_THREADS: usize = 2;
const POOL_THREADS: usize = 2;
/// Static worst cases on ia32 (release build, call graph from the binary):
/// message threads ~1.3 KB (an ext2 mount), pool threads ~1.7 KB (an ext2
/// create down to our block I/O), receivers ~1 KB: one page each. Printing a
/// panic or an error is bounded only loosely (below 6 KB); see REWRITE.md.
const MSG_STACK: usize = 4 * 1024;
const RECEIVER_STACK: usize = 4 * 1024;
const POOL_STACK: usize = 4 * 1024;

macro_rules! log {
    ($($arg:tt)*) => { println!("umass: {}", format_args!($($arg)*)) };
}

macro_rules! log_error {
    ($($arg:tt)*) => { eprintln!("umass: error: {}", format_args!($($arg)*)) };
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A mounted partition: its port and filesystem, and the device it was
/// mounted from, kept even after that device is removed.
struct Partition {
    port: Port,
    mounted: Mounted,
    device: Arc<Device>,
}

/// A partition's message, for a pool thread.
struct Job {
    part: Arc<Partition>,
    msg: Received,
}

/// Mount tokens: what a filesystem reaches its device by.
struct Tokens {
    next: Id,
    devices: BTreeMap<Id, Arc<Device>>,
}

pub struct Driver {
    port: Port,
    devices: Registry<Device>,
    /// By mount token.
    mounts: Mutex<BTreeMap<Id, Arc<Partition>>>,
    tokens: Mutex<Tokens>,
    filesystems: OnceLock<Filesystems>,
    /// Mount the next device inserted as the root filesystem.
    mount_root: AtomicBool,
    jobs: Sender<Job>,
}

static DRIVER: OnceLock<Driver> = OnceLock::new();

/// Creates the driver and starts its threads; then [`sys::usb::run`] feeds
/// it USB events.
pub fn start(mount_root: bool) -> Result<&'static Driver> {
    let port = Port::new()?;
    let (jobs, queue) = mpsc::channel();
    let driver = Driver {
        port,
        devices: Registry::default(),
        mounts: Mutex::default(),
        tokens: Mutex::new(Tokens { next: 0, devices: BTreeMap::new() }),
        filesystems: OnceLock::new(),
        mount_root: AtomicBool::new(mount_root),
        jobs,
    };
    if DRIVER.set(driver).is_err() {
        return Err(Errno::EBUSY);
    }
    let driver = DRIVER.get().expect("just set");
    let _ = driver.filesystems.set(filesystems(driver));

    for i in 0..MSG_THREADS {
        spawn(&format!("umass-msg{i}"), MSG_STACK, move || {
            if let Err(e) = driver.port.serve(|req| driver.handle(req)) {
                log_error!("message thread: {e}");
            }
        })?;
    }
    let queue = Arc::new(Mutex::new(queue));
    for i in 0..POOL_THREADS {
        let queue = queue.clone();
        spawn(&format!("umass-pool{i}"), POOL_STACK, move || driver.work(&queue))?;
    }
    #[cfg(feature = "stack-report")]
    thread::Builder::new().name("umass-stacks".into()).stack_size(MSG_STACK).spawn(report_stacks).map_err(Errno::from)?;
    Ok(driver)
}

fn filesystems(#[allow(unused_variables)] driver: &'static Driver) -> Filesystems {
    #[allow(unused_mut)]
    let mut fss = Filesystems::default();
    #[cfg(feature = "ext2")]
    fss.register(Box::new(sys::ext2::Ext2::new(driver)));
    fss
}

fn spawn(name: &str, stack: usize, f: impl FnOnce() + Send + 'static) -> Result<()> {
    #[cfg(feature = "stack-report")]
    let f = {
        let name = name.to_string();
        move || {
            sys::stack::paint(&name, stack);
            f()
        }
    };
    thread::Builder::new().name(name.into()).stack_size(stack).spawn(f).map(drop).map_err(Errno::from)
}

/// Logs the threads' deepest stack use whenever it grows.
#[cfg(feature = "stack-report")]
fn report_stacks() {
    let mut last = Vec::new();
    loop {
        thread::sleep(std::time::Duration::from_secs(5));
        let now = sys::stack::report();
        if now != last {
            for (name, used, size) in &now {
                log!("stack {name}: {used} of {size} bytes");
            }
            last = now;
        }
    }
}

impl Driver {
    fn device(&self, oid: Oid) -> Result<Arc<Device>> {
        self.devices.get(oid.id).ok_or(Errno::ENOENT)
    }

    /// A message to `/dev/umassN`.
    fn handle(&self, req: Request<'_>) -> Result<Reply> {
        match req {
            // the device file's removal after a deletion can come back to us
            Request::Unlink { .. } => Ok(Reply::Done),
            Request::Open { oid, .. } | Request::Close { oid, .. } => self.device(oid).map(|_| Reply::Done),
            Request::Read { oid, offset, buf } => self.device(oid)?.read(offset, buf).map(Reply::Bytes),
            Request::Write { oid, offset, data, .. } => {
                let len = self.device(oid)?.write(offset, data)?;
                Ok(Reply::Written { len, end: offset + len as i64 })
            }
            Request::GetAttr { oid, attr: AttrType::Size } => {
                Ok(Reply::Attr(i64::try_from(self.device(oid)?.size()).map_err(|_| Errno::EINVAL)?))
            }
            Request::GetAttr { .. } => Err(Errno::EINVAL),
            Request::Mount { oid, fstype, .. } => {
                let dev = self.device(oid)?;
                let name = fstype.to_str().ok_or(Errno::EINVAL)?;
                self.mount(&dev, Some(name)).map(Reply::Mounted)
            }
            _ => Err(Errno::ENOSYS),
        }
    }

    /// Mounts the device's partition with the filesystem named `fstype`, or
    /// the one its partition type calls for; the filesystem's root.
    fn mount(&self, dev: &Arc<Device>, fstype: Option<&str>) -> Result<Oid> {
        let fss = self.filesystems.get().ok_or(Errno::ENOSYS)?;
        let mut mounts = lock(&self.mounts);
        if mounts.values().any(|p| Arc::ptr_eq(&p.device, dev) && p.mounted.is_mounted()) {
            return Err(Errno::EEXIST);
        }
        let fs: &dyn Filesystem = match fstype {
            Some(name) => fss.by_name(name),
            None => fss.by_partition_type(dev.partition().kind),
        }
        .ok_or(Errno::ENOENT)?;

        let port = Port::new()?;
        let token = self.issue_token(dev);
        let (root, fs) = match fs.mount(Oid::new(port.id(), token)) {
            Ok(mounted) => mounted,
            Err(e) => {
                self.revoke_token(token);
                return Err(e);
            }
        };
        let part = Arc::new(Partition { port, mounted: Mounted::new(fs), device: dev.clone() });
        let root = Oid::new(part.port.id(), root);

        let receiver = part.clone();
        let jobs = self.jobs.clone();
        if let Err(e) = spawn("umass-part", RECEIVER_STACK, move || receive(receiver, jobs)) {
            let _ = part.mounted.unmount();
            self.revoke_token(token);
            return Err(e);
        }
        mounts.insert(token, part);
        Ok(root)
    }

    fn issue_token(&self, dev: &Arc<Device>) -> Id {
        let mut tokens = lock(&self.tokens);
        let token = tokens.next;
        tokens.next = tokens.next.wrapping_add(1);
        tokens.devices.insert(token, dev.clone());
        token
    }

    fn revoke_token(&self, token: Id) {
        lock(&self.tokens).devices.remove(&token);
    }

    /// A pool thread: handles partition messages until the driver goes.
    fn work(&self, queue: &Mutex<Receiver<Job>>) {
        loop {
            let Ok(Job { part, mut msg }) = lock(queue).recv() else { return };
            let raw = msg.raw();
            if raw.typ == MT_UMOUNT {
                let result = part.mounted.unmount();
                raw.o.err = result.err().map_or(0, |e| -e.code());
                let _ = part.port.respond(msg);
                if result.is_ok() {
                    self.unmounted(&part);
                }
            } else {
                if !part.mounted.handle(raw) {
                    raw.o.err = -Errno::ENOENT.code();
                }
                let _ = part.port.respond(msg);
            }
        }
    }

    /// Forgets an unmounted partition and closes its port, which ends its
    /// receiving thread.
    fn unmounted(&self, part: &Arc<Partition>) {
        let mut mounts = lock(&self.mounts);
        let token = mounts.iter().find(|(_, p)| Arc::ptr_eq(p, part)).map(|(&t, _)| t);
        if let Some(token) = token {
            mounts.remove(&token);
            self.revoke_token(token);
        }
        part.port.close();
    }
}

/// A partition's receiving thread: passes its messages to the pool until the
/// port is closed.
fn receive(part: Arc<Partition>, jobs: Sender<Job>) {
    loop {
        match part.port.receive() {
            Ok(Some(msg)) => {
                if jobs.send(Job { part: part.clone(), msg }).is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(e) => {
                log_error!("partition port: {e}");
                return;
            }
        }
    }
}

impl BlockIo for Driver {
    fn read(&self, token: Id, offset: i64, buf: &mut [u8]) -> Result<usize> {
        let dev = lock(&self.tokens).devices.get(&token).cloned().ok_or(Errno::ENODEV)?;
        dev.read(offset, buf)
    }

    fn write(&self, token: Id, offset: i64, data: &[u8]) -> Result<usize> {
        let dev = lock(&self.tokens).devices.get(&token).cloned().ok_or(Errno::ENODEV)?;
        dev.write(offset, data)
    }
}

impl Handler for Driver {
    fn insert(&self, mut usb: UsbDevice) -> Result<Inserted> {
        // freed again if anything below fails
        let slot = self.devices.reserve();
        let id = slot.id();

        usb.configure(1).inspect_err(|e| log_error!("setting the configuration failed: {e}"))?;
        let pipes: Box<dyn BulkPipes> = Box::new(usb.bulk_pipes().inspect_err(|e| log_error!("opening pipes failed: {e}"))?);
        let ready = Attached::new(pipes)
            .init()
            .inspect_err(|_| log_error!("the device did not initialize after the SCSI init sequence"))?;
        log!("{} {} (device type {:#x})", ready.inquiry.vendor, ready.inquiry.product, ready.inquiry.device_type);
        let (scsi, part) = ready.partition().inspect_err(|e| log_error!("no usable MBR partition: {e}"))?;

        let dev = Arc::new(Device::new(id, usb.location(), scsi, part));
        let path = format!("/dev/umass{id}");
        let oid = self.port.oid(id);
        sys::posix::create_device(oid, &path).inspect_err(|e| log_error!("creating {path} failed: {e}"))?;
        slot.fill(dev.clone());
        log!("New USB Mass Storage device: {path} sectors: {}", part.sectors);

        if self.mount_root.load(Ordering::SeqCst) {
            match self.mount(&dev, None).and_then(|root| sys::posix::register("/", root)) {
                Ok(()) => self.mount_root.store(false, Ordering::SeqCst),
                Err(e) => log_error!("mounting {path} as the root filesystem failed: {e}"),
            }
        }
        Ok(Inserted { oid, path })
    }

    fn delete(&self, at: Location) {
        for dev in self.devices.remove_where(|d| d.location() == at) {
            // a mounted partition keeps the device, whose requests now fail
            dev.detach();
            let path = format!("/dev/umass{}", dev.id());
            let _ = std::fs::remove_file(&path);
            log!("Device removed: {path}");
        }
    }
}
