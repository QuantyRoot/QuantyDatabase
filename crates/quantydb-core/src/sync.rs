//! Locks, over the standard library.
//!
//! These are thin wrappers rather than direct use of `std::sync`, and they
//! exist for one reason: to put the poisoning policy in a single place with
//! its justification next to it, instead of in a dozen call sites where it
//! would eventually be decided differently.
//!
//! **A poisoned lock is taken anyway.** `std` marks a lock as poisoned when
//! a thread panics while holding it, so that other threads find out the
//! data behind it may be half updated. That is the right default for
//! arbitrary data, and it is not what this database needs, because the
//! state these locks guard is not where durability lives.
//!
//! Concretely: the pager's in-memory `committed` metadata is only replaced
//! after a commit has already been made durable on disk, and a write
//! transaction that panics is dropped without committing, so the on-disk
//! state is the previous commit either way. A panic therefore cannot leave
//! the guarded state disagreeing with the file. What can leave the file
//! inconsistent is a process that dies mid-write, and that is covered by
//! the crash harness rather than by lock poisoning.
//!
//! This also keeps the behaviour that was here before: parking_lot has no
//! poisoning at all, so taking the value on poison is what the code has
//! always done, now written down instead of inherited.

use std::sync::{self, LockResult};

/// A mutual exclusion lock.
#[derive(Debug, Default)]
pub struct Mutex<T>(sync::Mutex<T>);

pub type MutexGuard<'a, T> = sync::MutexGuard<'a, T>;
pub type RwLockReadGuard<'a, T> = sync::RwLockReadGuard<'a, T>;
pub type RwLockWriteGuard<'a, T> = sync::RwLockWriteGuard<'a, T>;

impl<T> Mutex<T> {
    pub fn new(value: T) -> Mutex<T> {
        Mutex(sync::Mutex::new(value))
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        unpoison(self.0.lock())
    }
}

/// A reader-writer lock.
#[derive(Debug, Default)]
pub struct RwLock<T>(sync::RwLock<T>);

impl<T> RwLock<T> {
    pub fn new(value: T) -> RwLock<T> {
        RwLock(sync::RwLock::new(value))
    }

    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        unpoison(self.0.read())
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        unpoison(self.0.write())
    }
}

/// Take the guard whether or not the lock was poisoned. See the note at the
/// top of this file for why that is the right answer here.
fn unpoison<T>(result: LockResult<T>) -> T {
    match result {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_mutex_excludes() {
        let counter = Arc::new(Mutex::new(0u32));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let counter = Arc::clone(&counter);
            threads.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    *counter.lock() += 1;
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(*counter.lock(), 8000);
    }

    #[test]
    fn readers_share_and_writers_do_not() {
        let lock = Arc::new(RwLock::new(vec![1u8, 2, 3]));
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let lock = Arc::clone(&lock);
                std::thread::spawn(move || lock.read().len())
            })
            .collect();
        for reader in readers {
            assert_eq!(reader.join().unwrap(), 3);
        }
        lock.write().push(4);
        assert_eq!(lock.read().len(), 4);
    }

    #[test]
    fn a_lock_poisoned_by_a_panic_still_opens() {
        // the whole reason these wrappers exist: after this thread dies
        // holding the lock, everything else has to keep working
        let lock = Arc::new(Mutex::new(7u32));
        let stolen = Arc::clone(&lock);
        let dead = std::thread::spawn(move || {
            let _guard = stolen.lock();
            panic!("on purpose");
        });
        assert!(dead.join().is_err(), "the thread panicked as intended");

        assert_eq!(*lock.lock(), 7);
        *lock.lock() += 1;
        assert_eq!(*lock.lock(), 8);

        let rw = Arc::new(RwLock::new(String::from("intact")));
        let stolen = Arc::clone(&rw);
        let dead = std::thread::spawn(move || {
            let _guard = stolen.write();
            panic!("on purpose");
        });
        assert!(dead.join().is_err());
        assert_eq!(&*rw.read(), "intact");
    }
}
