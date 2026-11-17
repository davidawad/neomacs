//! Locked Baton ownership for the experimental Parallel World.

use super::Baton;
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread::ThreadId;
use std::time::Instant;

/// A copy of the counters exposed by `neomacs-shared-heap-stats`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ParallelStats {
    pub threads: u64,
    pub attach_count: u64,
    pub attach_wait_ns: u64,
    pub detached_ns: u64,
    pub safepoint_stops: u64,
    pub collections: u64,
    pub deferred_bytes: u64,
}

/// Serializes all World access while keeping the World's Baton in one lock.
pub(crate) struct BatonLock {
    baton: Mutex<Option<Baton>>,
    available: Condvar,
    holder: Mutex<Cell<Option<ThreadId>>>,
    detached_at: Mutex<Option<Instant>>,
    attach_count: AtomicU64,
    attach_wait_ns: AtomicU64,
    detached_ns: AtomicU64,
    threads: AtomicU64,
    safepoint_stops: AtomicU64,
    collections: AtomicU64,
    deferred_bytes: AtomicU64,
}

impl BatonLock {
    /// Wrap the World's issued Baton.
    pub(crate) fn new(baton: Baton) -> Self {
        Self {
            baton: Mutex::new(Some(baton)),
            available: Condvar::new(),
            holder: Mutex::new(Cell::new(None)),
            detached_at: Mutex::new(None),
            attach_count: AtomicU64::new(0),
            attach_wait_ns: AtomicU64::new(0),
            detached_ns: AtomicU64::new(0),
            threads: AtomicU64::new(1),
            safepoint_stops: AtomicU64::new(0),
            collections: AtomicU64::new(0),
            deferred_bytes: AtomicU64::new(0),
        }
    }

    /// Wait until the Baton is free and attach the calling OS thread.
    pub(crate) fn attach(&self) -> Attached<'_> {
        self.attach_inner(false)
            .expect("blocking Baton attach always acquires")
    }

    /// Attach only when the Baton is currently free.
    pub(crate) fn try_attach(&self) -> Option<Attached<'_>> {
        self.attach_inner(true)
    }

    fn attach_inner(&self, try_only: bool) -> Option<Attached<'_>> {
        let started = Instant::now();
        let current = std::thread::current().id();
        assert!(!self.is_attached_here(), "Baton attach is not re-entrant");
        let mut slot = self.baton.lock().expect("Baton lock poisoned");
        while slot.is_none() {
            if try_only {
                return None;
            }
            slot = self
                .available
                .wait(slot)
                .expect("Baton lock poisoned while waiting");
        }
        let baton = slot.take().expect("Baton slot checked above");
        let holder = self.holder.lock().expect("Baton holder lock poisoned");
        assert!(holder.get().is_none(), "free Baton has no holder");
        holder.set(Some(current));
        drop(slot);
        drop(holder);
        baton.enter();
        let wait_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.attach_count.fetch_add(1, Ordering::Relaxed);
        self.attach_wait_ns.fetch_add(wait_ns, Ordering::Relaxed);
        if let Some(detached_at) = self
            .detached_at
            .lock()
            .expect("detached interval lock poisoned")
            .take()
        {
            let ns = u64::try_from(detached_at.elapsed().as_nanos()).unwrap_or(u64::MAX);
            self.detached_ns.fetch_add(ns, Ordering::Relaxed);
        }
        Some(Attached {
            lock: self,
            baton: Some(baton),
        })
    }

    /// Whether this OS thread currently owns the Baton.
    pub(crate) fn is_attached_here(&self) -> bool {
        let holder = self.holder.lock().expect("Baton holder lock poisoned");
        holder.get() == Some(std::thread::current().id())
    }

    /// Snapshot the current counters.
    pub(crate) fn stats(&self) -> ParallelStats {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        ParallelStats {
            threads: load(&self.threads),
            attach_count: load(&self.attach_count),
            attach_wait_ns: load(&self.attach_wait_ns),
            detached_ns: load(&self.detached_ns),
            safepoint_stops: load(&self.safepoint_stops),
            collections: load(&self.collections),
            deferred_bytes: load(&self.deferred_bytes),
        }
    }

    pub(crate) fn thread_started(&self) {
        self.threads.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn thread_finished(&self) {
        self.threads.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A held Baton. Dropping it releases the World to one waiting owner.
pub(crate) struct Attached<'lock> {
    lock: &'lock BatonLock,
    baton: Option<Baton>,
}

impl Attached<'_> {
    /// Access the World authority while this guard is alive.
    pub(crate) fn baton(&self) -> &Baton {
        self.baton.as_ref().expect("attached guard owns its Baton")
    }

    /// Access the mutable World authority while this guard is alive.
    pub(crate) fn baton_mut(&mut self) -> &mut Baton {
        self.baton.as_mut().expect("attached guard owns its Baton")
    }
}

impl Drop for Attached<'_> {
    fn drop(&mut self) {
        let baton = self.baton.take().expect("attached guard drops once");
        let now = Instant::now();
        let mut slot = self.lock.baton.lock().expect("Baton lock poisoned");
        debug_assert!(slot.is_none(), "only one Attached guard exists");
        let holder = self.lock.holder.lock().expect("Baton holder lock poisoned");
        debug_assert_eq!(holder.get(), Some(std::thread::current().id()));
        holder.set(None);
        *self
            .lock
            .detached_at
            .lock()
            .expect("detached interval lock poisoned") = Some(now);
        *slot = Some(baton);
        drop(slot);
        drop(holder);
        self.lock.available.notify_one();
    }
}
