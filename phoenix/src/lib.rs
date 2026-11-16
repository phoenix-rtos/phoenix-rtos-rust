//! Safe bindings for Phoenix-RTOS's own APIs, beside what `std` and `libc`
//! cover.
//!
//! * [`msg`]: message passing (`<sys/msg.h>`), for servers and their clients.
//! * [`Oid`] and [`Id`] name objects; [`Errno`] is Phoenix's error number.

pub mod msg;

mod errno;
mod oid;

pub use errno::Errno;
pub use oid::{Id, Oid};

pub type Result<T, E = Errno> = core::result::Result<T, E>;
