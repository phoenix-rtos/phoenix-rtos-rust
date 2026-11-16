//! Object identifiers (`phoenix-rtos-kernel/include/types.h`).

use core::mem::{offset_of, size_of};

/// `id_t`, an object's id within its server: 32 bits on the Cortex-R targets
/// and SPARC (`arch/{armv7r,armv8r,sparcv8leon}/types.h`), 64 elsewhere.
#[cfg(any(target_env = "libphoenix_nommu", target_arch = "sparc"))]
pub type Id = u32;
#[cfg(not(any(target_env = "libphoenix_nommu", target_arch = "sparc")))]
pub type Id = u64;

/// `oid_t`: an object, named by the port of the server that owns it and an
/// id meaningful to that server.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Oid {
    pub port: u32,
    pub id: Id,
}

impl Oid {
    pub const fn new(port: u32, id: Id) -> Self {
        Oid { port, id }
    }

    /// The C representation with zeroed padding, as sent in message payloads
    /// such as `mtSetAttr(atDev)`.
    pub fn to_ne_bytes(self) -> [u8; size_of::<Oid>()] {
        let mut b = [0u8; size_of::<Oid>()];
        let id = offset_of!(Oid, id);
        b[..4].copy_from_slice(&self.port.to_ne_bytes());
        b[id..id + size_of::<Id>()].copy_from_slice(&self.id.to_ne_bytes());
        b
    }

    /// Parses [`Oid::to_ne_bytes`]; `None` unless `b` is exactly that size.
    pub fn from_ne_bytes(b: &[u8]) -> Option<Self> {
        let id = offset_of!(Oid, id);
        (b.len() == size_of::<Oid>()).then(|| Oid {
            port: u32::from_ne_bytes(b[..4].try_into().unwrap()),
            id: Id::from_ne_bytes(b[id..id + size_of::<Id>()].try_into().unwrap()),
        })
    }
}
