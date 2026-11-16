//! USB Mass Storage class driver: SCSI direct-access devices over the
//! Bulk-Only Transport, each served as `/dev/umassN`, its first partition
//! mountable with a registered filesystem.
//!
//! Two layers: `sys` holds every `unsafe` line (the C libusb, libext2 and
//! libphoenix calls); the rest is safe Rust, and all of it but the driver's
//! message loops is tested on the host against a mock device.

#![deny(unsafe_code)]

pub mod bot;
pub mod device;
pub mod fs;
pub mod mbr;
pub mod registry;
pub mod scsi;

#[cfg(target_os = "phoenix")]
pub mod driver;
#[cfg(target_os = "phoenix")]
#[allow(unsafe_code)]
pub mod sys;

#[cfg(test)]
mod tests;
