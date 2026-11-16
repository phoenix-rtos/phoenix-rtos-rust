# umass-rs: the USB Mass Storage driver in Rust

A rewrite of `phoenix-rtos-devices/storage/umass` (C, ~1,060 lines in
`umass.c`), not a translation. It serves the same devices the same way: SCSI
direct-access devices on the Bulk-Only Transport, each as `/dev/umassN`, whose
first MBR partition can be mounted (`mtMount`) or mounted as the root
filesystem (`-r`).

For now it is a USB **process driver** only, built as `umass-rs` on ia32 next
to the C `umass`. ia32 still runs the C driver linked into the USB server
(`libusbdrv-umass`).

## Layout

| Module | What | `unsafe` |
| --- | --- | --- |
| `sys/usb.rs` | libusb: the `usb_driver_t`, the event callbacks, pipes | yes |
| `sys/ext2.rs` | libext2 as a `Filesystem` (`ext2` feature) | yes |
| `sys/posix.rs` | `create_dev()`, `portRegister()` | yes |
| `bot.rs` | Bulk-Only Transport: CBW/CSW, tags | no |
| `scsi.rs` | commands, INQUIRY/sense parsing, retry after REQUEST SENSE | no |
| `mbr.rs` | the first partition | no |
| `device.rs` | probing (`Attached` → `Ready` → `Device`), bounds-checked I/O, `Present`/`Gone` | no |
| `registry.rs` | devices by id, ids reserved while probing | no |
| `fs.rs` | `Filesystem`/`MountedFs` traits, the unmount gate | no |
| `driver.rs` | insertion/deletion, the `/dev/umassN` server, partition servers | no |

`lib.rs` denies `unsafe_code` and allows it for `sys` alone, which also denies
`unsafe_op_in_unsafe_fn` and warns on `unsafe` blocks without a `// SAFETY:`
comment. All but `sys` and `driver` is tested on the host
(`cargo test -p umass`, also under Miri) against a mock SCSI device.

The driver has no registers, DMA or interrupts: the USB server owns the host
controller and copies transfer data through its own DMA memory, so a transfer
only lends a buffer for one blocking call.

### Threads and locks

libusb's event thread (insertion, deletion); two threads serving `/dev/umassN`;
per mounted partition one thread receiving its messages; two pool threads
handling them. Locks, outermost first: `mounts` → `tokens` → `devices` → a
device's link (one SCSI command at a time). None is held across a message
wait, and the device registry is not held across probing.

## Fixed compared to the C driver

Line numbers are `umass.c`'s.

1. **A device freed while in use.** Request paths looked the device up under
   `umass_common.lock`, released it, then used the pointer (`umass_read()`
   553-559, `umass_msgthr()` 727-750), while deletion freed it, mutex
   included (946). Devices are `Arc`s now; a removed one turns `Gone` and its
   requests fail with `ENODEV`.
2. **A running thread's stack freed.** A partition thread ran on
   `dev->part.fsstack`, inside the device struct (142, 610), which deletion
   freed (946). Threads own their stacks; a mounted partition holds its device.
3. **Unmount raced requests.** With two pool threads, `mtUmount` could set
   `part->fs = NULL` and unmount libext2 while the other thread was inside
   `fs->handler` (693-699). Handling takes a read lock and unmount the write
   lock, so an unmount waits for the requests in progress and none runs after.
4. **Partitions of 4 GiB and more.** `part.sectors * UMASS_SECTOR_SIZE` was a
   32-bit product (484, 514, 539): the bounds check and the size reported
   wrapped. Sizes and offsets are `u64` now.
5. **Negative offsets.** A negative, sector-aligned offset passed the checks
   (480-486) and became an LBA before the partition. Refused (`EINVAL`).
6. **LBA overflow.** `offs / 512 + part.start` was not checked against
   READ(10)/WRITE(10)'s 32-bit LBA (490, 520). It is.
7. **INQUIRY and REQUEST SENSE asked for 0 bytes on little-endian.** The 6-byte
   CDB's allocation length is one byte, set to `htons(len)` (349, 364): 0 on
   ia32, while the CBW announced 36 and 20 bytes. It is the length itself now.
8. **The command tag was a signed `int` incremented without bound** (248),
   undefined on overflow. It is a `u32` that wraps.
