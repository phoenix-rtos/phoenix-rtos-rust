//! The devices by id. The lock is held only to look up or change the map,
//! never across I/O: a lookup hands out an `Arc`, which keeps the device
//! alive however long the caller uses it.

use phoenix::Id;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

enum Slot<T> {
    /// Taken by a device still being probed.
    Reserved,
    Present(Arc<T>),
}

pub struct Registry<T> {
    slots: Mutex<BTreeMap<Id, Slot<T>>>,
}

impl<T> Default for Registry<T> {
    fn default() -> Self {
        Registry { slots: Mutex::new(BTreeMap::new()) }
    }
}

impl<T> Registry<T> {
    fn slots(&self) -> MutexGuard<'_, BTreeMap<Id, Slot<T>>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes the lowest free id, for a device about to be probed; it is free
    /// again when the reservation is dropped without being filled.
    pub fn reserve(&self) -> Reservation<'_, T> {
        let mut slots = self.slots();
        let mut id: Id = 0;
        for &taken in slots.keys() {
            if taken != id {
                break;
            }
            id += 1;
        }
        slots.insert(id, Slot::Reserved);
        Reservation { registry: self, id, filled: false }
    }

    pub fn get(&self, id: Id) -> Option<Arc<T>> {
        match self.slots().get(&id) {
            Some(Slot::Present(dev)) => Some(dev.clone()),
            _ => None,
        }
    }

    /// Removes the devices `pred` picks, returning them.
    pub fn remove_where(&self, mut pred: impl FnMut(&T) -> bool) -> Vec<Arc<T>> {
        let mut slots = self.slots();
        let ids: Vec<Id> = slots
            .iter()
            .filter_map(|(&id, slot)| matches!(slot, Slot::Present(dev) if pred(dev)).then_some(id))
            .collect();
        ids.into_iter()
            .filter_map(|id| match slots.remove(&id) {
                Some(Slot::Present(dev)) => Some(dev),
                _ => None,
            })
            .collect()
    }
}

/// An id taken for a device being probed.
pub struct Reservation<'a, T> {
    registry: &'a Registry<T>,
    id: Id,
    filled: bool,
}

impl<T> Reservation<'_, T> {
    pub fn id(&self) -> Id {
        self.id
    }

    /// Makes the device visible under the reserved id.
    pub fn fill(mut self, dev: Arc<T>) {
        self.registry.slots().insert(self.id, Slot::Present(dev));
        self.filled = true;
    }
}

impl<T> Drop for Reservation<'_, T> {
    fn drop(&mut self) {
        if !self.filled {
            self.registry.slots().remove(&self.id);
        }
    }
}
