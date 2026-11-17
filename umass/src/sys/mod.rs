//! Every `unsafe` line of the driver: the C libusb (USB host), libext2 and
//! libphoenix calls, each behind a safe function or type. Nothing here keeps
//! driver state; it only translates.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod cache;
#[cfg(feature = "ext2")]
pub mod ext2;
pub mod posix;
#[cfg(feature = "stack-report")]
pub mod stack;
pub mod usb;
