//! libcache (`phoenix-rtos-corelibs/libcache`) as a [`ReadCache`]: a
//! 4-way set-associative line cache with its own lock.
//!
//! Only its read side is used. Its write-through writes back a whole line,
//! which would turn every block written into a line read and written, so
//! writes go to the device directly and only invalidate the cache.

use crate::device::ReadCache;
use core::cell::Cell;
use core::ffi::{c_int, c_void};
use core::ptr::NonNull;
use phoenix::{Errno, Result};

type Fetch<'a> = &'a mut dyn FnMut(u64, &mut [u8]) -> Result<usize>;

type ReadCb = unsafe extern "C" fn(u64, *mut c_void, usize, *mut c_void) -> isize;
type WriteCb = unsafe extern "C" fn(u64, *const c_void, usize, *mut c_void) -> isize;

/// `cache_ops_t`
#[repr(C)]
struct CacheOps {
    read: ReadCb,
    write: WriteCb,
    ctx: *mut c_void,
}

unsafe extern "C" {
    fn cache_init(src_mem_size: usize, line_size: usize, lines_cnt: usize, ops: *const CacheOps) -> *mut c_void;
    fn cache_deinit(cache: *mut c_void) -> c_int;
    fn cache_read(cache: *mut c_void, addr: u64, buffer: *mut c_void, count: usize) -> isize;
    fn cache_invalidate(cache: *mut c_void, beg: u64, end: u64) -> c_int;
}

/// Where the read in progress keeps its line source for the callback: the
/// context libcache passes back is fixed when the cache is made.
type Slot = Cell<*mut c_void>;

pub struct LineCache {
    cache: NonNull<c_void>,
    slot: Box<Slot>,
    limit: u64,
}

// SAFETY: the cache is used through `&mut self` only, and libcache locks
// its own state; nothing in it is tied to the creating thread.
unsafe impl Send for LineCache {}

impl LineCache {
    /// A cache of `lines` lines of `line` bytes (powers of two; `lines` a
    /// multiple of 4, the ways) over `size` bytes.
    pub fn new(size: u64, line: usize, lines: usize) -> Result<LineCache> {
        // libcache addresses up to a size_t; beyond, reads bypass it
        let limit = usize::try_from(size).unwrap_or(usize::MAX);
        let slot = Box::new(Slot::new(core::ptr::null_mut()));
        let ops = CacheOps { read: fetch, write: refuse_write, ctx: (&raw const *slot).cast_mut().cast() };
        // SAFETY: `ops` is valid for the call (libcache copies it); `ctx`
        // points into `slot`, a box kept as long as the cache.
        let cache = unsafe { cache_init(limit, line, lines, &ops) };
        let cache = NonNull::new(cache).ok_or(Errno::ENOMEM)?;
        Ok(LineCache { cache, slot, limit: limit as u64 })
    }
}

impl ReadCache for LineCache {
    fn limit(&self) -> u64 {
        self.limit
    }

    fn read(&mut self, offset: u64, buf: &mut [u8], mut fetch: Fetch<'_>) -> Result<usize> {
        self.slot.set((&raw mut fetch).cast());
        // SAFETY: `cache` is live; `buf` is valid for writing its length; the
        // slot points at `fetch` for exactly this call, during which only
        // libcache's callback, on this thread, uses it.
        let n = unsafe { cache_read(self.cache.as_ptr(), offset, buf.as_mut_ptr().cast(), buf.len()) };
        self.slot.set(core::ptr::null_mut());
        if n < 0 { Err(Errno::from_raw(n as i32)) } else { Ok(n as usize) }
    }

    fn invalidate(&mut self, offset: u64, len: u64) {
        let end = offset.saturating_add(len).min(self.limit);
        if offset < end {
            // SAFETY: `cache` is live.
            unsafe { cache_invalidate(self.cache.as_ptr(), offset, end) };
        }
    }
}

impl Drop for LineCache {
    fn drop(&mut self) {
        // SAFETY: `cache` is live and, `self` being dropped, never used again.
        unsafe { cache_deinit(self.cache.as_ptr()) };
    }
}

/// libcache's line fill: the line source of the read in progress.
unsafe extern "C" fn fetch(offset: u64, buffer: *mut c_void, count: usize, ctx: *mut c_void) -> isize {
    // SAFETY: `ctx` is the slot `LineCache::new` registered, which lives as
    // long as the cache.
    let slot = unsafe { &*ctx.cast::<Slot>() };
    let source = slot.get().cast::<Fetch<'_>>();
    if source.is_null() || buffer.is_null() {
        return -(Errno::EIO.code() as isize);
    }
    // SAFETY: set by `LineCache::read` to its `fetch` for the duration of the
    // `cache_read` this callback runs in; `buffer` is valid for writing
    // `count` bytes, a line of libcache's own.
    let (source, line) = unsafe { (&mut *source, core::slice::from_raw_parts_mut(buffer.cast::<u8>(), count)) };
    match source(offset, line) {
        Ok(n) => n as isize,
        Err(e) => -(e.code() as isize),
    }
}

/// Never called: nothing is written through the cache.
unsafe extern "C" fn refuse_write(_: u64, _: *const c_void, _: usize, _: *mut c_void) -> isize {
    -(Errno::EIO.code() as isize)
}