9. **Status wrappers of other commands were accepted.** A CSW with a wrong
   signature or tag but status 1 was retried as a failed command (278-292).
   It is a transport error now.
10. **Filesystem I/O after a device's removal could reach another device.**
    libext2 calls back with the id it was mounted with, which was the device
    id (601-603), reused by the next device inserted. Each mount gets its own
    token, which keeps naming the device it was made on.
11. **`umass_mountFromDev()` error paths.** It returned `1` when `portCreate()`
    failed (597), and left `part.fs` set after a failed `portCreate()` or mount
    (592-606), so every later mount answered `EEXIST`. Errors are returned as
    errors, and a failed mount leaves nothing behind.
12. **A partition's port was never destroyed** after `mtUmount`. It is closed
    after the unmount, which also ends its receiving thread.
13. **The partition thread spun on `malloc()` failure** (643-646). There is no
    retry loop.
14. **The device registry was locked across probing** (840-915: SCSI init and
    the MBR read), stalling requests to other devices. An id is reserved
    instead, and released if probing fails.
15. **An unused first MBR entry was accepted** as a 0-sector partition on
    which every request failed. Such a device is refused at insertion, like
    one without an MBR.
16. **INQUIRY's vendor and product were printed as `%s` without a terminator**
    (393-396, debug output). They are length-bounded strings.

## Changed on purpose

- **The root filesystem is picked by partition type** among the registered
  filesystems (ext2: 0x83), not hard-coded to ext2 (817); `mtMount` still names
  one. Filesystems are registered, ext2 behind the `ext2` feature, so a build
  without libext2 has none.
- **A failed root mount does not fail the insertion.** C returned the error
  after creating `/dev/umassN` (917-922), reporting a failed insertion for a
  device that had been created. It is logged; the device stays usable.
- **Messages to an unmounted partition** get `ENOENT`.

## Kept

The first partition only; LUN 0 only; requests cut to 4 KiB; no data stage
(and for a 0-byte request, no command at all) when there is no data; three tries of a
failed command with REQUEST SENSE between; ten rounds of TEST UNIT READY +
INQUIRY at insertion; two message and two pool threads; `/dev/umassN` names
with the lowest free N; `-r`; waiting for `/` otherwise.

## Not done (TODO)

- **The in-server variant** (`libusbdrv-umass`, linked into the USB server) is
  C only: this is a process driver.
- **Reset recovery.** A phase error or an invalid status wrapper should be
  followed by a Bulk-Only Mass Storage Reset and clearing both bulk endpoints'
  halt; that needs the endpoint addresses, which libusb's pipes do not expose.
  Such a command fails with `EIO`, as in C.
- **Pipes are never closed**: libusb has no call for it; the USB server frees
  a device's pipes when it goes away.
- **Thread priorities.** The C threads ran at priority 4; the Rust ones run
  at the default.
- **Thread stacks** are 4 KiB (one page) each, from a static estimate on
  ia32 (call graph and frame sizes read from the release binary, indirect
  calls resolved from the source): ~1.3 KB for the message threads, ~1.7 KB
  for the pool, ~1 KB for partition receivers. Formatting and panic paths are
  bounded only loosely, below 6 KB: printing an error or a panic message on
  these threads could exceed a page, and with no guard page that would
  silently corrupt memory below the stack. Not yet confirmed by a measurement
  on a device: build with `UMASS_FEATURES="ext2 stack-report"` and the driver
  logs each thread's deepest stack use every few seconds.
- **Targets.** Built on ia32 only, where libusb and libext2 are; the C driver
  also runs on armv7a7-imx6ull.
- **On target**: not yet run against a device (QEMU's `usb-storage`), see the
  section below once it is.

## Verification

- `cargo test -p umass` and `cargo +nightly miri test -p umass` on the host:
  17 tests (transport encoding, tag wrap-around, retries and phase errors,
  probing, MBR parsing, bounds and >4 GiB partitions, removal, the registry,
  the unmount gate, filesystem lookup).
- `cargo clippy -D warnings`: on the host, and with the fork's clippy for
  `i586`, `riscv64gc` and `armv7r-unknown-phoenix` (which covers `sys` and
  `driver`, Phoenix-only).
- `cargo check` for all seven Phoenix targets, with and without `ext2`.
- Built and linked in the ia32 project build (`make -C phoenix-rtos-rust
  umass-rs`), with libusb and libext2.
