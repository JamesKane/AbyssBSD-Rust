// SPDX-License-Identifier: BSD-2-Clause
//! Shared/exclusive locks.
//!
//! Many readers or one writer. Writers are preferred: once a writer waits,
//! new readers wait too, so a stream of readers cannot starve it. Waiting
//! threads sleep on wait queues. [`RwLock`] holders must not sleep (the
//! witness checks); [`SxLock`] holders may.
//!
//! Behavior reference: sys/kern/kern_rwlock.c, sys/kern/kern_sx.c.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::ptr;

use sync::SpinLock;

use super::witness::{self, LockClass};
use crate::WaitQueue;

#[derive(Debug)]
struct State {
    readers: u32,
    writer: bool,
    writers_waiting: u32,
}

#[derive(Debug)]
struct RwCore {
    state: SpinLock<State>,
    readers_q: WaitQueue,
    writers_q: WaitQueue,
    class: &'static LockClass,
}

impl RwCore {
    const fn new(class: &'static LockClass) -> Self {
        Self {
            state: SpinLock::new(State {
                readers: 0,
                writer: false,
                writers_waiting: 0,
            }),
            readers_q: WaitQueue::new(),
            writers_q: WaitQueue::new(),
            class,
        }
    }

    fn read(&self) {
        self.readers_q.wait_until(|| {
            let mut s = self.state.lock();
            let ok = !s.writer && s.writers_waiting == 0;
            if ok {
                s.readers += 1;
            }
            ok
        });
        // Recorded once held, so waiting is not "sleeping while holding".
        witness::acquire(self.class, ptr::from_ref(self).addr(), ptr::null());
    }

    fn read_unlock(&self) {
        witness::release(ptr::from_ref(self).addr());
        let wake = {
            let mut s = self.state.lock();
            s.readers -= 1;
            s.readers == 0 && s.writers_waiting > 0
        };
        if wake {
            self.writers_q.wake_one();
        }
    }

    fn write(&self) {
        self.state.lock().writers_waiting += 1;
        self.writers_q.wait_until(|| {
            let mut s = self.state.lock();
            let ok = !s.writer && s.readers == 0;
            if ok {
                s.writer = true;
                s.writers_waiting -= 1;
            }
            ok
        });
        witness::acquire(self.class, ptr::from_ref(self).addr(), ptr::null());
    }

    fn write_unlock(&self) {
        witness::release(ptr::from_ref(self).addr());
        let writers = {
            let mut s = self.state.lock();
            s.writer = false;
            s.writers_waiting > 0
        };
        if writers {
            self.writers_q.wake_one();
        } else {
            self.readers_q.wake_all();
        }
    }
}

macro_rules! shared_exclusive {
    ($(#[$doc:meta])* $name:ident, $sleepable:literal) => {
        $(#[$doc])*
        pub struct $name<T> {
            core: RwCore,
            value: UnsafeCell<T>,
        }

        // SAFETY: the lock gives shared access to readers (needs `Sync`)
        // and exclusive access to one writer (needs `Send`).
        unsafe impl<T: Send + Sync> Sync for $name<T> {}
        // SAFETY: moving the lock moves the value.
        unsafe impl<T: Send> Send for $name<T> {}

        impl<T> core::fmt::Debug for $name<T> {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("class", &self.core.class.name)
                    .finish_non_exhaustive()
            }
        }

        impl<T> $name<T> {
            /// A new unlocked lock of `class`.
            ///
            /// # Panics
            ///
            /// At compile time (in const contexts) if the class's sleep
            /// rule does not match the lock kind.
            pub const fn new(class: &'static LockClass, value: T) -> Self {
                assert!(class.sleepable == $sleepable, "lock class sleep rule mismatch");
                Self {
                    core: RwCore::new(class),
                    value: UnsafeCell::new(value),
                }
            }

            /// Shared access, sleeping while a writer holds or waits.
            pub fn read(&self) -> ReadGuard<'_, T> {
                self.core.read();
                ReadGuard {
                    core: &self.core,
                    value: &self.value,
                    _not_send: PhantomData,
                }
            }

            /// Exclusive access.
            pub fn write(&self) -> WriteGuard<'_, T> {
                self.core.write();
                WriteGuard {
                    core: &self.core,
                    value: &self.value,
                    _not_send: PhantomData,
                }
            }
        }
    };
}

shared_exclusive!(
    /// A reader/writer lock whose holders must not sleep.
    RwLock,
    false
);
shared_exclusive!(
    /// A shared/exclusive lock whose holders may sleep.
    SxLock,
    true
);

/// Shared access to a [`RwLock`] or [`SxLock`].
#[derive(Debug)]
pub struct ReadGuard<'a, T> {
    core: &'a RwCore,
    value: &'a UnsafeCell<T>,
    _not_send: PhantomData<*const ()>,
}

impl<T> Deref for ReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: readers share; no writer holds the lock.
        unsafe { &*self.value.get() }
    }
}

impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        self.core.read_unlock();
    }
}

/// Exclusive access to a [`RwLock`] or [`SxLock`].
#[derive(Debug)]
pub struct WriteGuard<'a, T> {
    core: &'a RwCore,
    value: &'a UnsafeCell<T>,
    _not_send: PhantomData<*const ()>,
}

impl<T> Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the writer holds the lock exclusively.
        unsafe { &*self.value.get() }
    }
}

impl<T> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above.
        unsafe { &mut *self.value.get() }
    }
}

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        self.core.write_unlock();
    }
}
