//! Locked Baton ownership for the experimental Parallel World
//! (`NEOVM_THREADS=parallel`, Cargo feature `shared-heap`).
//!
//! A Parallel World keeps the Green safety argument (`green_owner`): one
//! Baton, held by one OS thread at a time, and only its holder touches the
//! World. What changes is how the Baton moves. Green sends it through the
//! chosen next owner's channel; here it rests in the World's [`BatonLock`]
//! while no owner runs, an owner takes it with [`BatonLock::attach`], and
//! puts it back when it blocks. The mutex's release/acquire pair is the
//! cross-thread edge point 3 of the Green argument names: it orders every
//! write the previous holder made before every access the next holder makes.
//!
//! The lock is the only synchronization this stage adds, and every
//! acquisition of it is counted in [`ParallelStats`].

use super::Baton;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{JoinHandle, ThreadId};
use std::time::Instant;

/// A copy of the counters `neomacs-shared-heap-stats` reports.
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

/// Where the Baton is, under the lock.
struct Slot {
    /// The Baton while no owner holds it.
    baton: Option<Baton>,
    /// The OS thread holding the Baton, while one does.
    holder: Option<ThreadId>,
    /// When the Baton was last put back, for `detached_ns`.
    released_at: Option<Instant>,
    /// Every attach bumps it: a yielding owner waits for it to move.
    attaches: u64,
    /// Owners blocked in `attach`.
    waiting: u64,
    /// Owners in `yield_baton`, waiting for a peer's attach.
    yielders: u64,
}

/// The World's Baton behind a lock: attach to run, detach to block.
pub(crate) struct BatonLock {
    slot: Mutex<Slot>,
    /// Signalled when the Baton is put back.
    released: Condvar,
    /// Signalled when an owner attaches while another yields.
    attached: Condvar,
    /// The dedicated OS thread of every Lisp thread started so far; main
    /// joins them at teardown. Touched by `make-thread` and teardown only.
    os_threads: Mutex<Vec<JoinHandle<()>>>,
    attach_count: AtomicU64,
    attach_wait_ns: AtomicU64,
    detached_ns: AtomicU64,
    threads: AtomicU64,
    safepoint_stops: AtomicU64,
    collections: AtomicU64,
    deferred_bytes: AtomicU64,
}

