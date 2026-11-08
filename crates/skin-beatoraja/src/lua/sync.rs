//! The interior mutability of a VM's host context, `Send` and `Sync` so a
//! kept VM can move from the import's job thread to the screen's thread
//! (mlua's `send` feature; docs/stage3.md S7-07).
//!
//! One thread owns a VM at a time, so these locks are never contended:
//! they exist to make the context movable, not shared. The rules that keep
//! them from deadlocking are those `RefCell` already imposed: no guard is
//! held across a call into Lua or a re-entry from Lua into the host, and a
//! field is never borrowed twice at once. A poisoned lock (a panic while a
//! guard was held) is read through: the import or runtime that panicked is
//! stopped by its own path, and nothing here panics again.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// A `Cell` for `Copy` values: get and set, behind a lock.
#[derive(Debug, Default)]
pub(crate) struct SyncCell<T: Copy>(Mutex<T>);

impl<T: Copy> SyncCell<T> {
    pub(crate) const fn new(v: T) -> Self {
        Self(Mutex::new(v))
    }

    pub(crate) fn get(&self) -> T {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn set(&self, v: T) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = v;
    }

    pub(crate) fn replace(&self, v: T) -> T {
        std::mem::replace(
            &mut *self.0.lock().unwrap_or_else(PoisonError::into_inner),
            v,
        )
    }
}

impl<T: Copy + Default> SyncCell<T> {
    pub(crate) fn take(&self) -> T {
        self.replace(T::default())
    }
}

/// A `RefCell`: one guard at a time, behind a lock.
#[derive(Debug, Default)]
pub(crate) struct SyncRefCell<T>(Mutex<T>);

impl<T> SyncRefCell<T> {
    pub(crate) const fn new(v: T) -> Self {
        Self(Mutex::new(v))
    }

    pub(crate) fn borrow(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn borrow_mut(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn replace(&self, v: T) -> T {
        std::mem::replace(&mut *self.borrow_mut(), v)
    }
}

impl<T: Default> SyncRefCell<T> {
    pub(crate) fn take(&self) -> T {
        self.replace(T::default())
    }
}
