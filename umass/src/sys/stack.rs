//! Stack high-water marks, for sizing the driver's threads (feature
//! `stack-report`): each thread paints its unused stack when it starts, and
//! [`report`] finds how deep each one has written since.
//!
//! The stack's top is taken as the end of the page the thread starts in:
//! std's and libphoenix's start-up frames are well under a page, and the
//! stacks are page-aligned mappings of whole pages.

use std::sync::Mutex;

const PAINT: u8 = 0xa5;
const PAGE: usize = 4096;
/// The kernel's canary at the bottom of every user stack.
const CANARY: usize = 16;
/// Left unpainted below the painting function's own frame.
const MARGIN: usize = 512;

struct Stack {
    name: String,
    bottom: usize,
    top: usize,
}

static STACKS: Mutex<Vec<Stack>> = Mutex::new(Vec::new());

/// Paints the calling thread's stack of `size` bytes below the current frame.
pub fn paint(name: &str, size: usize) {
    let marker = 0u8;
    let sp = core::ptr::addr_of!(marker) as usize;
    let top = sp.next_multiple_of(PAGE);
    let Some(bottom) = top.checked_sub(size) else { return };
    let start = bottom + CANARY;
    let end = sp.saturating_sub(MARGIN);
    for addr in start..end {
        // SAFETY: [bottom, top) is this thread's stack (see the module
        // documentation); `start..end` lies between the kernel's canary and
        // the frames in use, so nothing lives there yet, and i386 has no red
        // zone below the stack pointer.
        unsafe { core::ptr::write_volatile(addr as *mut u8, PAINT) };
    }
    STACKS.lock().unwrap_or_else(|e| e.into_inner()).push(Stack { name: name.into(), bottom, top });
}

/// Each painted thread's name, deepest use and size, in bytes.
pub fn report() -> Vec<(String, usize, usize)> {
    let stacks = STACKS.lock().unwrap_or_else(|e| e.into_inner());
    stacks
        .iter()
        .map(|s| {
            let mut addr = s.bottom + CANARY;
            // SAFETY: the stack's pages stay mapped while its thread runs, and
            // the driver's threads never end. The reads race with the owner's
            // writes; a byte read mid-change only moves the mark by one frame.
            while addr < s.top && unsafe { core::ptr::read_volatile(addr as *const u8) } == PAINT {
                addr += 1;
            }
            (s.name.clone(), s.top - addr, s.top - s.bottom)
        })
        .collect()
}
