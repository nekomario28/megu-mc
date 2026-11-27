// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Poison-recovering `Mutex`/`RwLock` extension traits.
//!
//! A panic while a guard is held poisons the lock. The usual response in this
//! tree was a copy-pasted `.lock().unwrap_or_else(|e| e.into_inner())` at every
//! call site, which is correct but easy to forget: one `.unwrap()` in a
//! gameplay path turns a poisoned lock into a second panic that hides the
//! original one. These traits give that audited recovery exactly one spelling,
//! so a lock is either recovered deliberately or the call site says so.
//!
//! The traits are zero-cost: they compile to the same monomorphized guard
//! construction the inline closure produced, with no allocation, no dynamic
//! dispatch and no extra branch.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Poison-recovering [`Mutex`] access.
pub trait MutexExt<T: ?Sized> {
    /// Lock the mutex, recovering the guard from a poisoned lock instead of
    /// panicking. The poison flag is deliberately left set: recovery here is
    /// per-call, not a silent global "this state is now trusted again".
    #[must_use = "the guard is the only access to the protected value"]
    fn lock_unpoison(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> MutexExt<T> for Mutex<T> {
    #[inline]
    fn lock_unpoison(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Poison-recovering [`RwLock`] access.
pub trait RwLockExt<T: ?Sized> {
    /// Shared access, recovering from a poisoned lock instead of panicking.
    #[must_use = "the guard is the only access to the protected value"]
    fn read_unpoison(&self) -> RwLockReadGuard<'_, T>;

    /// Exclusive access, recovering from a poisoned lock instead of panicking.
    #[must_use = "the guard is the only access to the protected value"]
    fn write_unpoison(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> RwLockExt<T> for RwLock<T> {
    #[inline]
    fn read_unpoison(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }

    #[inline]
    fn write_unpoison(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::{MutexExt, RwLockExt};
    use std::sync::{Arc, Mutex, MutexGuard, RwLock};

    fn poison_mutex<T: Send + 'static>(mutex: &Arc<Mutex<T>>) {
        let clone = Arc::clone(mutex);
        let joined = std::thread::spawn(move || {
            let _guard = clone.lock().expect("fresh mutex is unpoisoned");
            panic!("poison the mutex");
        })
        .join();
        assert!(joined.is_err(), "the poisoning thread must have panicked");
    }

    fn poison_rwlock<T: Send + Sync + 'static>(lock: &Arc<RwLock<T>>) {
        let clone = Arc::clone(lock);
        let joined = std::thread::spawn(move || {
            let _guard = clone.write().expect("fresh rwlock is unpoisoned");
            panic!("poison the rwlock");
        })
        .join();
        assert!(joined.is_err(), "the poisoning thread must have panicked");
    }

    #[test]
    fn mutex_recovers_from_a_poisoned_lock() {
        let mutex = Arc::new(Mutex::new(1_i32));
        poison_mutex(&mutex);
        assert!(mutex.is_poisoned());

        {
            let mut guard: MutexGuard<'_, i32> = mutex.lock_unpoison();
            assert_eq!(*guard, 1);
            *guard = 7;
        }
        assert_eq!(*mutex.lock_unpoison(), 7);
        // Recovery is per call: the poison flag is not silently cleared.
        assert!(mutex.is_poisoned());
    }

    #[test]
    fn rwlock_read_and_write_recover_from_a_poisoned_lock() {
        let lock = Arc::new(RwLock::new(vec![1_u8, 2]));
        poison_rwlock(&lock);
        assert!(lock.is_poisoned());

        assert_eq!(*lock.read_unpoison(), vec![1, 2]);
        {
            let mut guard = lock.write_unpoison();
            guard.push(3);
        }
        assert_eq!(*lock.read_unpoison(), vec![1, 2, 3]);
        assert!(lock.is_poisoned());
    }

    #[test]
    fn unpoisoned_locks_behave_like_std() {
        let mutex = Mutex::new(String::from("ok"));
        mutex.lock_unpoison().push('!');
        assert_eq!(*mutex.lock_unpoison(), "ok!");
        assert!(!mutex.is_poisoned());

        let lock = RwLock::new(41_u32);
        *lock.write_unpoison() += 1;
        assert_eq!(*lock.read_unpoison(), 42);
        assert!(!lock.is_poisoned());
    }
}
