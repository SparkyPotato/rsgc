use std::{time::Instant, ops::{Deref, DerefMut}};

use super::mutex::{Mutex, Condvar, MutexGuard};

pub struct Monitor<T> {
    mutex: Mutex<T>,
    cv: Condvar
}

impl<T> Monitor<T> {
    pub const fn new(val: T) -> Self {
        Self {
            mutex: Mutex::new(val),
            cv: Condvar::new()
        }
    }

    pub fn lock<'a>(&'a self, safepoint: bool) -> MontiroLocker<'a, T> {
        MontiroLocker {
            guard: self.mutex.lock(safepoint),
            cv: &self.cv
        }
    }

    pub fn try_lock<'a>(&'a self, safepoint: bool) -> Option<MontiroLocker<'a, T>> {
        self.mutex.try_lock(safepoint).map(|guard| MontiroLocker {
            guard,
            cv: &self.cv
        })
    }

    pub fn notify_all(&self) -> usize {
        self.cv.notify_all()
    }

    pub fn notify_one(&self) -> bool {
        self.cv.notify_one()
    }
}

pub struct MontiroLocker<'a, T> {
    cv: &'a Condvar,
    guard: MutexGuard<'a, T>,
}

impl<'a, T> MontiroLocker<'a, T> {
    pub fn wait(&mut self) {
        self.cv.wait(&mut self.guard);
    }

    pub fn wait_until(&mut self, timeout: Instant) {
        self.cv.wait_until(&mut self.guard, timeout);
    }

    pub fn notify(&self) -> bool {
        self.cv.notify_one()
    }

    pub fn notify_all(&self) -> usize {
        self.cv.notify_all()
    }

    pub fn into_inner(self) -> MutexGuard<'a, T> {
        self.guard
    }
}

impl<'a, T> Deref for MontiroLocker<'a, T> {
    type Target = MutexGuard<'a, T>;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<'a, T> DerefMut for MontiroLocker<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}