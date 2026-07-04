//! Userspace page cache with clock eviction.
//!
//! Deliberately boring: a map from page id to slot, a ring of slots, one
//! referenced bit per slot. Committed pages are immutable (COW discipline),
//! so this cache never has to deal with invalidation or dirty entries. That
//! property is what keeps this file short.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::page::PageId;

pub(crate) struct PageCache {
    inner: Mutex<Inner>,
    capacity: usize,
}

struct Inner {
    map: HashMap<PageId, usize>,
    slots: Vec<Slot>,
    hand: usize,
}

struct Slot {
    id: PageId,
    buf: Arc<[u8]>,
    referenced: bool,
}

impl PageCache {
    pub(crate) fn new(capacity: usize) -> Self {
        PageCache {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                slots: Vec::new(),
                hand: 0,
            }),
            capacity: capacity.max(8),
        }
    }

    pub(crate) fn get(&self, id: PageId) -> Option<Arc<[u8]>> {
        let mut inner = self.inner.lock();
        let &slot_idx = inner.map.get(&id)?;
        let slot = &mut inner.slots[slot_idx];
        slot.referenced = true;
        Some(Arc::clone(&slot.buf))
    }

    pub(crate) fn insert(&self, id: PageId, buf: Arc<[u8]>) {
        let mut inner = self.inner.lock();

        if let Some(&slot_idx) = inner.map.get(&id) {
            let slot = &mut inner.slots[slot_idx];
            slot.buf = buf;
            slot.referenced = true;
            return;
        }

        if inner.slots.len() < self.capacity {
            let slot_idx = inner.slots.len();
            inner.slots.push(Slot {
                id,
                buf,
                referenced: true,
            });
            inner.map.insert(id, slot_idx);
            return;
        }

        // Clock sweep: give referenced slots a second chance, evict the
        // first slot found resting.
        loop {
            let hand = inner.hand;
            inner.hand = (hand + 1) % inner.slots.len();
            let victim = &mut inner.slots[hand];
            if victim.referenced {
                victim.referenced = false;
                continue;
            }
            let old_id = victim.id;
            victim.id = id;
            victim.buf = buf;
            victim.referenced = true;
            inner.map.remove(&old_id);
            inner.map.insert(id, hand);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(byte: u8) -> Arc<[u8]> {
        Arc::from(vec![byte; 32].into_boxed_slice())
    }

    #[test]
    fn hit_and_miss() {
        let cache = PageCache::new(8);
        cache.insert(3, page(3));
        assert_eq!(cache.get(3).unwrap()[0], 3);
        assert!(cache.get(4).is_none());
    }

    #[test]
    fn eviction_keeps_capacity_bounded_and_correct() {
        let cache = PageCache::new(8);
        for id in 0..100u64 {
            cache.insert(id, page(id as u8));
        }
        let inner = cache.inner.lock();
        assert_eq!(inner.slots.len(), 8);
        assert_eq!(inner.map.len(), 8);
        drop(inner);
        // Whatever survived must map to its own content.
        for id in 0..100u64 {
            if let Some(buf) = cache.get(id) {
                assert_eq!(buf[0], id as u8);
            }
        }
    }
}