fn elapsed_ns(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

impl BatonLock {
    /// Wrap the World's issued Baton, free. The main thread is the one live
    /// Lisp thread.
    pub(crate) fn new(baton: Baton) -> Self {
        Self {
            slot: Mutex::new(Slot {
                baton: Some(baton),
                holder: None,
                released_at: None,
                attaches: 0,
                waiting: 0,
                yielders: 0,
            }),
            released: Condvar::new(),
            attached: Condvar::new(),
            os_threads: Mutex::new(Vec::new()),
            attach_count: AtomicU64::new(0),
            attach_wait_ns: AtomicU64::new(0),
            detached_ns: AtomicU64::new(0),
            threads: AtomicU64::new(1),
            safepoint_stops: AtomicU64::new(0),
            collections: AtomicU64::new(0),
            deferred_bytes: AtomicU64::new(0),
        }
    }

    fn lock_slot(&self) -> MutexGuard<'_, Slot> {
        // Every critical section moves a few plain fields and cannot panic
        // half-way, so a poisoned slot is still consistent.
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait until the Baton is free and take it on this OS thread.
    ///
    /// # Panics
    ///
    /// When this OS thread already holds the Baton: attaching twice would
    /// wait for itself forever.
    pub(crate) fn attach(&self) -> Attached<'_> {
        match self.attach_inner(true) {
            Some(attached) => attached,
            None => unreachable!("a blocking attach returns only with the Baton"),
        }
    }

    /// Take the Baton only if it is free now.
    pub(crate) fn try_attach(&self) -> Option<Attached<'_>> {
        self.attach_inner(false)
    }

    fn attach_inner(&self, block: bool) -> Option<Attached<'_>> {
        let started = Instant::now();
        let me = std::thread::current().id();
        let mut slot = self.lock_slot();
        assert_ne!(slot.holder, Some(me), "Baton attach is not re-entrant");
        if slot.baton.is_none() {
            if !block {
                return None;
            }
            slot.waiting += 1;
            while slot.baton.is_none() {
                slot = self
                    .released
                    .wait(slot)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            slot.waiting -= 1;
        }
        let baton = slot.baton.take()?;
        slot.holder = Some(me);
        slot.attaches += 1;
        let released_at = slot.released_at.take();
        let wake_yielders = slot.yielders > 0;
        drop(slot);
        if wake_yielders {
            self.attached.notify_all();
        }
        // The holder bookkeeping the Green argument's debug checks read.
        baton.enter();
        self.attach_count.fetch_add(1, Ordering::Relaxed);
        self.attach_wait_ns
            .fetch_add(elapsed_ns(started), Ordering::Relaxed);
        if let Some(released_at) = released_at {
            self.detached_ns
                .fetch_add(elapsed_ns(released_at), Ordering::Relaxed);
        }
        Some(Attached {
            lock: self,
            baton: Some(baton),
        })
    }

    /// Put back the Baton this OS thread carries (see
    /// [`Attached::into_baton`]), waking one owner waiting to attach.
    pub(crate) fn detach(&self, baton: Baton) {
        let mut slot = self.lock_slot();
        Self::put_back(&mut slot, baton);
        let wake = slot.waiting > 0;
        drop(slot);
        if wake {
            self.released.notify_one();
        }
    }

    fn put_back(slot: &mut Slot, baton: Baton) {
        debug_assert!(slot.baton.is_none(), "the World has one Baton");
        debug_assert_eq!(
            slot.holder,
            Some(std::thread::current().id()),
            "only the holder puts the Baton back"
        );
        slot.holder = None;
        slot.released_at = Some(Instant::now());
        slot.baton = Some(baton);
    }

    /// `thread-yield`: put the Baton back, let another owner attach first,
    /// then attach again and carry it. Another owner is one already waiting
    /// to attach or, with PEER_RUNNABLE, one the scheduler made runnable
    /// and that is on its way to attach. With neither this takes the Baton
    /// straight back.
    pub(crate) fn yield_baton(&self, baton: Baton, peer_runnable: bool) -> Baton {
        let mut slot = self.lock_slot();
        Self::put_back(&mut slot, baton);
        if slot.waiting > 0 || peer_runnable {
            let seen = slot.attaches;
            slot.yielders += 1;
            self.released.notify_one();
            while slot.attaches == seen {
                slot = self
                    .attached
                    .wait(slot)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            slot.yielders -= 1;
        }
        drop(slot);
        self.attach().into_baton()
    }

    /// Whether this OS thread holds the Baton.
    pub(crate) fn is_attached_here(&self) -> bool {
        self.lock_slot().holder == Some(std::thread::current().id())
    }

    /// The counters now.
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

    /// A Lisp thread is about to start on its own OS thread.
    pub(crate) fn thread_started(&self) {
        self.threads.fetch_add(1, Ordering::Relaxed);
    }

    /// A Lisp thread ended (or never started).
    pub(crate) fn thread_finished(&self) {
        self.threads.fetch_sub(1, Ordering::Relaxed);
    }

    /// Keep a started Lisp thread's OS thread for [`Self::join_threads`].
    pub(crate) fn adopt_os_thread(&self, handle: JoinHandle<()>) {
        let mut os_threads = self
            .os_threads
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        os_threads.retain(|handle| !handle.is_finished());
        os_threads.push(handle);
    }

    /// Join every Lisp thread's OS thread. Main calls this at teardown, once
    /// every other Lisp thread has ended; they need the Baton no more.
    pub(crate) fn join_threads(&self) {
        let os_threads = std::mem::take(
            &mut *self
                .os_threads
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for handle in os_threads {
            // A panic that escaped a thread already aborted the process.
            let _ = handle.join();
        }
    }
}

/// A held Baton. Dropping it puts the Baton back.
#[must_use = "dropping the guard detaches at once"]
pub(crate) struct Attached<'lock> {
    lock: &'lock BatonLock,
    baton: Option<Baton>,
}

impl Attached<'_> {
    /// The World authority, while attached.
    pub(crate) fn baton(&self) -> &Baton {
        match &self.baton {
            Some(baton) => baton,
            None => unreachable!("a live guard holds its Baton"),
        }
    }

    /// Keep holding the Baton past this guard: an owner seats it, and gives
    /// it back with [`BatonLock::detach`] or [`BatonLock::yield_baton`].
    pub(crate) fn into_baton(mut self) -> Baton {
        match self.baton.take() {
            Some(baton) => baton,
            None => unreachable!("a live guard holds its Baton"),
        }
    }
}

impl Drop for Attached<'_> {
    fn drop(&mut self) {
        if let Some(baton) = self.baton.take() {
            self.lock.detach(baton);
        }
    }
}
